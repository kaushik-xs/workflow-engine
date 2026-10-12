//! Firing schedules: the `scheduler` role.
//!
//! A scheduler polls for enabled schedules whose `next_run_at` has passed. For each one it
//! decides what the slot does ([`decide`]), queues a run for a worker when it fires, and moves
//! the schedule to its next slot, all in one transaction. Schedules are locked with
//! `FOR UPDATE SKIP LOCKED`, so any number of schedulers can share the database and each slot
//! is handled once. The run is queued, not run here: firing stays cheap and on time however
//! long the runs take, and workers absorb bursts (every schedule due at 09:00).
//!
//! A slot fires at most once: the run is queued in the same transaction that moves the
//! schedule on, and a queued run is started at most once (see [`crate::run_queue`]).
//!
//! When a slot comes due:
//!   - **Missed** (due more than the misfire grace ago, e.g. no scheduler was running): with
//!     `catch_up = once`, one run for the latest missed slot; with `skip`, none.
//!   - **Overlap**: with `overlap = skip`, the slot is skipped while the schedule's previous
//!     run is queued, waiting or paused, or started less than the run lease ago. (A run that
//!     goes on past the lease no longer blocks the next slot; the lease also frees a schedule
//!     whose run was lost with its worker.)
//!   - Otherwise the slot is queued, with `Trigger.scheduled_for` set to it.
//!
//! Either way the schedule moves to its first slot after now, and its `last_*` fields record
//! what happened.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::time::Duration;
use tracing::Instrument;
use uuid::Uuid;

use crate::schedule::{CatchUp, CronSchedule, Occurrence, Overlap};
use crate::storage::{self, DueSchedule, QueuedRun, ScheduleFired};
use crate::trace;
use crate::triggers::{self, TriggerKind};

/// What a due slot does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The slot this handles: the one that was due, or the latest one missed.
    pub slot: Occurrence,
    pub action: Action,
    /// The schedule's first slot after now.
    pub next: Option<Occurrence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Queue a run for `slot`.
    Queue,
    /// The previous run is still going.
    SkipOverlap,
    /// The slot was missed and the schedule does not catch up.
    SkipMissed,
}

impl Action {
    pub fn outcome(self) -> &'static str {
        match self {
            Self::Queue => "queued",
            Self::SkipOverlap => "skipped_overlap",
            Self::SkipMissed => "skipped_missed",
        }
    }
}

/// Decide what a schedule due at `due` does at `now`. `busy`: its previous run is still going.
pub fn decide(
    cron: &CronSchedule,
    due: DateTime<Utc>,
    now: DateTime<Utc>,
    catch_up: CatchUp,
    overlap: Overlap,
    busy: bool,
    misfire_grace: ChronoDuration,
) -> Decision {
    let next = cron.next_after(now);
    let due_slot = Occurrence { at: due, local: due.with_timezone(&cron.timezone()).fixed_offset() };
    let missed = now - due > misfire_grace;
    let (slot, action) = if missed {
        // Catch up on the latest slot that has passed, not the first one missed.
        let latest = cron.latest_at_or_before(now).filter(|o| o.at >= due).unwrap_or(due_slot);
        match catch_up {
            CatchUp::Skip => (latest, Action::SkipMissed),
            CatchUp::Once => (latest, Action::Queue),
        }
    } else {
        (due_slot, Action::Queue)
    };
    let action = if action == Action::Queue && overlap == Overlap::Skip && busy {
        Action::SkipOverlap
    } else {
        action
    };
    Decision { slot, action, next }
}

/// Fires due schedules.
pub struct Scheduler {
    pool: sqlx::PgPool,
    misfire_grace: ChronoDuration,
    run_lease: ChronoDuration,
    batch: i64,
}

impl Scheduler {
    pub fn new(pool: sqlx::PgPool, misfire_grace: Duration, run_lease: Duration, batch: usize) -> Self {
        Self {
            pool,
            misfire_grace: ChronoDuration::from_std(misfire_grace).unwrap_or(ChronoDuration::seconds(60)),
            run_lease: ChronoDuration::from_std(run_lease).unwrap_or(ChronoDuration::hours(1)),
            batch: batch.max(1) as i64,
        }
    }

