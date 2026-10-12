//! Schedules: when a workflow runs on its own.
//!
//! A schedule names a workflow, a cron expression and an IANA timezone. The expression is
//! read as wall-clock time in that timezone, so `0 9 * * 1-5` in `America/New_York` is 09:00
//! New York time all year round, across daylight-saving changes. Expressions have the
//! standard five fields (minute, hour, day of month, month, day of week); the finest a
//! schedule can fire is once a minute.
//!
//! Daylight-saving changes, for a time that names a fixed hour and minute:
//!   - a time skipped when clocks go forward (e.g. 02:30) runs at the first valid time after
//!     the gap (03:00), the same day;
//!   - a time repeated when clocks go back (e.g. 01:30) runs once, the first time.
//! Expressions with `*` or steps in the hour or minute fire on every real occurrence: none in
//! the skipped hour, and in both passes of the repeated hour.

use chrono::{DateTime, FixedOffset, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds};
use croner::Cron;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::persistence::Persistence;

/// A parsed cron expression in a timezone.
#[derive(Debug, Clone)]
pub struct CronSchedule {
    cron: Cron,
    tz: Tz,
}

/// One time a schedule fires: as an instant, and as wall-clock time in its timezone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    /// UTC, RFC 3339.
    pub at: DateTime<Utc>,
    /// The same instant in the schedule's timezone, with its offset, e.g.
    /// `2026-10-13T09:00:00-04:00`.
    pub local: DateTime<FixedOffset>,
}

impl CronSchedule {
    /// Parse `expression` in `timezone`, rejecting expressions that never fire.
    pub fn parse(expression: &str, timezone: &str) -> Result<Self, String> {
        let tz = parse_timezone(timezone)?;
        let expression = expression.trim();
        if expression.is_empty() {
            return Err("cron is required".to_string());
        }
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .build()
            .parse(expression)
            .map_err(|e| format!("invalid cron '{expression}': {e}"))?;
        let schedule = Self { cron, tz };
        if schedule.next_after(Utc::now()).is_none() {
            return Err(format!("cron '{expression}' never fires"));
        }
        Ok(schedule)
    }

    pub fn timezone(&self) -> Tz {
        self.tz
    }

    /// The first time strictly after `t` that the schedule fires, or `None` if it never does.
    pub fn next_after(&self, t: DateTime<Utc>) -> Option<Occurrence> {
        self.cron
            .find_next_occurrence(&t.with_timezone(&self.tz), false)
            .ok()
            .map(occurrence)
    }

    /// The latest time at or before `t` that the schedule fires, if any.
    pub fn latest_at_or_before(&self, t: DateTime<Utc>) -> Option<Occurrence> {
        self.cron
            .find_previous_occurrence(&t.with_timezone(&self.tz), true)
            .ok()
            .map(occurrence)
    }

    /// The next `count` times after `t` that the schedule fires.
    pub fn upcoming(&self, t: DateTime<Utc>, count: usize) -> Vec<Occurrence> {
        self.cron
            .iter_after(t.with_timezone(&self.tz))
            .take(count)
            .map(occurrence)
            .collect()
    }

    /// The expression in words, e.g. "At 09:00 on Monday through Friday".
    pub fn describe(&self) -> String {
        self.cron.describe()
    }
}

fn occurrence(t: DateTime<Tz>) -> Occurrence {
    Occurrence { at: t.with_timezone(&Utc), local: t.fixed_offset() }
}

/// An IANA timezone name such as `Asia/Kolkata` or `UTC`. Fixed offsets (`+05:30`) are
/// rejected: they drift by an hour whenever daylight saving changes.
pub fn parse_timezone(timezone: &str) -> Result<Tz, String> {
    let timezone = timezone.trim();
    timezone.parse::<Tz>().map_err(|_| {
        format!("unknown timezone '{timezone}': use an IANA name such as Asia/Kolkata or UTC, not an offset")
    })
}

/// Whether a slot may start while the schedule's previous run is still going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    /// Skip the slot.
    #[default]
    Skip,
    /// Start it anyway.
    Allow,
}

