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
pub struct WorkflowGlobal {
    pub tenant: String,
    pub key: String,
    pub value: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Workflow {
    pub id: Uuid,
    pub tenant: String,
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
    tenant: &str,
    name: &str,
    version: i32,
    definition: &serde_json::Value,
) -> Result<Workflow, sqlx::Error> {
    let row = sqlx::query_as::<_, Workflow>(
        r#"
        INSERT INTO workflows (tenant, name, version, definition, is_latest)
        VALUES ($1, $2, $3, $4, true)
        RETURNING id, tenant, name, version, is_latest, definition, created_at, updated_at
        "#,
    )
    .bind(tenant)
    .bind(name)
    .bind(version)
    .bind(definition)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"UPDATE workflows SET is_latest = false WHERE tenant = $1 AND name = $2 AND id != $3"#,
    )
    .bind(tenant)
    .bind(name)
    .bind(row.id)
    .execute(pool)
    .await?;
    Ok(row)
}

pub async fn update_workflow(
    pool: &sqlx::PgPool,
    id: Uuid,
    tenant: Option<&str>,
    definition: Option<&serde_json::Value>,
    is_latest: Option<bool>,
) -> Result<Option<Workflow>, sqlx::Error> {
    let existing = match get_workflow_by_id(pool, id).await? {
        Some(w) => w,
        None => return Ok(None),
    };
    if let Some(t) = tenant {
        sqlx::query(
            r#"UPDATE workflows SET tenant = $1, updated_at = now() WHERE id = $2"#,
        )
        .bind(t)
        .bind(id)
        .execute(pool)
        .await?;
    }
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
            r#"UPDATE workflows SET is_latest = false WHERE tenant = $1 AND name = $2"#,
        )
        .bind(&existing.tenant)
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

