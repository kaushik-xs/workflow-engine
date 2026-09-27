//! Resuming suspended runs.
//!
//! A node that suspends (see the Wait node) leaves its execution `waiting` with a row in
//! `workflow_waits`. Two things resume it, whichever comes first:
//!   - the timer worker, polling for waits whose `wake_at` has passed, and
//!   - a signal (`POST /signals`) whose key matches and whose payload passes the filter.
//!
//! Either one first claims the wait ([`storage::claim_wait`]), which deletes the row and
//! marks the run `running` in one transaction, so a wait resumes exactly once even when a
//! timer and a signal race or several replicas poll. The run then continues in the
//! background, from the node that suspended it.
//!
//! A run claimed but interrupted before it finishes (the process dies) stays `running`,
//! like any run interrupted mid-way.

use crate::executor::{self, ResumeTarget};
use crate::expression;
use crate::nodes::{truthy, ResumeReason};
use crate::registry::NodeRegistry;
use crate::storage::{self, WorkflowWait};
use crate::templates;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::Instrument;
use uuid::Uuid;

/// Resumes suspended runs, with at most `concurrency` running at a time.
pub struct Waits {
    pool: sqlx::PgPool,
    registry: Arc<dyn NodeRegistry>,
    permits: Arc<Semaphore>,
}

impl Waits {
    pub fn new(pool: sqlx::PgPool, registry: Arc<dyn NodeRegistry>, concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        })
    }

    /// Poll for due timers every `interval`, forever.
    pub fn spawn_timer_worker(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let waits = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = waits.resume_due().await {
                    tracing::error!(error = %e, "timer worker: failed to poll waits");
                }
            }
        })
    }

    /// Claim waits whose time has come, as many as there is room to run, and resume them.
    /// Returns how many were resumed.
    pub async fn resume_due(self: &Arc<Self>) -> Result<usize, sqlx::Error> {
        let room = self.permits.available_permits();
        if room == 0 {
            return Ok(0);
        }
        let mut resumed = 0;
        for execution_id in storage::due_waits(&self.pool, room as i64).await? {
            if let Some(wait) = storage::claim_wait(&self.pool, execution_id).await? {
                self.spawn_resume(wait, ResumeReason::Timer);
                resumed += 1;
            }
        }
        Ok(resumed)
    }

    /// Deliver a signal: resume every run of `tenant` waiting on `key` whose filter accepts
    /// `payload`. Returns the ids of the executions resumed.
    pub async fn signal(self: &Arc<Self>, tenant: &str, key: &str, payload: Value) -> Result<Vec<Uuid>, sqlx::Error> {
        let mut resumed = Vec::new();
        for wait in storage::waits_for_key(&self.pool, tenant, key).await? {
            if !signal_matches(wait.filter.as_deref(), key, &payload) {
                continue;
            }
            if let Some(wait) = storage::claim_wait(&self.pool, wait.execution_id).await? {
                resumed.push(wait.execution_id);
                self.spawn_resume(wait, ResumeReason::Signal(payload.clone()));
            }
        }
        Ok(resumed)
    }

    fn spawn_resume(self: &Arc<Self>, wait: WorkflowWait, reason: ResumeReason) {
        let waits = self.clone();
        let span = tracing::info_span!(
            "execution",
            trace_id = %wait.trace_id.as_deref().unwrap_or_default(),
            execution_id = %wait.execution_id,
        );
        tokio::spawn(
            async move {
                let _permit = waits.permits.clone().acquire_owned().await;
                let execution_id = wait.execution_id;
                if let Err(e) = waits.resume(wait, reason).await {
                    tracing::error!(execution_id = %execution_id, error = %e, "resumed run failed");
                }
            }
            .instrument(span),
        );
    }

    /// Continue a claimed run from its suspended node: to the end, or one step in step mode.
    async fn resume(&self, wait: WorkflowWait, reason: ResumeReason) -> Result<(), String> {
        let execution_id = wait.execution_id;
        let exec = storage::get_execution(&self.pool, execution_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("execution not found")?;
        let workflow = storage::get_workflow_by_id(&self.pool, exec.workflow_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("workflow not found")?;
        tracing::info!(execution_id = %execution_id, node_id = %wait.node_id, reason = ?reason, "resuming run");

        // Use the template versions the run started with, even if newer ones exist.
        let resolved = match templates::resolve_definition(
            &self.pool,
            &workflow.tenant,
            &workflow.definition,
            exec.context.get(templates::LOCK_CONTEXT_KEY),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(e) => {
                storage::update_execution(&self.pool, execution_id, "failed", &exec.context, Some(chrono::Utc::now()))
                    .await
                    .map_err(|e| e.to_string())?;
                return Err(e);
            }
        };

        let target = ResumeTarget {
            node_id: wait.node_id,
            iteration: wait.iteration,
            state: wait.state,
            reason,
        };
        if wait.step_mode {
            executor::run_next_step(
                &self.pool,
                self.registry.clone(),
                exec,
                &resolved.definition,
                wait.trace_id,
                Some(target),
            )
            .await
            .map(|_| ())
        } else {
            executor::resume_workflow(
                &self.pool,
                self.registry.clone(),
                exec,
                &resolved.definition,
                target,
                wait.trace_id,
            )
            .await
            .map(|_| ())
        }
    }
}

/// Whether a signal passes a wait's filter: a JMESPath expression over
/// `{ "signal": { "key", "payload" } }`. No filter accepts every signal; a filter that
/// fails to evaluate accepts none.
pub fn signal_matches(filter: Option<&str>, key: &str, payload: &Value) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    match expression::evaluate(filter, &json!({ "signal": { "key": key, "payload": payload } })) {
        Ok(v) => truthy(&v),
        Err(e) => {
            tracing::warn!(filter = %filter, error = %e, "signal filter failed to evaluate");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_signals() {
        let payload = json!({ "status": "ack", "by": "l2" });
        assert!(signal_matches(None, "k", &payload));
        assert!(signal_matches(Some("signal.payload.status == 'ack'"), "k", &payload));
        assert!(!signal_matches(Some("signal.payload.status == 'nack'"), "k", &payload));
        assert!(signal_matches(Some("signal.key == 'k'"), "k", &payload));
        assert!(!signal_matches(Some("signal.payload.missing"), "k", &payload));
        assert!(!signal_matches(Some("not valid ((("), "k", &payload));
    }
}
