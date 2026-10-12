use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::schedule::{CatchUp, Overlap, ScheduleSpec};
use crate::triggers::{RunOrigin, TriggerKind};

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
    /// What started the run: `webhook`, `schedule` or `workflow_call`.
    pub trigger: String,
    /// The schedule that started it, for `schedule` runs.
    pub schedule_id: Option<Uuid>,
}

const EXECUTION_COLUMNS: &str =
    "id, workflow_id, workflow_version, status, context, started_at, finished_at, trigger, schedule_id";

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
    id: Uuid,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    context: &serde_json::Value,
    initial_status: Option<&str>,
    origin: RunOrigin,
) -> Result<WorkflowExecution, sqlx::Error> {
    let status = initial_status.unwrap_or("running");
    let row = sqlx::query_as::<_, WorkflowExecution>(&format!(
        r#"
        INSERT INTO workflow_executions (id, workflow_id, workflow_version, status, context, trigger, schedule_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING {EXECUTION_COLUMNS}
        "#
    ))
    .bind(id)
    .bind(workflow_id)
    .bind(workflow_version)
    .bind(status)
    .bind(context)
    .bind(origin.trigger.as_str())
    .bind(origin.schedule_id)
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
    insert_execution_with_steps(&mut tx, execution, steps).await?;
    tx.commit().await
}

async fn insert_execution_with_steps(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    execution: &WorkflowExecution,
    steps: &[StepRecord],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO workflow_executions
            (id, workflow_id, workflow_version, status, context, started_at, finished_at, trigger, schedule_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(execution.id)
    .bind(execution.workflow_id)
    .bind(execution.workflow_version)
    .bind(&execution.status)
    .bind(&execution.context)
    .bind(execution.started_at)
    .bind(execution.finished_at)
    .bind(&execution.trigger)
    .bind(execution.schedule_id)
    .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// What a waiting execution is waiting for (see the `workflow_waits` migration).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct WorkflowWait {
    pub execution_id: Uuid,
    pub node_id: String,
    pub iteration: String,
    pub wake_at: Option<DateTime<Utc>>,
    pub correlation_key: Option<String>,
    pub filter: Option<String>,
    pub state: serde_json::Value,
    pub step_mode: bool,
    pub trace_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

const WAIT_COLUMNS: &str =
    "execution_id, node_id, iteration, wake_at, correlation_key, filter, state, step_mode, trace_id, created_at";

/// Save a run as `waiting` on `wait`, in one transaction. `unsaved` is the execution row and
/// steps of a run that was so far kept in memory (see `persistence::Recorder`); `None` when
/// the row already exists, in which case only its status and context are updated.
pub async fn suspend_execution(
    pool: &sqlx::PgPool,
    execution_id: Uuid,
    context: &serde_json::Value,
    wait: &WorkflowWait,
    unsaved: Option<(&WorkflowExecution, &[StepRecord])>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    match unsaved {
        Some((execution, steps)) => insert_execution_with_steps(&mut tx, execution, steps).await?,
        None => {
            sqlx::query("UPDATE workflow_executions SET status = 'waiting', context = $2 WHERE id = $1")
                .bind(execution_id)
                .bind(context)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query(
        r#"
        INSERT INTO workflow_waits
            (execution_id, node_id, iteration, wake_at, correlation_key, filter, state, step_mode, trace_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(execution_id)
    .bind(&wait.node_id)
    .bind(&wait.iteration)
    .bind(wait.wake_at)
    .bind(&wait.correlation_key)
    .bind(&wait.filter)
    .bind(&wait.state)
    .bind(wait.step_mode)
    .bind(&wait.trace_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

pub async fn get_wait(pool: &sqlx::PgPool, execution_id: Uuid) -> Result<Option<WorkflowWait>, sqlx::Error> {
    sqlx::query_as::<_, WorkflowWait>(&format!(
        "SELECT {WAIT_COLUMNS} FROM workflow_waits WHERE execution_id = $1"
    ))
    .bind(execution_id)
    .fetch_optional(pool)
    .await
}

/// Executions whose timer is due, earliest first.
pub async fn due_waits(pool: &sqlx::PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT execution_id FROM workflow_waits WHERE wake_at <= now() ORDER BY wake_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Waits on a signal key, limited to one tenant's workflows.
pub async fn waits_for_key(
    pool: &sqlx::PgPool,
    tenant: &str,
    correlation_key: &str,
) -> Result<Vec<WorkflowWait>, sqlx::Error> {
    let columns = WAIT_COLUMNS
        .split(", ")
        .map(|c| format!("w.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query_as::<_, WorkflowWait>(&format!(
        r#"
        SELECT {columns}
        FROM workflow_waits w
        JOIN workflow_executions e ON e.id = w.execution_id
        JOIN workflows f ON f.id = e.workflow_id
        WHERE w.correlation_key = $1 AND f.tenant = $2
        ORDER BY w.created_at
        "#
    ))
    .bind(correlation_key)
    .bind(tenant)
    .fetch_all(pool)
    .await
}

/// Take a waiting execution to resume it: delete its wait and mark it `running`, atomically.
/// Returns `None` when someone else got there first (a timer and a signal racing, another
/// replica, or a cancel), so each wait resumes at most once.
pub async fn claim_wait(pool: &sqlx::PgPool, execution_id: Uuid) -> Result<Option<WorkflowWait>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let wait = sqlx::query_as::<_, WorkflowWait>(&format!(
        "DELETE FROM workflow_waits WHERE execution_id = $1 RETURNING {WAIT_COLUMNS}"
    ))
    .bind(execution_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(wait) = wait else {
        return Ok(None);
    };
    let updated = sqlx::query(
        "UPDATE workflow_executions SET status = 'running' WHERE id = $1 AND status = 'waiting'",
    )
    .bind(execution_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(wait))
}

/// A run accepted but not yet started (see the `workflow_run_queue` migration).
#[derive(Debug, Clone, FromRow)]
pub struct QueuedRun {
    /// The execution id the run will have.
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub trigger_context: serde_json::Value,
    pub trigger: String,
    pub schedule_id: Option<Uuid>,
    /// Overrides the workflow's persistence mode (`full`, `errors_only`, `none`).
    pub persistence: Option<String>,
    pub trace_id: String,
    pub available_at: DateTime<Utc>,
    pub enqueued_at: DateTime<Utc>,
}

const QUEUED_RUN_COLUMNS: &str =
    "id, workflow_id, trigger_context, trigger, schedule_id, persistence, trace_id, available_at, enqueued_at";

/// Add a run to the queue (on a pool, or inside a transaction).
pub async fn enqueue_run<'e>(executor: impl sqlx::PgExecutor<'e>, run: &QueuedRun) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "INSERT INTO workflow_run_queue ({QUEUED_RUN_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
    ))
    .bind(run.id)
    .bind(run.workflow_id)
    .bind(&run.trigger_context)
    .bind(&run.trigger)
    .bind(run.schedule_id)
    .bind(&run.persistence)
    .bind(&run.trace_id)
    .bind(run.available_at)
    .bind(run.enqueued_at)
    .execute(executor)
    .await?;
    Ok(())
}

/// Take up to `limit` runs that are due, oldest first, removing them from the queue. Rows
/// another worker is taking at the same moment are skipped, so each run is taken once.
pub async fn claim_queued_runs(pool: &sqlx::PgPool, limit: i64) -> Result<Vec<QueuedRun>, sqlx::Error> {
    let mut runs = sqlx::query_as::<_, QueuedRun>(&format!(
        r#"
        DELETE FROM workflow_run_queue
        WHERE id IN (
            SELECT id FROM workflow_run_queue
            WHERE available_at <= now()
            ORDER BY available_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING {QUEUED_RUN_COLUMNS}
        "#
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    runs.sort_by_key(|r| r.available_at);
    Ok(runs)
}

pub async fn get_queued_run(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<QueuedRun>, sqlx::Error> {
    sqlx::query_as::<_, QueuedRun>(&format!("SELECT {QUEUED_RUN_COLUMNS} FROM workflow_run_queue WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Cancel a run still in the queue: remove it and record it as a `cancelled` execution, so
/// its id stays visible. Returns false when it is not in the queue (already started, or
/// never queued).
pub async fn cancel_queued_run(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let run = sqlx::query_as::<_, QueuedRun>(&format!(
        "DELETE FROM workflow_run_queue WHERE id = $1 RETURNING {QUEUED_RUN_COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = run else {
        return Ok(false);
    };
    sqlx::query(
        r#"
        INSERT INTO workflow_executions (id, workflow_id, status, context, started_at, finished_at, trigger, schedule_id)
        VALUES ($1, $2, 'cancelled', $3, now(), now(), $4, $5)
        "#,
    )
    .bind(run.id)
    .bind(run.workflow_id)
    .bind(&run.trigger_context)
    .bind(&run.trigger)
    .bind(run.schedule_id)
    .execute(&mut *tx)
    .await?;
    if let Some(schedule_id) = run.schedule_id {
        release_schedule_run(&mut *tx, schedule_id, run.id, false).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Cancel a waiting or paused execution. Returns false when it is in any other state.
pub async fn cancel_execution(pool: &sqlx::PgPool, execution_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE workflow_executions SET status = 'cancelled', finished_at = now()
        WHERE id = $1 AND status IN ('waiting', 'paused')
        "#,
    )
    .bind(execution_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM workflow_waits WHERE execution_id = $1")
        .bind(execution_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
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
    let row = sqlx::query_as::<_, WorkflowExecution>(&format!(
        "SELECT {EXECUTION_COLUMNS} FROM workflow_executions WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Filters for listing executions. Every field is optional; unset fields match everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecutionFilter<'a> {
    pub workflow_id: Option<Uuid>,
    /// Only executions of this tenant's workflows.
    pub tenant: Option<&'a str>,
    /// Only executions started at or after this time.
    pub since: Option<DateTime<Utc>>,
    pub trigger: Option<TriggerKind>,
    pub schedule_id: Option<Uuid>,
}

/// Push the shared `FROM` + optional tenant join + `WHERE` filters onto a query
/// builder. Used by both `list_executions` and `count_executions` so the two
/// always apply identical filtering.
fn push_executions_filters<'a>(qb: &mut sqlx::QueryBuilder<'a, sqlx::Postgres>, filter: ExecutionFilter<'a>) {
    qb.push(" FROM workflow_executions e");
    // Tenant scoping is enforced by joining to the owning workflow row.
    if let Some(t) = filter.tenant {
        qb.push(" INNER JOIN workflows w ON w.id = e.workflow_id AND w.tenant = ");
        qb.push_bind(t);
    }
    qb.push(" WHERE TRUE");
    if let Some(wid) = filter.workflow_id {
        qb.push(" AND e.workflow_id = ");
        qb.push_bind(wid);
    }
    if let Some(s) = filter.since {
        qb.push(" AND e.started_at >= ");
        qb.push_bind(s);
    }
    if let Some(trigger) = filter.trigger {
        qb.push(" AND e.trigger = ");
        qb.push_bind(trigger.as_str());
    }
    if let Some(sid) = filter.schedule_id {
        qb.push(" AND e.schedule_id = ");
        qb.push_bind(sid);
    }
}

/// List executions matching `filter`, newest first. `limit`/`offset` drive pagination.
pub async fn list_executions(
    pool: &sqlx::PgPool,
    filter: ExecutionFilter<'_>,
    limit: i64,
    offset: i64,
) -> Result<Vec<WorkflowExecution>, sqlx::Error> {
    let columns = EXECUTION_COLUMNS
        .split(", ")
        .map(|c| format!("e.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut qb = sqlx::QueryBuilder::new(format!("SELECT {columns}"));
    push_executions_filters(&mut qb, filter);
    qb.push(" ORDER BY e.started_at DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);
    qb.build_query_as::<WorkflowExecution>().fetch_all(pool).await
}

/// Total number of executions matching the same filters as `list_executions`,
/// ignoring `limit`/`offset`. Used to report the full count for pagination.
pub async fn count_executions(pool: &sqlx::PgPool, filter: ExecutionFilter<'_>) -> Result<i64, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*)");
    push_executions_filters(&mut qb, filter);
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

/// A schedule as stored (see the `workflow_schedules` migration and [`crate::schedule`]).
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Schedule {
    pub id: Uuid,
    pub tenant: String,
    pub name: String,
    pub workflow_name: String,
    pub workflow_version: Option<i32>,
    pub cron: String,
    pub timezone: String,
    pub input: serde_json::Value,
    pub enabled: bool,
    pub overlap: String,
    pub catch_up: String,
    pub persistence: Option<String>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub active_execution_id: Option<Uuid>,
    pub active_until: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_scheduled_for: Option<DateTime<Utc>>,
    pub last_execution_id: Option<Uuid>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const SCHEDULE_COLUMNS: &str = "id, tenant, name, workflow_name, workflow_version, cron, timezone, input, enabled, \
     overlap, catch_up, persistence, next_run_at, active_execution_id, active_until, last_run_at, \
     last_scheduled_for, last_execution_id, last_outcome, last_error, created_at, updated_at";

impl Schedule {
    /// The schedule as written: what a config file holds for it.
    pub fn spec(&self) -> ScheduleSpec {
        ScheduleSpec {
            name: self.name.clone(),
            workflow: self.workflow_name.clone(),
            version: self.workflow_version,
            cron: self.cron.clone(),
            timezone: self.timezone.clone(),
            input: self.input.clone(),
            enabled: self.enabled,
            overlap: if self.overlap == Overlap::Allow.as_str() { Overlap::Allow } else { Overlap::Skip },
            catch_up: if self.catch_up == CatchUp::Skip.as_str() { CatchUp::Skip } else { CatchUp::Once },
            persist: self.persistence.clone(),
        }
    }
}

#[derive(FromRow)]
struct UpsertedSchedule {
    #[sqlx(flatten)]
    schedule: Schedule,
    inserted: bool,
}

/// Create or replace the tenant's schedule named `spec.name`. Returns it, and whether it was
/// created.
///
/// `next_run_at` is when it should fire next if its timing is new: it is used when the
/// schedule is created, re-enabled, or its cron or timezone changed. Otherwise the stored time
/// is kept, so saving an unchanged schedule (as every config sync does) never moves or drops
/// a slot that is due. A disabled schedule has no next time.
pub async fn upsert_schedule(
    pool: &sqlx::PgPool,
    tenant: &str,
    spec: &ScheduleSpec,
    next_run_at: Option<DateTime<Utc>>,
) -> Result<(Schedule, bool), sqlx::Error> {
    let returning = SCHEDULE_COLUMNS
        .split(", ")
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let row = sqlx::query_as::<_, UpsertedSchedule>(&format!(
        r#"
        INSERT INTO workflow_schedules AS s
            (tenant, name, workflow_name, workflow_version, cron, timezone, input, enabled, overlap, catch_up,
             persistence, next_run_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, CASE WHEN $8 THEN $12 END)
        ON CONFLICT (tenant, name) DO UPDATE SET
            workflow_name = EXCLUDED.workflow_name,
            workflow_version = EXCLUDED.workflow_version,
            cron = EXCLUDED.cron,
            timezone = EXCLUDED.timezone,
            input = EXCLUDED.input,
            enabled = EXCLUDED.enabled,
            overlap = EXCLUDED.overlap,
            catch_up = EXCLUDED.catch_up,
            persistence = EXCLUDED.persistence,
            next_run_at = CASE
                WHEN NOT EXCLUDED.enabled THEN NULL
                WHEN s.next_run_at IS NULL OR s.cron <> EXCLUDED.cron OR s.timezone <> EXCLUDED.timezone
                    THEN EXCLUDED.next_run_at
                ELSE s.next_run_at
            END,
            updated_at = now()
        RETURNING {returning}, (xmax = 0) AS inserted
        "#
    ))
    .bind(tenant)
    .bind(&spec.name)
    .bind(&spec.workflow)
    .bind(spec.version)
    .bind(&spec.cron)
    .bind(&spec.timezone)
    .bind(&spec.input)
    .bind(spec.enabled)
    .bind(spec.overlap.as_str())
    .bind(spec.catch_up.as_str())
    .bind(&spec.persist)
    .bind(next_run_at)
    .fetch_one(pool)
    .await?;
    Ok((row.schedule, row.inserted))
}

pub async fn get_schedule_by_name(pool: &sqlx::PgPool, tenant: &str, name: &str) -> Result<Option<Schedule>, sqlx::Error> {
    sqlx::query_as::<_, Schedule>(&format!(
        "SELECT {SCHEDULE_COLUMNS} FROM workflow_schedules WHERE tenant = $1 AND name = $2"
    ))
    .bind(tenant)
    .bind(name)
    .fetch_optional(pool)
    .await
}

pub async fn get_schedule_by_id(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<Schedule>, sqlx::Error> {
    sqlx::query_as::<_, Schedule>(&format!("SELECT {SCHEDULE_COLUMNS} FROM workflow_schedules WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// The tenant's schedules by name, optionally only those of one workflow.
pub async fn list_schedules(
    pool: &sqlx::PgPool,
    tenant: &str,
    workflow_name: Option<&str>,
) -> Result<Vec<Schedule>, sqlx::Error> {
    sqlx::query_as::<_, Schedule>(&format!(
        "SELECT {SCHEDULE_COLUMNS} FROM workflow_schedules
         WHERE tenant = $1 AND ($2::text IS NULL OR workflow_name = $2)
         ORDER BY name"
    ))
    .bind(tenant)
    .bind(workflow_name)
    .fetch_all(pool)
    .await
}

/// Delete the tenant's schedule. Its runs keep their history. Returns false if there was none.
pub async fn delete_schedule(pool: &sqlx::PgPool, tenant: &str, name: &str) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM workflow_schedules WHERE tenant = $1 AND name = $2")
        .bind(tenant)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// A due schedule, claimed by a scheduler, and whether its previous run is still going.
#[derive(Debug, Clone, FromRow)]
pub struct DueSchedule {
    #[sqlx(flatten)]
    pub schedule: Schedule,
    /// Its last scheduled run is queued, waiting or paused, or started less than the lease ago.
    pub busy: bool,
}

/// Lock up to `limit` enabled schedules that are due, earliest first, in `tx`. Schedules
/// another scheduler has locked are skipped, so each due slot is handled by one scheduler.
pub async fn claim_due_schedules(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    limit: i64,
) -> Result<Vec<DueSchedule>, sqlx::Error> {
    let columns = SCHEDULE_COLUMNS
        .split(", ")
        .map(|c| format!("s.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query_as::<_, DueSchedule>(&format!(
        r#"
        SELECT {columns},
            -- COALESCE: `active_until` is NULL once a run is waiting, and a NULL here would
            -- fail the whole batch, not just this schedule.
            COALESCE(s.active_execution_id IS NOT NULL AND (
                s.active_until > now()
                OR EXISTS (SELECT 1 FROM workflow_run_queue q WHERE q.id = s.active_execution_id)
                OR EXISTS (
                    SELECT 1 FROM workflow_executions e
                    WHERE e.id = s.active_execution_id AND e.status IN ('waiting', 'paused')
                )
            ), false) AS busy
        FROM workflow_schedules s
        WHERE s.enabled AND s.next_run_at <= now()
        ORDER BY s.next_run_at
        LIMIT $1
        FOR UPDATE OF s SKIP LOCKED
        "#
    ))
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
}

/// What happened when a schedule came due.
#[derive(Debug, Clone)]
pub struct ScheduleFired<'a> {
    pub next_run_at: Option<DateTime<Utc>>,
    /// The slot it was due for (or caught up on).
    pub scheduled_for: DateTime<Utc>,
    /// `queued`, `skipped_overlap`, `skipped_missed` or `failed_to_start`.
    pub outcome: &'a str,
    pub error: Option<&'a str>,
    /// The run queued for it, and until when to treat that run as going.
    pub queued: Option<(Uuid, DateTime<Utc>)>,
}

/// Record that a claimed schedule came due, and move it to its next slot.
pub async fn record_schedule_fired(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    schedule_id: Uuid,
    fired: &ScheduleFired<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE workflow_schedules SET
            next_run_at = $2,
            last_run_at = now(),
            last_scheduled_for = $3,
            last_outcome = $4,
            last_error = $5,
            last_execution_id = COALESCE($6, last_execution_id),
            active_execution_id = COALESCE($6, active_execution_id),
            active_until = CASE WHEN $6::uuid IS NULL THEN active_until ELSE $7 END
        WHERE id = $1
        "#,
    )
    .bind(schedule_id)
    .bind(fired.next_run_at)
    .bind(fired.scheduled_for)
    .bind(fired.outcome)
    .bind(fired.error)
    .bind(fired.queued.map(|(id, _)| id))
    .bind(fired.queued.map(|(_, until)| until))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A schedule's run `execution_id` has stopped running. When it finished (or never started),
/// the schedule no longer counts it as going; when it is `waiting`, only the lease is dropped,
/// and the execution's status says whether it is still going.
pub async fn release_schedule_run<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    schedule_id: Uuid,
    execution_id: Uuid,
    waiting: bool,
) -> Result<(), sqlx::Error> {
    let set = if waiting {
        "active_until = NULL"
    } else {
        "active_execution_id = NULL, active_until = NULL"
    };
    sqlx::query(&format!(
        "UPDATE workflow_schedules SET {set} WHERE id = $1 AND active_execution_id = $2"
    ))
    .bind(schedule_id)
    .bind(execution_id)
    .execute(executor)
    .await?;
    Ok(())
}
