use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::Serialize;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tracing::Instrument;
use uuid::Uuid;
use workflow_engine::error::AppError;
use workflow_engine::executor;
use workflow_engine::hawk::{self, HawkClient, HawkError};
use workflow_engine::persistence::Persistence;
use workflow_engine::registry::{DefaultNodeRegistry, NodeRegistry};
use workflow_engine::roles::{self, Roles};
use workflow_engine::run_queue::{self, RunWorker};
use workflow_engine::schedule::{CronSchedule, ScheduleSpec};
use workflow_engine::scheduler::Scheduler;
use workflow_engine::runs::{self, RunOutcome, StartRun};
use workflow_engine::storage::{self, ExecutionFilter};
use workflow_engine::templates;
use workflow_engine::triggers::{self, RunOrigin, TriggerKind};
use workflow_engine::waits::Waits;

#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    node_registry: Arc<dyn NodeRegistry>,
    waits: Arc<Waits>,
    hawk: Arc<HawkClient>,
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
struct ExecutionResponse {
    id: Uuid,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    status: String,
    context: serde_json::Value,
    started_at: String,
    finished_at: Option<String>,
    trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_id: Option<Uuid>,
    steps: Vec<StepItem>,
    /// What a `waiting` execution is waiting for; omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    wait: Option<WaitItem>,
}

#[derive(Serialize)]
struct WaitItem {
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration: Option<String>,
    wake_at: Option<String>,
    correlation_key: Option<String>,
    since: String,
}

#[derive(serde::Deserialize)]
struct SignalRequest {
    key: String,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Serialize)]
struct SignalResponse {
    key: String,
    /// Executions this signal resumed (they continue in the background).
    resumed: Vec<Uuid>,
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
    trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_id: Option<Uuid>,
}

#[derive(Serialize)]
struct ExecutionListResponse {
    executions: Vec<ExecutionListItem>,
    /// Total executions matching the filters, ignoring limit/offset.
    total: i64,
    /// Page size that was applied (after clamping).
    limit: i64,
    /// Offset that was applied.
    offset: i64,
}