/// What to do about slots missed while no scheduler was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatchUp {
    /// Run once, for the latest missed slot.
    #[default]
    Once,
    /// Run none of them; wait for the next slot.
    Skip,
}

impl Overlap {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Allow => "allow",
        }
    }
}

impl CatchUp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Skip => "skip",
        }
    }
}

/// What a schedule is, as written by a person (or synced from a `.workflow.schedules` file).
/// Field names are snake_case; the camelCase names used in config files are accepted too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSpec {
    /// Unique within the tenant; how sync matches a schedule to its file entry.
    pub name: String,
    /// The workflow's name.
    #[serde(alias = "workflowName")]
    pub workflow: String,
    /// The workflow version to run. `None`: whichever is latest when the schedule fires.
    #[serde(default, alias = "workflowVersion")]
    pub version: Option<i32>,
    pub cron: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Sent to the run as `Webhook.body`.
    #[serde(default = "empty_object")]
    pub input: Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub overlap: Overlap,
    #[serde(default, alias = "catchUp")]
    pub catch_up: CatchUp,
    /// Overrides the workflow's persistence mode for scheduled runs.
    #[serde(default, alias = "persistence")]
    pub persist: Option<String>,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

fn default_true() -> bool {
    true
}

const MAX_NAME_LEN: usize = 128;

