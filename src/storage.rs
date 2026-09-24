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
    /// Loop iteration this step ran in ("0", "2.1" for nested loops); empty outside loops.
    pub iteration: String,
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

/// A step held in memory until its run is written out (see `executor::Recorder`).
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub node_id: String,
    pub iteration: String,
    pub status: String,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Write a whole finished execution and its steps in one transaction. Used when a run
/// was kept in memory and only saved once it failed. Steps keep the time they ran at,
/// so they list in the order they happened.
pub async fn insert_finished_execution(
    pool: &sqlx::PgPool,
    execution: &WorkflowExecution,
    steps: &[StepRecord],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO workflow_executions (id, workflow_id, workflow_version, status, context, started_at, finished_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(execution.id)
    .bind(execution.workflow_id)
    .bind(execution.workflow_version)
    .bind(&execution.status)
    .bind(&execution.context)
    .bind(execution.started_at)
    .bind(execution.finished_at)
    .execute(&mut *tx)
    .await?;
    for s in steps {
        sqlx::query(
            r#"
            INSERT INTO workflow_steps (execution_id, node_id, iteration, status, output, error, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (execution_id, node_id, iteration) DO UPDATE
            SET status = EXCLUDED.status, output = EXCLUDED.output, error = EXCLUDED.error
            "#,
        )
        .bind(execution.id)
        .bind(&s.node_id)
        .bind(&s.iteration)
        .bind(&s.status)
        .bind(&s.output)
        .bind(&s.error)
        .bind(s.created_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
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
    iteration: &str,
    status: &str,
    output: Option<&serde_json::Value>,
    error: Option<&str>,
) -> Result<WorkflowStep, sqlx::Error> {
    let row = sqlx::query_as::<_, WorkflowStep>(
        r#"
        INSERT INTO workflow_steps (execution_id, node_id, iteration, status, output, error)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (execution_id, node_id, iteration) DO UPDATE
        SET status = EXCLUDED.status, output = EXCLUDED.output, error = EXCLUDED.error
        RETURNING id, execution_id, node_id, status, output, error, iteration, created_at
        "#,
    )
    .bind(execution_id)
    .bind(node_id)
    .bind(iteration)
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
        SELECT id, execution_id, node_id, status, output, error, iteration, created_at
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

/// One immutable version of a reusable node template (see `crate::templates`).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct NodeTemplate {
    pub id: Uuid,
    pub tenant: String,
    pub slug: String,
    pub version: i32,
    pub is_latest: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub node_type: String,
    pub config: serde_json::Value,
    pub params: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// The editable content of a node template; publishing it creates a new version.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeTemplateContent {
    pub name: Option<String>,
    pub description: Option<String>,
    pub node_type: String,
    pub config: serde_json::Value,
    pub params: serde_json::Value,
}

const NODE_TEMPLATE_COLUMNS: &str =
    "id, tenant, slug, version, is_latest, name, description, node_type, config, params, created_at";

/// The latest version of each of a tenant's templates, or every version when `all_versions`.
pub async fn list_node_templates(
    pool: &sqlx::PgPool,
    tenant: &str,
    all_versions: bool,
) -> Result<Vec<NodeTemplate>, sqlx::Error> {
    let sql = format!(
        "SELECT {NODE_TEMPLATE_COLUMNS} FROM node_templates
         WHERE tenant = $1 AND ($2 OR is_latest)
         ORDER BY slug, version DESC"
    );
    sqlx::query_as::<_, NodeTemplate>(&sql)
        .bind(tenant)
        .bind(all_versions)
        .fetch_all(pool)
        .await
}

/// One template version: `version`, or the latest when `None`.
pub async fn get_node_template(
    pool: &sqlx::PgPool,
    tenant: &str,
    slug: &str,
    version: Option<i32>,
) -> Result<Option<NodeTemplate>, sqlx::Error> {
    let sql = format!(
        "SELECT {NODE_TEMPLATE_COLUMNS} FROM node_templates
         WHERE tenant = $1 AND slug = $2 AND (($3::int IS NULL AND is_latest) OR version = $3)"
    );
    sqlx::query_as::<_, NodeTemplate>(&sql)
        .bind(tenant)
        .bind(slug)
        .bind(version)
        .fetch_optional(pool)
        .await
}

/// Candidate rows for resolving a set of references in one query: the latest version of
/// each slug plus any of the listed pinned versions. The caller picks the exact match.
pub async fn fetch_node_templates(
    pool: &sqlx::PgPool,
    tenant: &str,
    slugs: &[String],
    versions: &[i32],
) -> Result<Vec<NodeTemplate>, sqlx::Error> {
    let sql = format!(
        "SELECT {NODE_TEMPLATE_COLUMNS} FROM node_templates
         WHERE tenant = $1 AND slug = ANY($2) AND (is_latest OR version = ANY($3))"
    );
    sqlx::query_as::<_, NodeTemplate>(&sql)
        .bind(tenant)
        .bind(slugs)
        .bind(versions)
        .fetch_all(pool)
        .await
}

/// Publish `content` as the next version of `slug` and mark it latest. When it is identical
/// to the current latest version nothing is written, so re-syncing unchanged content does
/// not mint versions. Returns the latest version and whether it was created.
pub async fn publish_node_template(
    pool: &sqlx::PgPool,
    tenant: &str,
    slug: &str,
    content: &NodeTemplateContent,
) -> Result<(NodeTemplate, bool), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Serialize publishes of the same template so version numbers cannot collide.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1 || '/' || $2))")
        .bind(tenant)
        .bind(slug)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "SELECT {NODE_TEMPLATE_COLUMNS} FROM node_templates
         WHERE tenant = $1 AND slug = $2 ORDER BY version DESC LIMIT 1"
    );
    let newest = sqlx::query_as::<_, NodeTemplate>(&sql)
        .bind(tenant)
        .bind(slug)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(t) = newest.as_ref().filter(|t| t.is_latest) {
        let current = NodeTemplateContent {
            name: t.name.clone(),
            description: t.description.clone(),
            node_type: t.node_type.clone(),
            config: t.config.clone(),
            params: t.params.clone(),
        };
        if current == *content {
            tx.commit().await?;
            return Ok((t.clone(), false));
        }
    }
    let version = newest.map_or(1, |t| t.version + 1);
    sqlx::query("UPDATE node_templates SET is_latest = false WHERE tenant = $1 AND slug = $2")
        .bind(tenant)
        .bind(slug)
        .execute(&mut *tx)
        .await?;
    let sql = format!(
        "INSERT INTO node_templates (tenant, slug, version, is_latest, name, description, node_type, config, params)
         VALUES ($1, $2, $3, true, $4, $5, $6, $7, $8)
         RETURNING {NODE_TEMPLATE_COLUMNS}"
    );
    let row = sqlx::query_as::<_, NodeTemplate>(&sql)
        .bind(tenant)
        .bind(slug)
        .bind(version)
        .bind(&content.name)
        .bind(&content.description)
        .bind(&content.node_type)
        .bind(&content.config)
        .bind(&content.params)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((row, true))
}