/// Just the webhook trigger input (body + headers) of the latest execution — the
/// only data the "Execute workflow" dialog needs to pre-fill a re-run.
#[derive(Serialize)]
struct LatestInputResponse {
    execution_id: Option<Uuid>,
    started_at: Option<String>,
    body: Option<serde_json::Value>,
    headers: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct StepItem {
    node_id: String,
    status: String,
    output: Option<serde_json::Value>,
    /// Loop iteration ("0", "2.1" for nested loops); omitted outside loops.
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration: Option<String>,
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
    // JSON logs by default; every line carries `trace_id` from the execution span.
    // Set LOG_FORMAT=text (or pretty) for human-readable local logs.
    workflow_engine::logging::init("workflow_engine=info,tower_http=info");

    let env_num = |key: &str, default: u64| {
        std::env::var(key).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(default)
    };
    let roles = Roles::parse(&std::env::var("ENGINE_ROLES").unwrap_or_else(|_| roles::DEFAULT_ROLES.into()))
        .map_err(anyhow::Error::msg)?;
    tracing::info!(roles = %roles.names(), "starting");

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/workflow_engine".into());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(env_num("DATABASE_MAX_CONNECTIONS", 5).max(1) as u32)
        .connect(&database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    // Credentials come from the credential store when a node needs them; none are kept here.
    workflow_engine::credentials::init(pool.clone());

    let node_registry: Arc<dyn NodeRegistry> =
        DefaultNodeRegistry::new_arc(Arc::new(pool.clone()));

    // Resume runs suspended by a Wait node: workers poll for due timers; signals arrive on
    // POST /signals, in the API.
    let waits = Waits::new(pool.clone(), node_registry.clone(), env_num("WAIT_RESUME_CONCURRENCY", 4) as usize);

    if roles.worker {
        waits.spawn_timer_worker(std::time::Duration::from_millis(env_num("WAIT_POLL_INTERVAL_MS", 1000).max(100)));
        RunWorker::new(pool.clone(), node_registry.clone(), env_num("RUN_WORKER_CONCURRENCY", 4) as usize)
            .spawn(std::time::Duration::from_millis(env_num("RUN_QUEUE_IDLE_POLL_MS", 500).max(50)));
    }
    if roles.scheduler {
        Scheduler::new(
            pool.clone(),
            std::time::Duration::from_secs(env_num("SCHEDULE_MISFIRE_GRACE_SECS", 60)),
            std::time::Duration::from_secs(env_num("SCHEDULE_RUN_LEASE_SECS", 3600)),
            env_num("SCHEDULER_BATCH", 100) as usize,
        )
        .spawn(std::time::Duration::from_millis(env_num("SCHEDULER_POLL_INTERVAL_MS", 1000).max(100)));
    }
    if !roles.api {
        // No HTTP server; the background roles above run until the process is stopped.
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

    let state = AppState {
        pool: pool.clone(),
        node_registry,
        waits,
        hawk: Arc::new(HawkClient::new(Some(Arc::new(pool.clone())))),
    };

    let app = Router::new()
        .route("/workflows", post(create_workflow).get(list_workflows))
        .route("/workflows/:id", get(get_workflow).put(update_workflow).delete(delete_workflow))
        .route("/webhook/:id", post(trigger_webhook))
        .route("/globals", get(list_globals).put(set_global))
        .route("/globals/:key", get(get_global).put(set_global_by_key).delete(delete_global))
        .route("/executions", get(list_executions))
        .route("/executions/latest-input", get(latest_execution_input))
        .route("/executions/:id", get(get_execution))
        .route("/executions/:id/step", post(run_next_step_handler))
        .route("/executions/:id/cancel", post(cancel_execution))
        .route("/signals", post(send_signal))
        .route("/schedules", get(list_schedules).post(create_schedule))
        .route("/schedules/preview", post(preview_schedule))
        .route("/schedules/:id", get(get_schedule).put(put_schedule).delete(delete_schedule))
        .route("/schedules/:id/run", post(run_schedule))
        .route("/node-templates", get(list_node_templates).post(create_node_template))
        .route(
            "/node-templates/:slug",
            get(get_node_template).put(publish_node_template).delete(delete_node_template),
        )
        .route("/node-templates/:slug/versions", get(list_node_template_versions))
        .route("/node-templates/:slug/usages", get(list_node_template_usages))
        .route("/browser-automations", get(list_browser_automations))
        .route("/browser-automations/:id", get(get_browser_automation))
        .route("/browser-automations/runs/:run_id", get(get_browser_automation_run))
        .route(
            "/browser-automations/runs/:run_id/artifacts/:name",
            get(get_browser_automation_artifact),
        )
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

    let usages = templates::validate_workflow(&state.pool, &tenant, &definition)
        .await
        .map_err(AppError::BadRequest)?;
    let w = storage::create_workflow(&state.pool, &tenant, &name, version, &definition)
        .await
        .map_err(AppError::from)?;
    storage::replace_template_usages(&state.pool, w.id, &w.tenant, &usages)
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
    // Re-check template references whenever what they resolve against changes.
    let usages = if definition.is_some() || tenant.is_some() {
        let def = definition_ref.unwrap_or(&w.definition);
        let t = tenant.unwrap_or(&w.tenant);
        Some(
            templates::validate_workflow(&state.pool, t, def)
                .await
                .map_err(AppError::BadRequest)?,
        )
    } else {
        None
    };
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
    if let Some(usages) = usages {
        storage::replace_template_usages(&state.pool, updated.id, &updated.tenant, &usages)
            .await
            .map_err(AppError::from)?;
    }
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

async fn delete_workflow(
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
    // Deleting the workflow cascades to its executions and their steps.
    let deleted = storage::delete_workflow(&state.pool, id)
        .await
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::NotFound("workflow not found".into()));
    }
    Ok(Json(serde_json::json!({
        "id": id,
        "deleted": true
    })))
}

#[derive(serde::Deserialize)]
struct WebhookPath {
    id: String,
}

#[derive(serde::Deserialize, Default)]
struct WebhookQuery {
    version: Option<i32>,
    #[serde(rename = "step")]
    step: Option<bool>,
    /// Overrides the workflow's `persistence` for this call: `full`, `errors_only` or `none`.
    persist: Option<String>,
    /// Queue the run for a worker and return at once with its `execution_id`.
    #[serde(rename = "async")]
    queue: Option<bool>,
}

/// Maximum number of executions returnable in a single page.
const EXECUTIONS_MAX_LIMIT: i64 = 500;
/// Default page size when the caller does not specify `limit`.
const EXECUTIONS_DEFAULT_LIMIT: i64 = 100;

#[derive(serde::Deserialize, Default)]
struct ExecutionsQuery {
    workflow_id: Option<Uuid>,
    /// Only executions started within the past N minutes. Absent or 0 = all time.
    /// Matches the builder execution tab's time-range dropdown (5, 10, 15, 30,
    /// 60, 120, 180, 360, 1440).
    last_minutes: Option<i64>,
    /// Page size. Defaults to 100, clamped to [1, 500].
    limit: Option<i64>,
    /// Rows to skip for pagination. Defaults to 0.
    offset: Option<i64>,
    /// Only executions started this way: `webhook`, `schedule` or `workflow_call`.
    trigger: Option<String>,
    /// Only executions started by this schedule.
    schedule_id: Option<Uuid>,
}

async fn trigger_webhook(
    State(state): State<AppState>,
    Path(WebhookPath { id: id_or_name }): Path<WebhookPath>,
    Query(query): Query<WebhookQuery>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<RunOutcome>, AppError> {
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

    let persistence = query
        .persist
        .as_deref()
        .map(Persistence::parse)
        .transpose()
        .map_err(AppError::BadRequest)?;
    let start = StartRun {
        execution_id: None,
        trigger_context: triggers::webhook_context_from_request(body, &headers),
        origin: RunOrigin::WEBHOOK,
        step_mode: query.step == Some(true),
        persistence,
        // Continue the caller's trace (from the incoming `traceparent`), or start a root.
        trace_id: workflow_engine::trace::resolve_trace_id(&headers),
    };
    let outcome = if query.queue == Some(true) {
        run_queue::enqueue(&state.pool, &workflow, start, None).await?
    } else {
        runs::start_run(&state.pool, state.node_registry.clone(), &workflow, start).await?
    };
    Ok(Json(outcome))
}

fn require_tenant(headers: &axum::http::HeaderMap) -> Result<String, AppError> {
    tenant_from_headers(headers)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("tenant is required (X-Tenant-ID header)".into()))
}

fn global_json(g: storage::WorkflowGlobal) -> serde_json::Value {
    serde_json::json!({
        "key": g.key,
        "value": g.value,
        "created_at": g.created_at.to_rfc3339(),
        "updated_at": g.updated_at.to_rfc3339(),
    })
}

async fn list_globals(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let rows = storage::list_globals(&state.pool, &tenant)
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({
        "globals": rows.into_iter().map(global_json).collect::<Vec<_>>(),
    })))
}

