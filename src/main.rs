use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;
use workflow_engine::error::AppError;
use workflow_engine::executor;
use workflow_engine::registry::{DefaultNodeRegistry, NodeRegistry};
use workflow_engine::storage;
use workflow_engine::triggers;

#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    node_registry: Arc<dyn NodeRegistry>,
}

#[derive(Serialize)]
struct CreateWorkflowResponse {
    id: Uuid,
    name: String,
    version: i32,
    is_latest: bool,
    created_at: String,
}

#[derive(Serialize)]
struct WorkflowListResponse {
    workflows: Vec<WorkflowListItem>,
}

#[derive(Serialize)]
struct WorkflowListItem {
    id: Uuid,
    name: String,
    version: i32,
    is_latest: bool,
    created_at: String,
}

#[derive(Serialize)]
struct WebhookResponse {
    execution_id: Uuid,
    status: String,
}

#[derive(Serialize)]
struct ExecutionResponse {
    id: Uuid,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    status: String,
    context: serde_json::Value,
    started_at: String,
    finished_at: Option<String>,
    steps: Vec<StepItem>,
}

#[derive(Serialize)]
struct ExecutionListItem {
    id: Uuid,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    status: String,
    context: serde_json::Value,
    started_at: String,
    finished_at: Option<String>,
}

#[derive(Serialize)]
struct ExecutionListResponse {
    executions: Vec<ExecutionListItem>,
}

#[derive(Serialize)]
struct StepItem {
    node_id: String,
    status: String,
    output: Option<serde_json::Value>,
}

const TENANT_HEADER: &str = "x-tenant-id";

fn tenant_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// Parse version from JSON value (number or numeric string). Returns None if missing/invalid.
fn parse_version(v: Option<&serde_json::Value>) -> Option<i32> {
    let v = v?;
    v.as_i64().and_then(|n| i32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "workflow_engine=info,tower_http=info".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/workflow_engine".into());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    let node_registry: Arc<dyn NodeRegistry> =
        Arc::new(DefaultNodeRegistry::new(Some(Arc::new(pool.clone()))));

    let state = AppState {
        pool: pool.clone(),
        node_registry,
    };

    let app = Router::new()
        .route("/workflows", post(create_workflow).get(list_workflows))
        .route("/workflows/:id", get(get_workflow).put(update_workflow))
        .route("/webhook/:id", post(trigger_webhook))
        .route("/executions", get(list_executions))
        .route("/executions/:id", get(get_execution))
        .layer(TimeoutLayer::new(std::time::Duration::from_secs(300)))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on {}", addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn create_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<CreateWorkflowResponse>), AppError> {
    let tenant = body
        .get("tenant")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| tenant_from_headers(&headers))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("tenant is required (provide in body or X-Tenant-ID header)".into()))?;
    let (name, version, definition) = if body.get("definition").is_some() {
        let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
        let definition = body
            .get("definition")
            .cloned()
            .ok_or_else(|| AppError::BadRequest("definition required".into()))?;
        let version = parse_version(body.get("version").or(definition.get("version"))).unwrap_or(1);
        (name, version, definition)
    } else {
        let name = body
            .get("name")
            .or_else(|| body.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unnamed")
            .to_string();
        let version = parse_version(body.get("version")).unwrap_or(1);
        (name.clone(), version, body)
    };

    if definition.get("data").and_then(|d| d.get("nodes")).is_none()
        && definition.get("nodes").is_none()
    {
        return Err(AppError::BadRequest(
            "workflow must have data.nodes or nodes".into(),
        ));
    }

    let w = storage::create_workflow(&state.pool, &tenant, &name, version, &definition)
        .await
        .map_err(AppError::from)?;
    Ok((
        StatusCode::CREATED,
        Json(CreateWorkflowResponse {
            id: w.id,
            name: w.name,
            version: w.version,
            is_latest: w.is_latest,
            created_at: w.created_at.to_rfc3339(),
        }),
    ))
}