impl ScheduleSpec {
    /// Check the spec and normalize it (trimmed names and timezone); returns its parsed cron.
    pub fn validate(&mut self) -> Result<CronSchedule, String> {
        self.name = self.name.trim().to_string();
        self.workflow = self.workflow.trim().to_string();
        self.timezone = self.timezone.trim().to_string();
        self.cron = self.cron.trim().to_string();
        if self.name.is_empty() {
            return Err("name is required".to_string());
        }
        if self.name.len() > MAX_NAME_LEN
            || !self.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "invalid name '{}': use up to {MAX_NAME_LEN} letters, digits, '-', '_' or '.'",
                self.name
            ));
        }
        if self.workflow.is_empty() {
            return Err("workflow is required".to_string());
        }
        if let Some(v) = self.version {
            if v < 1 {
                return Err("version must be 1 or more".to_string());
            }
        }
        if let Some(p) = self.persist.as_deref() {
            self.persist = Some(Persistence::parse(p)?.as_str().to_string());
        }
        CronSchedule::parse(&self.cron, &self.timezone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn locals(s: &CronSchedule, after: &str, n: usize) -> Vec<String> {
        s.upcoming(utc(after), n).into_iter().map(|o| o.local.to_rfc3339()).collect()
    }

    #[test]
    fn fires_at_wall_clock_time_in_its_timezone() {
        let s = CronSchedule::parse("0 9 * * *", "Asia/Kolkata").unwrap();
        let next = s.next_after(utc("2026-10-12T00:00:00Z")).unwrap();
        assert_eq!(next.local.to_rfc3339(), "2026-10-12T09:00:00+05:30");
        assert_eq!(next.at, utc("2026-10-12T03:30:00Z"));
    }

    #[test]
    fn next_after_is_strictly_after() {
        let s = CronSchedule::parse("0 9 * * *", "UTC").unwrap();
        assert_eq!(s.next_after(utc("2026-10-12T09:00:00Z")).unwrap().at, utc("2026-10-13T09:00:00Z"));
    }

    #[test]
    fn keeps_local_time_across_daylight_saving() {
        // US clocks go back on 2026-11-01: 09:00 New York is 13:00 UTC before, 14:00 after.
        let s = CronSchedule::parse("0 9 * * *", "America/New_York").unwrap();
        let at: Vec<_> = s.upcoming(utc("2026-10-31T00:00:00Z"), 2).into_iter().map(|o| o.at).collect();
        assert_eq!(at, [utc("2026-10-31T13:00:00Z"), utc("2026-11-01T14:00:00Z")]);
    }

    #[test]
    fn skipped_time_runs_after_the_gap() {
        // US clocks go forward on 2026-03-08: 02:30 does not exist that day.
        let s = CronSchedule::parse("30 2 * * *", "America/New_York").unwrap();
        assert_eq!(
            locals(&s, "2026-03-07T12:00:00Z", 3),
            ["2026-03-08T03:00:00-04:00", "2026-03-09T02:30:00-04:00", "2026-03-10T02:30:00-04:00"]
        );
    }

    #[test]
    fn repeated_time_runs_once() {
        // US clocks go back on 2026-11-01: 01:30 happens twice that day.
        let s = CronSchedule::parse("30 1 * * *", "America/New_York").unwrap();
        assert_eq!(
            locals(&s, "2026-10-31T12:00:00Z", 2),
            ["2026-11-01T01:30:00-04:00", "2026-11-02T01:30:00-05:00"]
        );
    }

    #[test]
    fn latest_at_or_before_includes_t() {
        let s = CronSchedule::parse("0 * * * *", "UTC").unwrap();
        assert_eq!(s.latest_at_or_before(utc("2026-10-12T10:00:00Z")).unwrap().at, utc("2026-10-12T10:00:00Z"));
        assert_eq!(s.latest_at_or_before(utc("2026-10-12T10:59:00Z")).unwrap().at, utc("2026-10-12T10:00:00Z"));
    }

    #[test]
    fn rejects_offsets_and_unknown_timezones() {
        for tz in ["+05:30", "UTC+5", "Mars/Olympus", ""] {
            let err = CronSchedule::parse("0 9 * * *", tz).unwrap_err();
            assert!(err.contains("unknown timezone"), "{tz}: {err}");
        }
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(CronSchedule::parse("", "UTC").is_err());
        assert!(CronSchedule::parse("not a cron", "UTC").is_err());
        assert!(CronSchedule::parse("61 * * * *", "UTC").is_err());
        // Seconds are not allowed: schedules fire at most once a minute.
        assert!(CronSchedule::parse("*/5 * * * * *", "UTC").is_err());
        assert!(CronSchedule::parse("0 0 30 2 *", "UTC").unwrap_err().contains("never fires"));
    }

    #[test]
    fn accepts_nicknames_and_names() {
        assert!(CronSchedule::parse("@daily", "UTC").is_ok());
        assert!(CronSchedule::parse("0 9 * * MON-FRI", "Europe/London").is_ok());
    }

    #[test]
    fn spec_defaults_and_camel_case_aliases() {
        let spec: ScheduleSpec = serde_json::from_value(json!({
            "name": "daily-report-in",
            "workflowName": "daily-report",
            "cron": "0 9 * * *",
            "catchUp": "skip",
        }))
        .unwrap();
        assert_eq!(spec.workflow, "daily-report");
        assert_eq!(spec.timezone, "UTC");
        assert_eq!(spec.input, json!({}));
        assert!(spec.enabled);
        assert_eq!(spec.overlap, Overlap::Skip);
        assert_eq!(spec.catch_up, CatchUp::Skip);
        assert_eq!(spec.version, None);
    }

    #[test]
    fn spec_validation() {
        let base = || ScheduleSpec {
            name: " daily-report-in ".into(),
            workflow: "daily-report".into(),
            version: None,
            cron: "0 9 * * *".into(),
            timezone: " Asia/Kolkata ".into(),
            input: json!({}),
            enabled: true,
            overlap: Overlap::Skip,
            catch_up: CatchUp::Once,
            persist: Some("errorsOnly".into()),
        };
        let mut spec = base();
        spec.validate().unwrap();
        assert_eq!(spec.name, "daily-report-in");
        assert_eq!(spec.timezone, "Asia/Kolkata");
        assert_eq!(spec.persist.as_deref(), Some("errors_only"));

        let mut bad = base();
        bad.name = "daily report".into();
        assert!(bad.validate().unwrap_err().contains("invalid name"));
        let mut bad = base();
        bad.workflow = " ".into();
        assert!(bad.validate().is_err());
        let mut bad = base();
        bad.version = Some(0);
        assert!(bad.validate().is_err());
        let mut bad = base();
        bad.persist = Some("sometimes".into());
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_unknown_overlap_and_catch_up() {
        let r: Result<ScheduleSpec, _> =
            serde_json::from_value(json!({ "name": "a", "workflow": "w", "cron": "@daily", "overlap": "queue" }));
        assert!(r.is_err());
    }
}