async fn get_global(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let g = storage::get_global(&state.pool, &tenant, &key)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("global not found".into()))?;
    Ok(Json(global_json(g)))
}

/// PUT /globals with body `{ "key": "...", "value": <any> }`.
async fn set_global(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant = require_tenant(&headers)?;
    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("key is required".into()))?
        .to_string();
    let value = body
        .get("value")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("value is required".into()))?;
    let g = storage::set_global(&state.pool, &tenant, &key, &value)
        .await
        .map_err(AppError::from)?;
    Ok((StatusCode::OK, Json(global_json(g))))
}

/// PUT /globals/:key — body is either the raw value or `{ "value": <any> }`.
async fn set_global_by_key(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(key): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant = require_tenant(&headers)?;
    // Accept `{ "value": ... }` wrapper, or treat the whole body as the value.
    let value = match body.get("value") {
        Some(v) => v.clone(),
        None => body,
    };
    let g = storage::set_global(&state.pool, &tenant, &key, &value)
        .await
        .map_err(AppError::from)?;
    Ok((StatusCode::OK, Json(global_json(g))))
}

async fn delete_global(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let deleted = storage::delete_global(&state.pool, &tenant, &key)
        .await
        .map_err(AppError::from)?;
    if !deleted {
        return Err(AppError::NotFound("global not found".into()));
    }
    Ok(Json(serde_json::json!({ "key": key, "deleted": true })))
}

async fn list_executions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ExecutionsQuery>,
) -> Result<Json<ExecutionListResponse>, AppError> {
    let tenant_header = tenant_from_headers(&headers);
    let tenant = tenant_header.as_deref();

    let limit = query
        .limit
        .unwrap_or(EXECUTIONS_DEFAULT_LIMIT)
        .clamp(1, EXECUTIONS_MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    // A positive `last_minutes` becomes a lower bound on started_at; 0/absent = all time.
    let since = match query.last_minutes {
        Some(m) if m > 0 => Some(Utc::now() - chrono::Duration::minutes(m)),
        _ => None,
    };

    let trigger = query
        .trigger
        .as_deref()
        .map(TriggerKind::parse)
        .transpose()
        .map_err(AppError::BadRequest)?;
    let filter = ExecutionFilter {
        workflow_id: query.workflow_id,
        tenant,
        since,
        trigger,
        schedule_id: query.schedule_id,
    };

    let rows = storage::list_executions(&state.pool, filter, limit, offset)
        .await
        .map_err(AppError::from)?;
    let total = storage::count_executions(&state.pool, filter)
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
                trigger: e.trigger,
                schedule_id: e.schedule_id,
            })
            .collect(),
        total,
        limit,
        offset,
    }))
}