    /// Fire due schedules, then look again every `interval`, forever.
    pub fn spawn(self, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Keep going while whole batches come back: more may be due.
                loop {
                    match self.fire_due().await {
                        Ok(n) if n as i64 == self.batch => continue,
                        Ok(_) => break,
                        Err(e) => {
                            tracing::error!(error = %e, "scheduler: failed to fire due schedules");
                            break;
                        }
                    }
                }
            }
        })
    }

    /// Claim one batch of due schedules and handle each. Returns how many were handled.
    pub async fn fire_due(&self) -> Result<usize, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let due = storage::claim_due_schedules(&mut tx, self.batch).await?;
        let handled = due.len();
        for schedule in due {
            self.fire(&mut tx, schedule).await?;
        }
        tx.commit().await?;
        Ok(handled)
    }

    async fn fire(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        due: DueSchedule,
    ) -> Result<(), sqlx::Error> {
        let span = tracing::info_span!("schedule", schedule = %due.schedule.name, tenant = %due.schedule.tenant);
        self.fire_in_span(tx, due).instrument(span).await
    }

    async fn fire_in_span(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        DueSchedule { schedule, busy }: DueSchedule,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        // Claimed rows are enabled with a next run time.
        let due = schedule.next_run_at.unwrap_or(now);

        let cron = match CronSchedule::parse(&schedule.cron, &schedule.timezone) {
            Ok(cron) => cron,
            Err(e) => {
                // Stop it until it is saved again with a valid cron and timezone.
                tracing::error!(error = %e, "schedule cannot be evaluated; it will not fire again until it is fixed");
                let fired = ScheduleFired {
                    next_run_at: None,
                    scheduled_for: due,
                    outcome: "failed_to_start",
                    error: Some(&e),
                    queued: None,
                };
                return storage::record_schedule_fired(tx, schedule.id, &fired).await;
            }
        };
        let spec = schedule.spec();
        let decision = decide(&cron, due, now, spec.catch_up, spec.overlap, busy, self.misfire_grace);
        let next_run_at = decision.next.as_ref().map(|o| o.at);

        if decision.action != Action::Queue {
            tracing::info!(
                slot = %decision.slot.local.to_rfc3339(),
                outcome = decision.action.outcome(),
                "schedule slot skipped"
            );
            let fired = ScheduleFired {
                next_run_at,
                scheduled_for: decision.slot.at,
                outcome: decision.action.outcome(),
                error: None,
                queued: None,
            };
            return storage::record_schedule_fired(tx, schedule.id, &fired).await;
        }

        let workflow = storage::get_workflow_by_name(
            &self.pool,
            &schedule.workflow_name,
            Some(&schedule.tenant),
            schedule.workflow_version,
        )
        .await?;
        let Some(workflow) = workflow else {
            let error = match schedule.workflow_version {
                Some(v) => format!("workflow '{}' version {v} not found", schedule.workflow_name),
                None => format!("workflow '{}' not found", schedule.workflow_name),
            };
            tracing::warn!(error = %error, "schedule slot could not start");
            let fired = ScheduleFired {
                next_run_at,
                scheduled_for: decision.slot.at,
                outcome: "failed_to_start",
                error: Some(&error),
                queued: None,
            };
            return storage::record_schedule_fired(tx, schedule.id, &fired).await;
        };

        let run = QueuedRun {
            id: Uuid::new_v4(),
            workflow_id: workflow.id,
            trigger_context: triggers::schedule_context(
                schedule.input.clone(),
                schedule.id,
                &schedule.name,
                Some(decision.slot.local),
            ),
            trigger: TriggerKind::Schedule.as_str().to_string(),
            schedule_id: Some(schedule.id),
            persistence: schedule.persistence.clone(),
            trace_id: trace::new_trace_id(),
            available_at: now,
            enqueued_at: now,
        };
        storage::enqueue_run(&mut **tx, &run).await?;
        let fired = ScheduleFired {
            next_run_at,
            scheduled_for: decision.slot.at,
            outcome: "queued",
            error: None,
            queued: Some((run.id, now + self.run_lease)),
        };
        storage::record_schedule_fired(tx, schedule.id, &fired).await?;
        tracing::info!(
            slot = %decision.slot.local.to_rfc3339(),
            execution_id = %run.id,
            workflow_id = %workflow.id,
            trace_id = %run.trace_id,
            late_ms = (now - decision.slot.at).num_milliseconds(),
            "schedule slot queued"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn grace() -> ChronoDuration {
        ChronoDuration::seconds(60)
    }

    fn hourly() -> CronSchedule {
        CronSchedule::parse("0 * * * *", "UTC").unwrap()
    }

    #[test]
    fn on_time_slot_is_queued_and_moves_on() {
        let d = decide(
            &hourly(),
            utc("2026-10-12T10:00:00Z"),
            utc("2026-10-12T10:00:02Z"),
            CatchUp::Once,
            Overlap::Skip,
            false,
            grace(),
        );
        assert_eq!(d.action, Action::Queue);
        assert_eq!(d.slot.at, utc("2026-10-12T10:00:00Z"));
        assert_eq!(d.next.unwrap().at, utc("2026-10-12T11:00:00Z"));
    }

    #[test]
    fn slot_keeps_local_time_with_offset() {
        let cron = CronSchedule::parse("0 9 * * *", "Asia/Kolkata").unwrap();
        let d = decide(
            &cron,
            utc("2026-10-12T03:30:00Z"),
            utc("2026-10-12T03:30:01Z"),
            CatchUp::Once,
            Overlap::Skip,
            false,
            grace(),
        );
        assert_eq!(d.slot.local.to_rfc3339(), "2026-10-12T09:00:00+05:30");
    }

    #[test]
    fn busy_skips_unless_overlap_allowed() {
        let (due, now) = (utc("2026-10-12T10:00:00Z"), utc("2026-10-12T10:00:01Z"));
        let skip = decide(&hourly(), due, now, CatchUp::Once, Overlap::Skip, true, grace());
        assert_eq!(skip.action, Action::SkipOverlap);
        assert_eq!(skip.next.unwrap().at, utc("2026-10-12T11:00:00Z"));
        let allow = decide(&hourly(), due, now, CatchUp::Once, Overlap::Allow, true, grace());
        assert_eq!(allow.action, Action::Queue);
    }

    #[test]
    fn missed_slots_catch_up_once_on_the_latest() {
        // Due at 10:00, scheduler back at 13:20: one run, for 13:00; next is 14:00.
        let d = decide(
            &hourly(),
            utc("2026-10-12T10:00:00Z"),
            utc("2026-10-12T13:20:00Z"),
            CatchUp::Once,
            Overlap::Skip,
            false,
            grace(),
        );
        assert_eq!(d.action, Action::Queue);
        assert_eq!(d.slot.at, utc("2026-10-12T13:00:00Z"));
        assert_eq!(d.next.unwrap().at, utc("2026-10-12T14:00:00Z"));
    }

    #[test]
    fn missed_slots_skipped_when_not_catching_up() {
        let d = decide(
            &hourly(),
            utc("2026-10-12T10:00:00Z"),
            utc("2026-10-12T13:20:00Z"),
            CatchUp::Skip,
            Overlap::Skip,
            false,
            grace(),
        );
        assert_eq!(d.action, Action::SkipMissed);
        assert_eq!(d.next.unwrap().at, utc("2026-10-12T14:00:00Z"));
    }

    #[test]
    fn late_within_grace_is_not_missed() {
        let d = decide(
            &hourly(),
            utc("2026-10-12T10:00:00Z"),
            utc("2026-10-12T10:00:59Z"),
            CatchUp::Skip,
            Overlap::Skip,
            false,
            grace(),
        );
        assert_eq!(d.action, Action::Queue);
        assert_eq!(d.slot.at, utc("2026-10-12T10:00:00Z"));
    }

    #[test]
    fn catching_up_while_busy_still_skips() {
        let d = decide(
            &hourly(),
            utc("2026-10-12T10:00:00Z"),
            utc("2026-10-12T13:20:00Z"),
            CatchUp::Once,
            Overlap::Skip,
            true,
            grace(),
        );
        assert_eq!(d.action, Action::SkipOverlap);
    }
}
