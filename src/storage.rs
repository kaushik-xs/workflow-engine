use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ServiceRegistryRow {
    pub id: Uuid,
    pub slug: String,
    pub name: Option<String>,
    pub base_url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Workflow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub version: i32,
    pub is_latest: bool,
    pub definition: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WorkflowExecution {
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub workflow_version: Option<i32>,
    pub status: String,
    pub context: serde_json::Value,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WorkflowStep {
    pub id: Uuid,
    pub execution_id: Uuid,
    pub node_id: String,
    pub status: String,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub async fn create_workflow(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    name: &str,
    version: i32,
    definition: &serde_json::Value,
) -> Result<Workflow, sqlx::Error> {
    let row = sqlx::query_as::<_, Workflow>(
        r#"
        INSERT INTO workflows (tenant_id, name, version, definition, is_latest)
        VALUES ($1, $2, $3, $4, true)
        RETURNING id, tenant_id, name, version, is_latest, definition, created_at, updated_at
        "#,
    )
    .bind(tenant_id)
    .bind(name)
    .bind(version)
    .bind(definition)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"UPDATE workflows SET is_latest = false WHERE tenant_id = $1 AND name = $2 AND id != $3"#,
    )
    .bind(tenant_id)
    .bind(name)
    .bind(row.id)
    .execute(pool)
    .await?;
    Ok(row)
}

pub async fn update_workflow(
    pool: &sqlx::PgPool,
    id: Uuid,
    definition: Option<&serde_json::Value>,
    is_latest: Option<bool>,
) -> Result<Option<Workflow>, sqlx::Error> {
    let existing = match get_workflow_by_id(pool, id).await? {
        Some(w) => w,
        None => return Ok(None),
    };
    if let Some(def) = definition {
        sqlx::query(
            r#"UPDATE workflows SET definition = $1, updated_at = now() WHERE id = $2"#,
        )
        .bind(def)
        .bind(id)
        .execute(pool)
        .await?;
    }
    if is_latest == Some(true) {
        sqlx::query(
            r#"UPDATE workflows SET is_latest = false WHERE tenant_id = $1 AND name = $2"#,
        )
        .bind(existing.tenant_id)
        .bind(&existing.name)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"UPDATE workflows SET is_latest = true, updated_at = now() WHERE id = $1"#,
        )
        .bind(id)
        .execute(pool)
        .await?;
    }
    get_workflow_by_id(pool, id).await
}

pub async fn get_workflow_by_id(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<Workflow>, sqlx::Error> {
    let row = sqlx::query_as::<_, Workflow>(
        r#"
        SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
        FROM workflows WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_workflow_by_name(
    pool: &sqlx::PgPool,
    name: &str,
    tenant_id: Option<Uuid>,
    version: Option<i32>,
) -> Result<Option<Workflow>, sqlx::Error> {
    let row = if let Some(v) = version {
        if let Some(tid) = tenant_id {
            sqlx::query_as::<_, Workflow>(
                r#"
                SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
                FROM workflows WHERE name = $1 AND tenant_id = $2 AND version = $3
                LIMIT 1
                "#,
            )
            .bind(name)
            .bind(tid)
            .bind(v)
            .fetch_optional(pool)
            .await?
        } else {
            sqlx::query_as::<_, Workflow>(
                r#"
                SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
                FROM workflows WHERE name = $1 AND version = $2
                LIMIT 1
                "#,
            )
            .bind(name)
            .bind(v)
            .fetch_optional(pool)
            .await?
        }
    } else if let Some(tid) = tenant_id {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
            FROM workflows WHERE name = $1 AND tenant_id = $2
            ORDER BY is_latest DESC, updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .bind(tid)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
            FROM workflows WHERE name = $1
            ORDER BY is_latest DESC, updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await?
    };
    Ok(row)
}

pub async fn list_workflows(
    pool: &sqlx::PgPool,
    tenant_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Workflow>, sqlx::Error> {
    let rows = if let Some(tid) = tenant_id {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
            FROM workflows WHERE tenant_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(tid)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant_id, name, version, is_latest, definition, created_at, updated_at
            FROM workflows
            ORDER BY created_at DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

pub async fn create_execution(
    pool: &sqlx::PgPool,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    context: &serde_json::Value,
) -> Result<WorkflowExecution, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowExecution>(
        r#"
        INSERT INTO workflow_executions (workflow_id, workflow_version, status, context)
        VALUES ($1, $2, 'running', $3)
        RETURNING id, workflow_id, workflow_version, status, context, started_at, finished_at
        "#,
    )
    .bind(workflow_id)
    .bind(workflow_version)
    .bind(context)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update_execution(
    pool: &sqlx::PgPool,
    id: Uuid,
    status: &str,
    context: &serde_json::Value,
    finished_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE workflow_executions
        SET status = $1, context = $2, finished_at = $3
        WHERE id = $4
        "#,
    )
    .bind(status)
    .bind(context)
    .bind(finished_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_execution(
    pool: &sqlx::PgPool,
    id: Uuid,
) -> Result<Option<WorkflowExecution>, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowExecution>(
        r#"
        SELECT id, workflow_id, workflow_version, status, context, started_at, finished_at
        FROM workflow_executions WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn insert_step(
    pool: &sqlx::PgPool,
    execution_id: Uuid,
    node_id: &str,
    status: &str,
    output: Option<&serde_json::Value>,
    error: Option<&str>,
) -> Result<WorkflowStep, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowStep>(
        r#"
        INSERT INTO workflow_steps (execution_id, node_id, status, output, error)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (execution_id, node_id) DO UPDATE
        SET status = EXCLUDED.status, output = EXCLUDED.output, error = EXCLUDED.error
        RETURNING id, execution_id, node_id, status, output, error, created_at
        "#,
    )
    .bind(execution_id)
    .bind(node_id)
    .bind(status)
    .bind(output)
    .bind(error)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn list_steps_by_execution(
    pool: &sqlx::PgPool,
    execution_id: Uuid,
) -> Result<Vec<WorkflowStep>, sqlx::Error> {
    let rows = sqlx::query_as::<_, WorkflowStep>(
        r#"
        SELECT id, execution_id, node_id, status, output, error, created_at
        FROM workflow_steps
        WHERE execution_id = $1
        ORDER BY created_at ASC
        "#,
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_service_by_slug(
    pool: &sqlx::PgPool,
    slug: &str,
) -> Result<Option<ServiceRegistryRow>, sqlx::Error> {
    let row = sqlx::query_as::<_, ServiceRegistryRow>(
        r#"
        SELECT id, slug, name, base_url, created_at, updated_at
        FROM service_registry
        WHERE slug = $1
        LIMIT 1
        "#,
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