/// Return only the webhook trigger input (body + headers) of the most recent
/// execution for a workflow. Lets the Execute dialog pre-fill a re-run without
/// pulling every node's output in the full execution context.
async fn latest_execution_input(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ExecutionsQuery>,
) -> Result<Json<LatestInputResponse>, AppError> {
    let tenant_header = tenant_from_headers(&headers);
    let tenant = tenant_header.as_deref();
    let filter = ExecutionFilter { workflow_id: query.workflow_id, tenant, ..Default::default() };
    let rows = storage::list_executions(&state.pool, filter, 1, 0)
        .await
        .map_err(AppError::from)?;
    let Some(exec) = rows.into_iter().next() else {
        return Ok(Json(LatestInputResponse {
            execution_id: None,
            started_at: None,
            body: None,
            headers: None,
        }));
    };
    let webhook = exec.context.get("Webhook");
    Ok(Json(LatestInputResponse {
        execution_id: Some(exec.id),
        started_at: Some(exec.started_at.to_rfc3339()),
        body: webhook.and_then(|w| w.get("body")).cloned(),
        headers: webhook.and_then(|w| w.get("headers")).cloned(),
    }))
}

async fn get_execution(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ExecutionResponse>, AppError> {
    match load_execution(&state, &headers, id).await {
        Ok(exec) => execution_response(&state.pool, exec).await.map(Json),
        // A queued run has no execution row until a worker starts it.
        Err(AppError::NotFound(_)) => queued_execution_response(&state, &headers, id).await.map(Json),
        Err(e) => Err(e),
    }
}

/// A run still in the queue, shown as an execution with status `queued` and no steps.
async fn queued_execution_response(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    id: Uuid,
) -> Result<ExecutionResponse, AppError> {
    let run = load_queued_run(state, headers, id).await?;
    Ok(ExecutionResponse {
        id: run.id,
        workflow_id: run.workflow_id,
        workflow_version: None,
        status: "queued".to_string(),
        context: run.trigger_context,
        started_at: run.enqueued_at.to_rfc3339(),
        finished_at: None,
        trigger: run.trigger,
        schedule_id: run.schedule_id,
        steps: Vec::new(),
        wait: None,
    })
}

/// The queued run, when it exists and belongs to the caller's tenant (if one is given).
async fn load_queued_run(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    id: Uuid,
) -> Result<storage::QueuedRun, AppError> {
    let not_found = || AppError::NotFound("execution not found".into());
    let run = storage::get_queued_run(&state.pool, id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(not_found)?;
    if let Some(ref tenant) = tenant_from_headers(headers) {
        let w = storage::get_workflow_by_id(&state.pool, run.workflow_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(not_found)?;
        if w.tenant != *tenant {
            return Err(not_found());
        }
    }
    Ok(run)
}

/// The execution, when it exists and belongs to the caller's tenant (if one is given).
async fn load_execution(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    id: Uuid,
) -> Result<storage::WorkflowExecution, AppError> {
    let exec = storage::get_execution(&state.pool, id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("execution not found".into()))?;
    if let Some(ref tenant) = tenant_from_headers(headers) {
        let w = storage::get_workflow_by_id(&state.pool, exec.workflow_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound("execution not found".into()))?;
        if w.tenant != *tenant {
            return Err(AppError::NotFound("execution not found".into()));
        }
    }
    Ok(exec)
}

async fn execution_response(
    pool: &sqlx::PgPool,
    exec: storage::WorkflowExecution,
) -> Result<ExecutionResponse, AppError> {
    let steps = storage::list_steps_by_execution(pool, exec.id)
        .await
        .map_err(AppError::from)?;
    let wait = if exec.status == "waiting" {
        storage::get_wait(pool, exec.id).await.map_err(AppError::from)?
    } else {
        None
    };
    Ok(ExecutionResponse {
        id: exec.id,
        workflow_id: exec.workflow_id,
        workflow_version: exec.workflow_version,
        status: exec.status,
        context: exec.context,
        started_at: exec.started_at.to_rfc3339(),
        finished_at: exec.finished_at.map(|t| t.to_rfc3339()),
        trigger: exec.trigger,
        schedule_id: exec.schedule_id,
        steps: steps
            .into_iter()
            .map(|s| StepItem {
                node_id: s.node_id,
                status: s.status,
                output: s.output,
                iteration: Some(s.iteration).filter(|i| !i.is_empty()),
            })
            .collect(),
        wait: wait.map(|w| WaitItem {
            node_id: w.node_id,
            iteration: Some(w.iteration).filter(|i| !i.is_empty()),
            wake_at: w.wake_at.map(|t| t.to_rfc3339()),
            correlation_key: w.correlation_key,
            since: w.created_at.to_rfc3339(),
        }),
    })
}

/// Cancel a queued, waiting or paused execution: it will not start or resume.
async fn cancel_execution(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ExecutionResponse>, AppError> {
    let exec = match load_execution(&state, &headers, id).await {
        Ok(exec) => exec,
        Err(AppError::NotFound(_)) => {
            load_queued_run(&state, &headers, id).await?;
            // It may have been started since; then it is cancelled (or not) as an execution.
            if !storage::cancel_queued_run(&state.pool, id).await.map_err(AppError::from)? {
                return Box::pin(cancel_execution(State(state), headers, Path(id))).await;
            }
            let exec = load_execution(&state, &headers, id).await?;
            return execution_response(&state.pool, exec).await.map(Json);
        }
        Err(e) => return Err(e),
    };
    if !storage::cancel_execution(&state.pool, id).await.map_err(AppError::from)? {
        return Err(AppError::Conflict(format!(
            "execution is {}; only queued, waiting or paused executions can be cancelled",
            exec.status
        )));
    }
    let exec = load_execution(&state, &headers, id).await?;
    execution_response(&state.pool, exec).await.map(Json)
}

/// Deliver a signal to the tenant's runs waiting on its key (see the Wait node).
async fn send_signal(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SignalRequest>,
) -> Result<Json<SignalResponse>, AppError> {
    let tenant = require_tenant(&headers)?;
    let key = body.key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::BadRequest("key is required".into()));
    }
    let resumed = state
        .waits
        .signal(&tenant, &key, body.payload)
        .await
        .map_err(AppError::from)?;
    Ok(Json(SignalResponse { key, resumed }))
}

async fn run_next_step_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ExecutionResponse>, AppError> {
    let exec = load_execution(&state, &headers, id).await?;
    if exec.status != "paused" {
        return Err(AppError::BadRequest(
            "Execution is not in paused state.".into(),
        ));
    }
    let workflow = storage::get_workflow_by_id(&state.pool, exec.workflow_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("workflow not found".into()))?;

    // Resolve templates to the versions this run started with, even if newer ones exist.
    let resolved = templates::resolve_definition(
        &state.pool,
        &workflow.tenant,
        &workflow.definition,
        exec.context.get(templates::LOCK_CONTEXT_KEY),
    )
    .await
    .map_err(AppError::BadRequest)?;

    let trace_id = workflow_engine::trace::resolve_trace_id(&headers);
    let span = tracing::info_span!("execution", trace_id = %trace_id, execution_id = %id);
    let _ = executor::run_next_step(
        &state.pool,
        state.node_registry.clone(),
        exec,
        &resolved.definition,
        Some(trace_id.clone()),
        None,
    )
    .instrument(span)
    .await;

    let exec = load_execution(&state, &headers, id).await?;
    execution_response(&state.pool, exec).await.map(Json)
}

#[derive(serde::Deserialize, Default)]
struct NodeTemplateQuery {
    version: Option<i32>,
    /// List every version instead of only the latest of each template.
    all_versions: Option<bool>,
    /// Publish even if nodes following the latest version would lack a new required param.
    force: Option<bool>,
}

fn node_template_json(t: &storage::NodeTemplate) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "slug": t.slug,
        "version": t.version,
        "is_latest": t.is_latest,
        "name": t.name,
        "description": t.description,
        "node_type": t.node_type,
        "config": t.config,
        "params": t.params,
        "created_at": t.created_at.to_rfc3339(),
    })
}