pub async fn delete_workflow(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    // Executions (and their steps) are removed automatically via ON DELETE CASCADE
    // on workflow_executions.workflow_id and workflow_steps.execution_id.
    let result = sqlx::query(r#"DELETE FROM workflows WHERE id = $1"#)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get_workflow_by_id(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<Workflow>, sqlx::Error> {
    let row = sqlx::query_as::<_, Workflow>(
        r#"
        SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
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
    tenant: Option<&str>,
    version: Option<i32>,
) -> Result<Option<Workflow>, sqlx::Error> {
    let row = if let Some(v) = version {
        if let Some(t) = tenant {
            sqlx::query_as::<_, Workflow>(
                r#"
                SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
                FROM workflows WHERE name = $1 AND tenant = $2 AND version = $3
                LIMIT 1
                "#,
            )
            .bind(name)
            .bind(t)
            .bind(v)
            .fetch_optional(pool)
            .await?
        } else {
            sqlx::query_as::<_, Workflow>(
                r#"
                SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
                FROM workflows WHERE name = $1 AND version = $2
                LIMIT 1
                "#,
            )
            .bind(name)
            .bind(v)
            .fetch_optional(pool)
            .await?
        }
    } else if let Some(t) = tenant {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
            FROM workflows WHERE name = $1 AND tenant = $2
            ORDER BY is_latest DESC, updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .bind(t)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
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
    tenant: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Workflow>, sqlx::Error> {
    let rows = if let Some(t) = tenant {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
            FROM workflows WHERE tenant = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(t)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, Workflow>(
            r#"
            SELECT id, tenant, name, version, is_latest, definition, created_at, updated_at
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
    initial_status: Option<&str>,
) -> Result<WorkflowExecution, sqlx::Error> {
    let status = initial_status.unwrap_or("running");
    let row = sqlx::query_as::<_, WorkflowExecution>(
        r#"
        INSERT INTO workflow_executions (workflow_id, workflow_version, status, context)
        VALUES ($1, $2, $3, $4)
        RETURNING id, workflow_id, workflow_version, status, context, started_at, finished_at
        "#,
    )
    .bind(workflow_id)
    .bind(workflow_version)
    .bind(status)
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

/// Push the shared `FROM` + optional tenant join + `WHERE` filters onto a query
/// builder. Used by both `list_executions` and `count_executions` so the two
/// always apply identical filtering.
fn push_executions_filters<'a>(
    qb: &mut sqlx::QueryBuilder<'a, sqlx::Postgres>,
    workflow_id: Option<Uuid>,
    tenant: Option<&'a str>,
    since: Option<DateTime<Utc>>,
) {
    qb.push(" FROM workflow_executions e");
    // Tenant scoping is enforced by joining to the owning workflow row.
    if let Some(t) = tenant {
        qb.push(" INNER JOIN workflows w ON w.id = e.workflow_id AND w.tenant = ");
        qb.push_bind(t);
    }
    qb.push(" WHERE TRUE");
    if let Some(wid) = workflow_id {
        qb.push(" AND e.workflow_id = ");
        qb.push_bind(wid);
    }
    if let Some(s) = since {
        qb.push(" AND e.started_at >= ");
        qb.push_bind(s);
    }
}

/// List executions, newest first, optionally filtered by workflow, tenant, and a
/// lower bound on `started_at` (`since`). `limit`/`offset` drive pagination.
pub async fn list_executions(
    pool: &sqlx::PgPool,
    workflow_id: Option<Uuid>,
    tenant: Option<&str>,
    since: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<WorkflowExecution>, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT e.id, e.workflow_id, e.workflow_version, e.status, e.context, e.started_at, e.finished_at",
    );
    push_executions_filters(&mut qb, workflow_id, tenant, since);
    qb.push(" ORDER BY e.started_at DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    qb.build_query_as::<WorkflowExecution>().fetch_all(pool).await
}

/// Total number of executions matching the same filters as `list_executions`,
/// ignoring `limit`/`offset`. Used to report the full count for pagination.
pub async fn count_executions(
    pool: &sqlx::PgPool,
    workflow_id: Option<Uuid>,
    tenant: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<i64, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*)");
    push_executions_filters(&mut qb, workflow_id, tenant, since);
    let (count,): (i64,) = qb.build_query_as().fetch_one(pool).await?;
    Ok(count)
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

/// List all globals for a tenant, most recently updated first.
pub async fn list_globals(
    pool: &sqlx::PgPool,
    tenant: &str,
) -> Result<Vec<WorkflowGlobal>, sqlx::Error> {
    let rows = sqlx::query_as::<_, WorkflowGlobal>(
        r#"
        SELECT tenant, key, value, created_at, updated_at
        FROM workflow_globals
        WHERE tenant = $1
        ORDER BY key ASC
        "#,
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch a single global by tenant + key.
pub async fn get_global(
    pool: &sqlx::PgPool,
    tenant: &str,
    key: &str,
) -> Result<Option<WorkflowGlobal>, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowGlobal>(
        r#"
        SELECT tenant, key, value, created_at, updated_at
        FROM workflow_globals
        WHERE tenant = $1 AND key = $2
        "#,
    )
    .bind(tenant)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Return a tenant's globals as a single JSON object `{ key: value, ... }`,
/// ready to inject into an execution context under `global`.
pub async fn get_globals_map(
    pool: &sqlx::PgPool,
    tenant: &str,
) -> Result<serde_json::Value, sqlx::Error> {
    let rows = list_globals(pool, tenant).await?;
    let mut map = serde_json::Map::new();
    for row in rows {
        map.insert(row.key, row.value);
    }
    Ok(serde_json::Value::Object(map))
}

/// Upsert a global value for a tenant + key.
pub async fn set_global(
    pool: &sqlx::PgPool,
    tenant: &str,
    key: &str,
    value: &serde_json::Value,
) -> Result<WorkflowGlobal, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowGlobal>(
        r#"
        INSERT INTO workflow_globals (tenant, key, value)
        VALUES ($1, $2, $3)
        ON CONFLICT (tenant, key) DO UPDATE
        SET value = EXCLUDED.value, updated_at = now()
        RETURNING tenant, key, value, created_at, updated_at
        "#,
    )
    .bind(tenant)
    .bind(key)
    .bind(value)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Delete a global. Returns true if a row was removed.
pub async fn delete_global(
    pool: &sqlx::PgPool,
    tenant: &str,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(r#"DELETE FROM workflow_globals WHERE tenant = $1 AND key = $2"#)
        .bind(tenant)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
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
