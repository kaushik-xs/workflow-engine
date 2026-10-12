//! Runs handed to workers instead of run in the request that started them.
//!
//! [`enqueue`] accepts a run and returns at once with the execution id it will have; a
//! [`RunWorker`] in any process with the `worker` role later takes it from
//! `workflow_run_queue` and runs it through [`runs::start_run`]. Workers claim runs with
//! `FOR UPDATE SKIP LOCKED`, so any number of them can share the queue and each run is taken
//! once.
//!
//! A run is removed from the queue when it is claimed, before it starts. If the worker dies
//! before the run's execution is recorded, the run is lost rather than started twice.

use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::error::AppError;
use crate::persistence::Persistence;
use crate::registry::NodeRegistry;
use crate::runs::{self, RunOutcome, StartRun};
use crate::storage::{self, QueuedRun, Workflow};
use crate::triggers::{RunOrigin, TriggerKind};

/// Queue a run of `workflow` to start at `available_at` (now, when `None`).
pub async fn enqueue(
    pool: &sqlx::PgPool,
    workflow: &Workflow,
    start: StartRun,
    available_at: Option<DateTime<Utc>>,
) -> Result<RunOutcome, AppError> {
    if start.step_mode {
        return Err(AppError::BadRequest("a step-mode run cannot be queued".into()));
    }
    let now = Utc::now();
    let run = QueuedRun {
        id: start.execution_id.unwrap_or_else(Uuid::new_v4),
        workflow_id: workflow.id,
        trigger_context: start.trigger_context,
        trigger: start.origin.trigger.as_str().to_string(),
        schedule_id: start.origin.schedule_id,
        persistence: start.persistence.map(|p| p.as_str().to_string()),
        trace_id: start.trace_id,
        available_at: available_at.unwrap_or(now),
        enqueued_at: now,
    };
    storage::enqueue_run(pool, &run).await?;
    tracing::info!(
        execution_id = %run.id,
        workflow_id = %workflow.id,
        trigger = %run.trigger,
        trace_id = %run.trace_id,
        "run queued"
    );
    Ok(RunOutcome {
        execution_id: run.id,
        status: "queued".to_string(),
        result: None,
        error: None,
    })
}

/// Takes queued runs and runs them, at most `concurrency` at a time in this process.
pub struct RunWorker {
    pool: sqlx::PgPool,
    registry: Arc<dyn NodeRegistry>,
    permits: Arc<Semaphore>,
}

impl RunWorker {
    pub fn new(pool: sqlx::PgPool, registry: Arc<dyn NodeRegistry>, concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        })
    }

    /// Take runs from the queue forever: as soon as there is room for one, claim as many as
    /// there is room for. When the queue is empty, look again after `idle_interval`.
    pub fn spawn(self: &Arc<Self>, idle_interval: Duration) -> tokio::task::JoinHandle<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            loop {
                match worker.run_due().await {
                    Ok(0) => tokio::time::sleep(idle_interval).await,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "run worker: failed to poll the run queue");
                        tokio::time::sleep(idle_interval).await;
                    }
                }
            }
        })
    }

    /// Wait for room to run at least one run, then claim as many due runs as there is room
    /// for and start them. Returns how many were started.
    pub async fn run_due(self: &Arc<Self>) -> Result<usize, sqlx::Error> {
        let Ok(first) = self.permits.clone().acquire_owned().await else {
            return Ok(0);
        };
        // Only this loop takes permits, so the ones available now are still free below.
        let room = 1 + self.permits.available_permits();
        let runs = storage::claim_queued_runs(&self.pool, room as i64).await?;
        let started = runs.len();
        let mut first = Some(first);
        for run in runs {
            let permit = match first.take() {
                Some(permit) => permit,
                None => match self.permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    // Cannot happen (see above); wait for one rather than overrun.
                    Err(_) => self.permits.clone().acquire_owned().await.expect("semaphore is never closed"),
                },
            };
            self.spawn_run(run, permit);
        }
        Ok(started)
    }

    fn spawn_run(self: &Arc<Self>, run: QueuedRun, permit: OwnedSemaphorePermit) {
        let worker = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let execution_id = run.id;
            let schedule_id = run.schedule_id;
            let waiting = match worker.run(run).await {
                Ok(outcome) => {
                    tracing::info!(execution_id = %execution_id, status = %outcome.status, "queued run finished");
                    outcome.status == "waiting"
                }
                Err(e) => {
                    tracing::error!(execution_id = %execution_id, error = %e, "queued run could not start");
                    false
                }
            };
            // Let the schedule's next slot run (see `scheduler`'s overlap rule).
            if let Some(schedule_id) = schedule_id {
                if let Err(e) = storage::release_schedule_run(&worker.pool, schedule_id, execution_id, waiting).await {
                    tracing::error!(execution_id = %execution_id, error = %e, "failed to release the schedule's run");
                }
            }
        });
    }

    async fn run(&self, run: QueuedRun) -> Result<RunOutcome, String> {
        let workflow = storage::get_workflow_by_id(&self.pool, run.workflow_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("workflow not found")?;
        let origin = RunOrigin {
            trigger: TriggerKind::parse(&run.trigger)?,
            schedule_id: run.schedule_id,
        };
        let persistence = run.persistence.as_deref().map(Persistence::parse).transpose()?;
        tracing::debug!(
            execution_id = %run.id,
            queued_for_ms = (Utc::now() - run.available_at).num_milliseconds(),
            "starting queued run"
        );
        runs::start_run(
            &self.pool,
            self.registry.clone(),
            &workflow,
            StartRun {
                execution_id: Some(run.id),
                trigger_context: run.trigger_context,
                origin,
                step_mode: false,
                persistence,
                trace_id: run.trace_id,
            },
        )
        .await
        .map_err(|e| e.to_string())
    }
}