/// Read a template's editable content from a request body. `type`/`data` are accepted as
/// aliases of `node_type`/`config`, so a React Flow node's fields can be sent as they are.
fn node_template_content(body: &serde_json::Value) -> Result<storage::NodeTemplateContent, AppError> {
    let text = |key: &str| {
        body.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let node_type = text("node_type")
        .or_else(|| text("type"))
        .ok_or_else(|| AppError::BadRequest("node_type is required".into()))?;
    let config = body
        .get("config")
        .or_else(|| body.get("data"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(storage::NodeTemplateContent {
        name: text("name"),
        description: text("description"),
        node_type,
        config,
        params: body.get("params").cloned().unwrap_or_else(|| serde_json::json!({})),
    })
}

async fn list_node_templates(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<NodeTemplateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let rows = storage::list_node_templates(&state.pool, &tenant, query.all_versions == Some(true))
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({
        "templates": rows.iter().map(node_template_json).collect::<Vec<_>>(),
    })))
}

async fn get_node_template(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<NodeTemplateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let t = storage::get_node_template(&state.pool, &tenant, &slug, query.version)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("node template not found".into()))?;
    Ok(Json(node_template_json(&t)))
}

async fn list_node_template_versions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let rows = storage::list_node_templates(&state.pool, &tenant, true)
        .await
        .map_err(AppError::from)?;
    let versions: Vec<_> = rows.iter().filter(|t| t.slug == slug).map(node_template_json).collect();
    if versions.is_empty() {
        return Err(AppError::NotFound("node template not found".into()));
    }
    Ok(Json(serde_json::json!({ "slug": slug, "versions": versions })))
}

