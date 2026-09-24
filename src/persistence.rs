//! How much of a run is saved to `workflow_executions` / `workflow_steps`.
//!
//! A workflow picks a mode with `persistence` in its definition (`data.persistence`, or
//! top-level `persistence`); a webhook call can override it with `?persist=`.
//!   - `full`: the execution row and every step are written while the run is going.
//!   - `errors_only` (default): the run is kept in memory. If it fails, the execution and
//!     every step up to the failure (completed, skipped and the failed one) are written
//!     in one go; a successful run leaves nothing behind.
//!   - `none`: nothing is written.
//!
//! Step mode always uses `full`, since each step reloads its state from the database.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use uuid::Uuid;

use crate::storage::{self, StepRecord, WorkflowExecution};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persistence {
    Full,
    #[default]
    ErrorsOnly,
    None,
}

impl Persistence {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "full" => Ok(Self::Full),
            "errors_only" | "errorsOnly" => Ok(Self::ErrorsOnly),
            "none" => Ok(Self::None),
            other => Err(format!(
                "invalid persistence '{other}': expected full, errors_only or none"
            )),
        }
    }

    /// The mode declared in a workflow definition, or the default when it declares none.
    pub fn from_definition(definition: &Value) -> Result<Self, String> {
        let declared = definition
            .get("data")
            .and_then(|d| d.get("persistence"))
            .or_else(|| definition.get("persistence"));
        match declared {
            None | Some(Value::Null) => Ok(Self::default()),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(Self::default()),
            Some(Value::String(s)) => Self::parse(s),
            Some(_) => Err("persistence must be one of: full, errors_only, none".to_string()),
        }
    }
}

/// Writes (or holds back) one run's execution row and steps according to its mode.
pub struct Recorder {
    pool: sqlx::PgPool,
    mode: Persistence,
    execution_id: Uuid,
    workflow_id: Uuid,
    workflow_version: Option<i32>,
    started_at: DateTime<Utc>,
    /// Steps held back in `errors_only` mode until the run fails.
    buffer: Mutex<Vec<StepRecord>>,
    /// Set once the run is finalized, so a failure is recorded only once.
    finished: AtomicBool,
}

impl Recorder {
    /// Start a new run. In `full` mode its execution row is created now; otherwise the
    /// id is only reserved and nothing is written yet.
    pub async fn start(
        pool: &sqlx::PgPool,
        mode: Persistence,
        workflow_id: Uuid,
        workflow_version: Option<i32>,
        initial_context: &Value,
    ) -> Result<Self, sqlx::Error> {
        let (execution_id, started_at) = if mode == Persistence::Full {
            let exec = storage::create_execution(pool, workflow_id, workflow_version, initial_context, None).await?;
            (exec.id, exec.started_at)
        } else {
            (Uuid::new_v4(), Utc::now())
        };
        Ok(Self {
            pool: pool.clone(),
            mode,
            execution_id,
            workflow_id,
            workflow_version,
            started_at,
            buffer: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        })
    }

    /// Record into an execution whose row already exists (step mode), always in `full` mode.
    pub fn existing(pool: &sqlx::PgPool, execution_id: Uuid, workflow_id: Uuid) -> Self {
        Self {
            pool: pool.clone(),
            mode: Persistence::Full,
            execution_id,
            workflow_id,
            workflow_version: None,
            started_at: Utc::now(),
            buffer: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
        }
    }

    pub fn execution_id(&self) -> Uuid {
        self.execution_id
    }

    pub fn workflow_id(&self) -> Uuid {
        self.workflow_id
    }

    pub fn mode(&self) -> Persistence {
        self.mode
    }

    /// Record one node's step.
    pub async fn step(
        &self,
        node_id: &str,
        iteration: &str,
        status: &str,
        output: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), String> {
        match self.mode {
            Persistence::Full => {
                storage::insert_step(&self.pool, self.execution_id, node_id, iteration, status, output, error)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Persistence::ErrorsOnly => self.lock_buffer().push(StepRecord {
                node_id: node_id.to_string(),
                iteration: iteration.to_string(),
                status: status.to_string(),
                output: output.cloned(),
                error: error.map(str::to_string),
                created_at: Utc::now(),
            }),
            Persistence::None => {}
        }
        Ok(())
    }

    /// Save the context mid-run (only `full` mode writes progress).
    pub async fn progress(&self, context: &Value) -> Result<(), String> {
        if self.mode == Persistence::Full {
            storage::update_execution(&self.pool, self.execution_id, "running", context, None)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Mark the run completed. In `errors_only` mode the held-back steps are dropped.
    pub async fn complete(&self, context: &Value) -> Result<(), String> {
        if self.finished.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match self.mode {
            Persistence::Full => storage::update_execution(
                &self.pool,
                self.execution_id,
                "completed",
                context,
                Some(Utc::now()),
            )
            .await
            .map_err(|e| e.to_string()),
            Persistence::ErrorsOnly => {
                self.lock_buffer().clear();
                Ok(())
            }
            Persistence::None => Ok(()),
        }
    }

    /// Mark the run failed with the context as it stood at the failure. In `errors_only`
    /// mode this is when the execution and all its held-back steps are written. Only the
    /// first call has an effect.
    pub async fn fail(&self, context: &Value) -> Result<(), String> {
        if self.finished.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match self.mode {
            Persistence::Full => storage::update_execution(
                &self.pool,
                self.execution_id,
                "failed",
                context,
                Some(Utc::now()),
            )
            .await
            .map_err(|e| e.to_string()),
            Persistence::ErrorsOnly => {
                let steps = std::mem::take(&mut *self.lock_buffer());
                let execution = WorkflowExecution {
                    id: self.execution_id,
                    workflow_id: self.workflow_id,
                    workflow_version: self.workflow_version,
                    status: "failed".to_string(),
                    context: context.clone(),
                    started_at: self.started_at,
                    finished_at: Some(Utc::now()),
                };
                storage::insert_finished_execution(&self.pool, &execution, &steps)
                    .await
                    .map_err(|e| e.to_string())
            }
            Persistence::None => Ok(()),
        }
    }

    fn lock_buffer(&self) -> std::sync::MutexGuard<'_, Vec<StepRecord>> {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_to_errors_only() {
        assert_eq!(Persistence::from_definition(&json!({ "data": {} })), Ok(Persistence::ErrorsOnly));
        assert_eq!(
            Persistence::from_definition(&json!({ "data": { "persistence": "" } })),
            Ok(Persistence::ErrorsOnly)
        );
    }

    #[test]
    fn reads_mode_from_data_or_top_level() {
        assert_eq!(
            Persistence::from_definition(&json!({ "data": { "persistence": "full" } })),
            Ok(Persistence::Full)
        );
        assert_eq!(Persistence::from_definition(&json!({ "persistence": "none" })), Ok(Persistence::None));
        assert_eq!(Persistence::parse("errorsOnly"), Ok(Persistence::ErrorsOnly));
    }

    #[test]
    fn rejects_unknown_mode() {
        assert!(Persistence::from_definition(&json!({ "persistence": "sometimes" })).is_err());
        assert!(Persistence::from_definition(&json!({ "persistence": true })).is_err());
    }
}