/// Delete one version of a template, or all of them when `version` is `None`. If the latest
/// version goes, the highest remaining one becomes latest. Returns the number of rows removed.
pub async fn delete_node_template(
    pool: &sqlx::PgPool,
    tenant: &str,
    slug: &str,
    version: Option<i32>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query(
        "DELETE FROM node_templates WHERE tenant = $1 AND slug = $2 AND ($3::int IS NULL OR version = $3)",
    )
    .bind(tenant)
    .bind(slug)
    .bind(version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE node_templates SET is_latest = true
         WHERE id = (SELECT id FROM node_templates WHERE tenant = $1 AND slug = $2 ORDER BY version DESC LIMIT 1)
           AND NOT EXISTS (SELECT 1 FROM node_templates WHERE tenant = $1 AND slug = $2 AND is_latest)",
    )
    .bind(tenant)
    .bind(slug)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(deleted)
}

/// A workflow node that references a template (`version` `None` = follows latest).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TemplateUsage {
    pub node_id: String,
    pub slug: String,
    pub version: Option<i32>,
}

/// Replace the recorded template usages of a workflow with `usages`.
pub async fn replace_template_usages(
    pool: &sqlx::PgPool,
    workflow_id: Uuid,
    tenant: &str,
    usages: &[TemplateUsage],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM workflow_template_usages WHERE workflow_id = $1")
        .bind(workflow_id)
        .execute(&mut *tx)
        .await?;
    for u in usages {
        sqlx::query(
            "INSERT INTO workflow_template_usages (workflow_id, node_id, tenant, slug, version)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(workflow_id)
        .bind(&u.node_id)
        .bind(tenant)
        .bind(&u.slug)
        .bind(u.version)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A workflow node using a template, with the workflow it belongs to.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TemplateUsageRow {
    pub workflow_id: Uuid,
    pub workflow_name: String,
    pub workflow_version: i32,
    pub workflow_is_latest: bool,
    pub node_id: String,
    pub version: Option<i32>,
    #[serde(skip)]
    pub definition: serde_json::Value,
}

/// Every workflow node that references `slug`.
pub async fn list_template_usages(
    pool: &sqlx::PgPool,
    tenant: &str,
    slug: &str,
) -> Result<Vec<TemplateUsageRow>, sqlx::Error> {
    sqlx::query_as::<_, TemplateUsageRow>(
        "SELECT u.workflow_id, w.name AS workflow_name, w.version AS workflow_version,
                w.is_latest AS workflow_is_latest, u.node_id, u.version, w.definition
         FROM workflow_template_usages u JOIN workflows w ON w.id = u.workflow_id
         WHERE u.tenant = $1 AND u.slug = $2
         ORDER BY w.name, w.version, u.node_id",
    )
    .bind(tenant)
    .bind(slug)
    .fetch_all(pool)
    .await
}