/// POST /node-templates — body `{ "slug", "node_type", "config", "params"?, "name"?, "description"? }`.
async fn create_node_template(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<NodeTemplateQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("slug is required".into()))?
        .to_string();
    publish(&state, &headers, &slug, &body, query.force == Some(true)).await
}

/// PUT /node-templates/:slug — publish the body as the next version (a no-op returning the
/// latest version when nothing changed).
async fn publish_node_template(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<NodeTemplateQuery>,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    publish(&state, &headers, &slug, &body, query.force == Some(true)).await
}

async fn publish(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    slug: &str,
    body: &serde_json::Value,
    force: bool,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant = require_tenant(headers)?;
    let mut content = node_template_content(body)?;
    templates::validate_content(slug, &mut content).map_err(AppError::BadRequest)?;
    if !force {
        let usages = storage::list_template_usages(&state.pool, &tenant, slug)
            .await
            .map_err(AppError::from)?;
        let broken = templates::breaking_usages(slug, &content.params, &usages);
        if !broken.is_empty() {
            return Err(AppError::Conflict(format!(
                "nodes following the latest version would break (pin them, set the param, or retry with ?force=true): {}",
                broken.join("; ")
            )));
        }
    }
    let (t, created) = storage::publish_node_template(&state.pool, &tenant, slug, &content)
        .await
        .map_err(AppError::from)?;
    let mut json = node_template_json(&t);
    json["created"] = serde_json::Value::Bool(created);
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(json)))
}

async fn list_node_template_usages(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let usages = storage::list_template_usages(&state.pool, &tenant, &slug)
        .await
        .map_err(AppError::from)?;
    let floating = usages.iter().filter(|u| u.version.is_none()).count();
    Ok(Json(serde_json::json!({
        "slug": slug,
        "usages": usages,
        "floating": floating,
        "pinned": usages.len() - floating,
    })))
}

/// DELETE /node-templates/:slug[?version=N]. Refused (409) while workflow nodes still use
/// what would be removed: any node for the whole template; for one version, nodes pinned to
/// it, or — when it is the latest — nodes following latest.
async fn delete_node_template(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<NodeTemplateQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let target = storage::get_node_template(&state.pool, &tenant, &slug, query.version)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("node template not found".into()))?;
    let usages = storage::list_template_usages(&state.pool, &tenant, &slug)
        .await
        .map_err(AppError::from)?;
    let blocking: Vec<String> = usages
        .iter()
        .filter(|u| match query.version {
            None => true,
            Some(v) => u.version == Some(v) || (u.version.is_none() && target.is_latest),
        })
        .map(|u| format!("{} v{} / {}", u.workflow_name, u.workflow_version, u.node_id))
        .collect();
    if !blocking.is_empty() {
        return Err(AppError::Conflict(format!("node template is in use by: {}", blocking.join(", "))));
    }
    let deleted = storage::delete_node_template(&state.pool, &tenant, &slug, query.version)
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({
        "slug": slug,
        "version": query.version,
        "deleted": deleted,
    })))
}

// ── Browser automations (hawk) ────────────────────────────────────────────────

fn hawk_error(e: HawkError) -> AppError {
    match e {
        HawkError::Status(404, m) => AppError::NotFound(m),
        HawkError::Status(s, m) if (400..500).contains(&s) => AppError::BadRequest(m),
        e => AppError::BadGateway(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct BrowserAutomationQuery {
    system: Option<String>,
    process: Option<String>,
}

/// GET /browser-automations: the automations a BrowserAutomation node can run (hawk's
/// published steps), with the `params` each expects. Filter with `?system=` / `?process=`.
async fn list_browser_automations(
    State(state): State<AppState>,
    Query(q): Query<BrowserAutomationQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let matches = |field: &serde_json::Value, want: &Option<String>| match want {
        None => true,
        Some(w) => field.as_str().is_some_and(|f| f.eq_ignore_ascii_case(w.trim())),
    };
    let automations: Vec<_> = state
        .hawk
        .catalog()
        .await
        .map_err(hawk_error)?
        .iter()
        .map(hawk::describe)
        .filter(|a| matches(&a["system"], &q.system) && matches(&a["process"], &q.process))
        .collect();
    Ok(Json(serde_json::json!({ "automations": automations })))
}

/// GET /browser-automations/:id: one automation from the catalog.
async fn get_browser_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let catalog = state.hawk.catalog().await.map_err(hawk_error)?;
    let step = catalog
        .iter()
        .find(|s| s["id"] == id.as_str())
        .ok_or_else(|| AppError::NotFound(format!("no published browser automation \"{id}\"")))?;
    Ok(Json(hawk::describe(step)))
}

/// GET /browser-automations/runs/:run_id: a hawk run as hawk reports it (status, output,
/// error, artifacts), so callers need no direct access to hawk.
async fn get_browser_automation_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(state.hawk.get_run(&run_id).await.map_err(hawk_error)?))
}

