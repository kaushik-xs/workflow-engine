//! Starting a run of a workflow.
//!
//! [`start_run`] is the one way a top-level run begins, whatever started it (a webhook call,
//! or a worker taking a queued run; see [`crate::run_queue`]): it builds the initial context, expands template references,
//! records the execution according to the persistence mode, and runs the workflow to the
//! end, to a suspension, or (in step mode) not at all.

use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tracing::Instrument;
use uuid::Uuid;

use crate::error::AppError;
use crate::executor;
use crate::persistence::{Persistence, Recorder};
use crate::registry::NodeRegistry;
use crate::storage::{self, Workflow};
use crate::templates;
use crate::triggers::RunOrigin;

/// How to start a run.
pub struct StartRun {
    /// The id the execution gets. `None`: a new one. A queued run already has one.
    pub execution_id: Option<Uuid>,
    /// The trigger's part of the initial context: `Webhook` and `Trigger` (see
    /// [`crate::triggers`]).
    pub trigger_context: Value,
    pub origin: RunOrigin,
    /// Create the execution `paused` without running any node (debug step mode).
    pub step_mode: bool,
    /// Overrides the workflow's own persistence mode.
    pub persistence: Option<Persistence>,
    pub trace_id: String,
}

/// How a run ended (or where it stopped). This is the webhook's response body.
#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub execution_id: Uuid,
    /// `completed`, `failed`, `waiting`, (step mode) `paused`, or `queued` for a run left
    /// to a worker.
    pub status: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// Start a run of `workflow` and drive it as far as it goes in this call.
///
/// A run that fails is reported in the outcome, not as an error; errors are for a run that
/// could not be started (a bad persistence mode, or a template failure in step mode) or
/// could not be recorded.
pub async fn start_run(
    pool: &sqlx::PgPool,
    registry: Arc<dyn NodeRegistry>,
    workflow: &Workflow,
    start: StartRun,
) -> Result<RunOutcome, AppError> {
    let StartRun { execution_id, trigger_context, origin, step_mode, persistence, trace_id } = start;
    let execution_id = execution_id.unwrap_or_else(Uuid::new_v4);
    // Snapshot the tenant's globals and seed the workflow's declared local variables into
    // the persisted execution context (used by both immediate and step-mode runs).
    let mut initial_context =
        executor::build_initial_context(pool, &workflow.tenant, &workflow.definition, trigger_context)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    let persistence = match persistence {
        Some(p) => p,
        None => Persistence::from_definition(&workflow.definition).map_err(AppError::BadRequest)?,
    };

    // Expand template references now; the versions used are recorded in the context.
    let resolved = templates::resolve_definition(pool, &workflow.tenant, &workflow.definition, None).await;
    let definition = match resolved {
        Ok(resolved) => {
            templates::record_lock(&mut initial_context, resolved.lock);
            resolved.definition
        }
        Err(e) if step_mode => return Err(AppError::BadRequest(e)),
        Err(e) => {
            // Record it like any other failure that stops a run before its first node.
            let recorder = Recorder::start(
                pool,
                persistence,
                execution_id,
                workflow.id,
                Some(workflow.version),
                origin,
                &initial_context,
            )
            .await?;
            recorder
                .fail(&initial_context)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
            return Ok(RunOutcome {
                execution_id: recorder.execution_id(),
                status: "failed".to_string(),
                result: None,
                error: Some(e),
            });
        }
    };

    if step_mode {
        // Step mode reloads state from the database between steps, so it is always saved.
        let exec = storage::create_execution(
            pool,
            execution_id,
            workflow.id,
            Some(workflow.version),
            &initial_context,
            Some("paused"),
            origin,
        )
        .await?;
        return Ok(RunOutcome {
            execution_id: exec.id,
            status: "paused".to_string(),
            result: None,
            error: None,
        });
    }

    let recorder = Recorder::start(
        pool,
        persistence,
        execution_id,
        workflow.id,
        Some(workflow.version),
        origin,
        &initial_context,
    )
    .await?;

    let span = tracing::info_span!(
        "execution",
        trace_id = %trace_id,
        execution_id = %recorder.execution_id(),
        workflow_id = %workflow.id,
        trigger = origin.trigger.as_str(),
    );
    let run = executor::run_workflow(&recorder, registry, &definition, initial_context, Some(trace_id))
        .instrument(span)
        .await;

    let (status, result, error) = match run {
        Ok(run) if run.waiting => ("waiting", None, None),
        Ok(run) => ("completed", run.result, None),
        Err(e) => ("failed", None, Some(e)),
    };
    Ok(RunOutcome {
        execution_id: recorder.execution_id(),
        status: status.to_string(),
        result,
        error,
    })
}