async fn list_workflows(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<WorkflowListResponse>, AppError> {
    let tenant_header = tenant_from_headers(&headers);
    let tenant = tenant_header.as_deref();
    let limit = 100_i64;
    let offset = 0_i64;
    let rows = storage::list_workflows(&state.pool, tenant, limit, offset)
        .await
        .map_err(AppError::from)?;
    Ok(Json(WorkflowListResponse {
        workflows: rows
            .into_iter()
            .map(|w| WorkflowListItem {
                id: w.id,
                name: w.name,
                version: w.version,
                is_latest: w.is_latest,
                created_at: w.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

async fn get_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let w = storage::get_workflow_by_id(&state.pool, id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("workflow not found".into()))?;
    if let Some(ref tenant) = tenant_from_headers(&headers) {
        if w.tenant != *tenant {
            return Err(AppError::NotFound("workflow not found".into()));
        }
    }
    Ok(Json(serde_json::json!({
        "id": w.id,
        "tenant": w.tenant,
        "name": w.name,
        "version": w.version,
        "is_latest": w.is_latest,
        "definition": w.definition,
        "created_at": w.created_at.to_rfc3339(),
        "updated_at": w.updated_at.to_rfc3339()
    })))
}

async fn update_workflow(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    let w = storage::get_workflow_by_id(&state.pool, id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("workflow not found".into()))?;
    if let Some(ref tenant) = tenant_from_headers(&headers) {
        if w.tenant != *tenant {
            return Err(AppError::NotFound("workflow not found".into()));
        }
    }
    let tenant = body.get("tenant").and_then(|v| v.as_str());
    let definition = body.get("definition").cloned();
    let is_latest = body.get("is_latest").and_then(|v| v.as_bool());
    let definition_ref = definition.as_ref();
    let updated = storage::update_workflow(
        &state.pool,
        id,
        tenant,
        definition_ref,
        is_latest,
    )
    .await
    .map_err(AppError::from)?
    .ok_or_else(|| AppError::NotFound("workflow not found".into()))?;
    Ok(Json(serde_json::json!({
        "id": updated.id,
        "tenant": updated.tenant,
        "name": updated.name,
        "version": updated.version,
        "is_latest": updated.is_latest,
        "definition": updated.definition,
        "created_at": updated.created_at.to_rfc3339(),
        "updated_at": updated.updated_at.to_rfc3339()
    })))
}

#[derive(serde::Deserialize)]
struct WebhookPath {
    id: String,
}

#[derive(serde::Deserialize, Default)]
struct WebhookQuery {
    version: Option<i32>,
}

#[derive(serde::Deserialize, Default)]
struct ExecutionsQuery {
    workflow_id: Option<Uuid>,
}

async fn trigger_webhook(
    State(state): State<AppState>,
    Path(WebhookPath { id: id_or_name }): Path<WebhookPath>,
    Query(query): Query<WebhookQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<WebhookResponse>, AppError> {
    let tenant_header = tenant_from_headers(&headers);
    let tenant = tenant_header.as_deref();
    let version = query.version;
    let workflow = if let Ok(uuid) = Uuid::parse_str(&id_or_name) {
        storage::get_workflow_by_id(&state.pool, uuid)
            .await
            .map_err(AppError::from)?
    } else {
        storage::get_workflow_by_name(&state.pool, &id_or_name, tenant, version)
            .await
            .map_err(AppError::from)?
    }
    .ok_or_else(|| AppError::NotFound("workflow not found".into()))?;
    if let Some(t) = tenant {
        if workflow.tenant != t {
            return Err(AppError::NotFound("workflow not found".into()));
        }
    }

    let initial_context = triggers::webhook_context_from_request(body, &headers);
    let exec = storage::create_execution(
        &state.pool,
        workflow.id,
        Some(workflow.version),
        &initial_context,
    )
    .await
    .map_err(AppError::from)?;

    let result = executor::run_workflow(
        &state.pool,
        state.node_registry.clone(),
        workflow.id,
        exec.id,
        &workflow.definition,
        initial_context,
    )
    .await;

    let status = match &result {
        Ok(_) => "completed",
        Err(_) => "failed",
    };

    Ok(Json(WebhookResponse {
        execution_id: exec.id,
        status: status.to_string(),
    }))
}

async fn list_executions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ExecutionsQuery>,
) -> Result<Json<ExecutionListResponse>, AppError> {
    let tenant_header = tenant_from_headers(&headers);
    let tenant = tenant_header.as_deref();
    let limit = 100_i64;
    let offset = 0_i64;
    let rows = storage::list_executions(
        &state.pool,
        query.workflow_id,
        tenant,
        limit,
        offset,
    )
    .await
    .map_err(AppError::from)?;
    Ok(Json(ExecutionListResponse {
        executions: rows
            .into_iter()
            .map(|e| ExecutionListItem {
                id: e.id,
                workflow_id: e.workflow_id,
                workflow_version: e.workflow_version,
                status: e.status,
                context: e.context,
                started_at: e.started_at.to_rfc3339(),
                finished_at: e.finished_at.map(|t| t.to_rfc3339()),
            })
            .collect(),
    }))
}

async fn get_execution(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ExecutionResponse>, AppError> {
    let exec = storage::get_execution(&state.pool, id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("execution not found".into()))?;
    if let Some(ref tenant) = tenant_from_headers(&headers) {
        let w = storage::get_workflow_by_id(&state.pool, exec.workflow_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound("execution not found".into()))?;
        if w.tenant != *tenant {
            return Err(AppError::NotFound("execution not found".into()));
        }
    }

    let steps = storage::list_steps_by_execution(&state.pool, id)
        .await
        .map_err(AppError::from)?;

    Ok(Json(ExecutionResponse {
        id: exec.id,
        workflow_id: exec.workflow_id,
        workflow_version: exec.workflow_version,
        status: exec.status,
        context: exec.context,
        started_at: exec.started_at.to_rfc3339(),
        finished_at: exec.finished_at.map(|t| t.to_rfc3339()),
        steps: steps
            .into_iter()
            .map(|s| StepItem {
                node_id: s.node_id,
                status: s.status,
                output: s.output,
            })
            .collect(),
    }))
}