/// GET /browser-automations/runs/:run_id/artifacts/:name: a file a run produced.
async fn get_browser_automation_artifact(
    State(state): State<AppState>,
    Path((run_id, name)): Path<(String, String)>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let file = state.hawk.artifact(&run_id, &name).await.map_err(hawk_error)?;
    let content_type = file.content_type.unwrap_or_else(|| "application/octet-stream".into());
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], file.bytes).into_response())
}

/// A schedule as the API shows it: its spec (the same fields `PUT /schedules/:name` takes,
/// so a listing can be written back to a config file), then its state.
fn schedule_json(s: &storage::Schedule, upcoming: Option<usize>) -> serde_json::Value {
    let cron = CronSchedule::parse(&s.cron, &s.timezone).ok();
    let local = |t: Option<chrono::DateTime<Utc>>| match (t, &cron) {
        (Some(t), Some(c)) => serde_json::json!(t.with_timezone(&c.timezone()).fixed_offset().to_rfc3339()),
        _ => serde_json::Value::Null,
    };
    let mut json = serde_json::to_value(s.spec()).unwrap_or_default();
    let extra = serde_json::json!({
        "id": s.id,
        "description": cron.as_ref().map(|c| c.describe()),
        "next_run_at": s.next_run_at.map(|t| t.to_rfc3339()),
        "next_run_local": local(s.next_run_at),
        "last_run_at": s.last_run_at.map(|t| t.to_rfc3339()),
        "last_scheduled_for": local(s.last_scheduled_for),
        "last_outcome": s.last_outcome,
        "last_error": s.last_error,
        "last_execution_id": s.last_execution_id,
        "active_execution_id": s.active_execution_id,
        "created_at": s.created_at.to_rfc3339(),
        "updated_at": s.updated_at.to_rfc3339(),
    });
    if let (Some(obj), serde_json::Value::Object(extra)) = (json.as_object_mut(), extra) {
        obj.extend(extra);
        if let Some(n) = upcoming {
            let times = match (&cron, s.enabled) {
                (Some(c), true) => c.upcoming(Utc::now(), n),
                _ => Vec::new(),
            };
            obj.insert("upcoming".into(), serde_json::to_value(times).unwrap_or_default());
        }
    }
    json
}

/// The tenant's schedule by UUID or name.
async fn find_schedule(state: &AppState, tenant: &str, id_or_name: &str) -> Result<Option<storage::Schedule>, AppError> {
    let found = match Uuid::parse_str(id_or_name) {
        Ok(id) => storage::get_schedule_by_id(&state.pool, id)
            .await
            .map_err(AppError::from)?
            .filter(|s| s.tenant == tenant),
        Err(_) => None,
    };
    match found {
        Some(s) => Ok(Some(s)),
        None => storage::get_schedule_by_name(&state.pool, tenant, id_or_name)
            .await
            .map_err(AppError::from),
    }
}

async fn load_schedule(state: &AppState, tenant: &str, id_or_name: &str) -> Result<storage::Schedule, AppError> {
    find_schedule(state, tenant, id_or_name)
        .await?
        .ok_or_else(|| AppError::NotFound("schedule not found".into()))
}

/// Validate `spec` (including that its workflow exists) and save it, creating or replacing
/// the schedule of that name.
async fn save_schedule(
    state: &AppState,
    tenant: &str,
    mut spec: ScheduleSpec,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let cron = spec.validate().map_err(AppError::BadRequest)?;
    let workflow = storage::get_workflow_by_name(&state.pool, &spec.workflow, Some(tenant), spec.version)
        .await
        .map_err(AppError::from)?;
    if workflow.is_none() {
        return Err(AppError::BadRequest(match spec.version {
            Some(v) => format!("workflow '{}' version {v} not found", spec.workflow),
            None => format!("workflow '{}' not found", spec.workflow),
        }));
    }
    let next_run_at = cron.next_after(Utc::now()).map(|o| o.at);
    let (schedule, created) = storage::upsert_schedule(&state.pool, tenant, &spec, next_run_at)
        .await
        .map_err(AppError::from)?;
    let mut json = schedule_json(&schedule, None);
    json["created"] = serde_json::Value::Bool(created);
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(json)))
}

#[derive(serde::Deserialize, Default)]
struct SchedulesQuery {
    /// Only schedules of this workflow (by name).
    workflow: Option<String>,
}

async fn list_schedules(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<SchedulesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let rows = storage::list_schedules(&state.pool, &tenant, query.workflow.as_deref())
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({
        "schedules": rows.iter().map(|s| schedule_json(s, None)).collect::<Vec<_>>(),
    })))
}

/// Create a schedule; 409 if the tenant already has one of that name (use PUT to replace).
async fn create_schedule(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(spec): Json<ScheduleSpec>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant = require_tenant(&headers)?;
    if find_schedule(&state, &tenant, spec.name.trim()).await?.is_some() {
        return Err(AppError::Conflict(format!("schedule '{}' already exists", spec.name.trim())));
    }
    save_schedule(&state, &tenant, spec).await
}

const SCHEDULE_UPCOMING: usize = 5;

async fn get_schedule(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let s = load_schedule(&state, &tenant, &id).await?;
    Ok(Json(schedule_json(&s, Some(SCHEDULE_UPCOMING))))
}

/// Create or replace a schedule by name (or replace one by id). This is what config sync
/// calls for each schedule in a `<env>.workflow.schedules` file. The body's `name` may be left
/// out; if given it must match. Saving an unchanged schedule does not move its next run.
async fn put_schedule(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(mut body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant = require_tenant(&headers)?;
    let name = match find_schedule(&state, &tenant, &id).await? {
        Some(existing) => existing.name,
        None => id.trim().to_string(),
    };
    let obj = body
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("body must be a JSON object".into()))?;
    match obj.get("name").and_then(|v| v.as_str()).map(str::trim) {
        Some(n) if n != name => {
            return Err(AppError::BadRequest(format!(
                "body name '{n}' does not match '{name}'; schedules cannot be renamed (create the new name and delete the old)"
            )))
        }
        _ => {
            obj.insert("name".into(), serde_json::Value::String(name));
        }
    }
    let spec: ScheduleSpec = serde_json::from_value(body).map_err(|e| AppError::BadRequest(e.to_string()))?;
    save_schedule(&state, &tenant, spec).await
}

async fn delete_schedule(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant = require_tenant(&headers)?;
    let s = load_schedule(&state, &tenant, &id).await?;
    if !storage::delete_schedule(&state.pool, &tenant, &s.name).await.map_err(AppError::from)? {
        return Err(AppError::NotFound("schedule not found".into()));
    }
    Ok(Json(serde_json::json!({ "id": s.id, "name": s.name, "deleted": true })))
}

#[derive(serde::Deserialize, Default)]
struct RunScheduleQuery {
    /// Queue the run for a worker and return at once.
    #[serde(rename = "async")]
    queue: Option<bool>,
}

/// Run a schedule now, with its input, as a webhook call would (`?async=true` queues it).
/// `Trigger.scheduled_for` is null. Its next slot, and the overlap rule, are not affected.
async fn run_schedule(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<RunScheduleQuery>,
) -> Result<Json<RunOutcome>, AppError> {
    let tenant = require_tenant(&headers)?;
    let s = load_schedule(&state, &tenant, &id).await?;
    let workflow = storage::get_workflow_by_name(&state.pool, &s.workflow_name, Some(&tenant), s.workflow_version)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::BadRequest(format!("workflow '{}' not found", s.workflow_name)))?;
    let persistence = s
        .persistence
        .as_deref()
        .map(Persistence::parse)
        .transpose()
        .map_err(AppError::BadRequest)?;
    let start = StartRun {
        execution_id: None,
        trigger_context: triggers::schedule_context(s.input.clone(), s.id, &s.name, None),
        origin: RunOrigin::schedule(s.id),
        step_mode: false,
        persistence,
        trace_id: workflow_engine::trace::resolve_trace_id(&headers),
    };
    let outcome = if query.queue == Some(true) {
        run_queue::enqueue(&state.pool, &workflow, start, None).await?
    } else {
        runs::start_run(&state.pool, state.node_registry.clone(), &workflow, start).await?
    };
    Ok(Json(outcome))
}

#[derive(serde::Deserialize)]
struct PreviewRequest {
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
    /// How many upcoming times to return (default 5, at most 50).
    #[serde(default)]
    count: Option<usize>,
}

/// Check a cron expression and timezone, and show when they would fire. For editors: the
/// times come from the same code the scheduler uses.
async fn preview_schedule(Json(body): Json<PreviewRequest>) -> Result<Json<serde_json::Value>, AppError> {
    let timezone = body.timezone.as_deref().unwrap_or("UTC").trim().to_string();
    let cron = CronSchedule::parse(&body.cron, &timezone).map_err(AppError::BadRequest)?;
    let count = body.count.unwrap_or(SCHEDULE_UPCOMING).clamp(1, 50);
    Ok(Json(serde_json::json!({
        "cron": body.cron.trim(),
        "timezone": timezone,
        "description": cron.describe(),
        "upcoming": cron.upcoming(Utc::now(), count),
    })))
}
