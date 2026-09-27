//! Wait node: suspends the run for a time, until a time, or until a signal arrives.
//!
//! The wait is durable: the run is saved as `waiting` and resumed later by the timer
//! worker or by `POST /signals`, so it survives restarts and holds no request open.
//!
//! Config keys (in node `data`; `{{ }}` expressions are interpolated before this runs):
//!   - `mode`: `"duration"`, `"until"` or `"event"`. Optional: inferred from the keys below
//!     (`correlationKey` → event, `until` → until, otherwise duration).
//!   - `duration`: how long to wait, e.g. `"90s"`, `"15m"`, `"1h30m"`, `"2d"`, or a number
//!     of seconds.
//!   - `until`: when to resume: an RFC 3339 timestamp, or a number of epoch milliseconds.
//!   - `correlationKey` (event): resume when a signal with this key arrives.
//!   - `filter` (event, optional): JMESPath over `{ signal: { key, payload } }` that must be
//!     truthy for a signal to count, e.g. `signal.payload.status == 'ack'`. Write it without
//!     `{{ }}` (those parts are filled in when the wait starts).
//!   - `timeout` (event, optional): give up after this long (same format as `duration`).
//!     Without it the run waits until a signal arrives or it is cancelled.
//!   - `receivedHandle` / `timedOutHandle` (event): port names, default `"received"` and
//!     `"timed_out"`.
//!
//! Output: `{ resumedBy: "timer" | "signal", startedAt, resumedAt, ... }`. Event waits add
//! `signal` (the payload) or `timedOut: true`, and select the matching port.

use super::{ExecutionContext, NodeExecutor, NodeOutcome, ResumeReason, SuspendSpec};
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

pub struct WaitExecutor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Duration,
    Until,
    Event,
}

impl Mode {
    fn from_config(config: &Value) -> Result<Self, String> {
        match config.get("mode").and_then(Value::as_str).map(str::trim) {
            Some("duration") => Ok(Self::Duration),
            Some("until") => Ok(Self::Until),
            Some("event") => Ok(Self::Event),
            Some("") | None => Ok(if present(config, "correlationKey") {
                Self::Event
            } else if present(config, "until") {
                Self::Until
            } else {
                Self::Duration
            }),
            Some(other) => Err(format!("Wait: unknown mode '{other}': expected duration, until or event")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::Until => "until",
            Self::Event => "event",
        }
    }
}

fn present(config: &Value, key: &str) -> bool {
    match config.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn handle(config: &Value, key: &str, default: &str) -> String {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default)
        .to_string()
}

/// Parse a duration: a number of seconds, or units joined together (`"1h30m"`, `"2d 4h"`).
/// Units: `ms`, `s`, `m`, `h`, `d`, `w`.
pub(crate) fn parse_duration(value: &Value) -> Result<Duration, String> {
    let invalid = || format!("invalid duration {value}: use e.g. \"90s\", \"15m\", \"1h30m\", \"2d\" or seconds");
    match value {
        Value::Number(n) => {
            let secs = n.as_f64().filter(|s| s.is_finite() && *s >= 0.0).ok_or_else(invalid)?;
            Ok(Duration::milliseconds((secs * 1000.0).round() as i64))
        }
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(invalid());
            }
            if let Ok(secs) = s.parse::<f64>() {
                return parse_duration(&json!(secs));
            }
            let mut total = Duration::zero();
            let mut rest = s;
            while !rest.is_empty() {
                rest = rest.trim_start();
                let digits = rest.find(|c: char| !c.is_ascii_digit()).ok_or_else(invalid)?;
                if digits == 0 {
                    return Err(invalid());
                }
                let amount: i64 = rest[..digits].parse().map_err(|_| invalid())?;
                rest = &rest[digits..];
                let unit_len = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
                let part = match &rest[..unit_len] {
                    "ms" => Duration::try_milliseconds(amount),
                    "s" => Duration::try_seconds(amount),
                    "m" => Duration::try_minutes(amount),
                    "h" => Duration::try_hours(amount),
                    "d" => Duration::try_days(amount),
                    "w" => Duration::try_weeks(amount),
                    _ => None,
                };
                total = total.checked_add(&part.ok_or_else(invalid)?).ok_or_else(invalid)?;
                rest = rest[unit_len..].trim_start();
            }
            Ok(total)
        }
        _ => Err(invalid()),
    }
}

/// Parse a point in time: an RFC 3339 string, or a number of epoch milliseconds.
fn parse_timestamp(value: &Value) -> Result<DateTime<Utc>, String> {
    let invalid = || format!("invalid time {value}: use an RFC 3339 timestamp or epoch milliseconds");
    match value {
        Value::String(s) => DateTime::parse_from_rfc3339(s.trim())
            .map(|t| t.with_timezone(&Utc))
            .map_err(|_| invalid()),
        Value::Number(n) => n
            .as_i64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
            .ok_or_else(invalid),
        _ => Err(invalid()),
    }
}

fn required<'a>(config: &'a Value, key: &str, mode: Mode) -> Result<&'a Value, String> {
    config
        .get(key)
        .filter(|_| present(config, key))
        .ok_or_else(|| format!("Wait ({}) needs `{key}`", mode.as_str()))
}

#[async_trait]
impl NodeExecutor for WaitExecutor {
    async fn run(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        _input: Value,
        config: Value,
    ) -> Result<NodeOutcome, String> {
        let mode = Mode::from_config(&config)?;
        let now = Utc::now();
        let mut spec = SuspendSpec::default();
        match mode {
            Mode::Duration => {
                let d = parse_duration(required(&config, "duration", mode)?).map_err(|e| format!("Wait: {e}"))?;
                spec.wake_at = Some(now + d);
            }
            Mode::Until => {
                spec.wake_at = Some(parse_timestamp(required(&config, "until", mode)?).map_err(|e| format!("Wait: {e}"))?);
            }
            Mode::Event => {
                let key = match required(&config, "correlationKey", mode)? {
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string(),
                };
                spec.correlation_key = Some(key);
                spec.filter = config
                    .get("filter")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if present(&config, "timeout") {
                    let d = parse_duration(&config["timeout"]).map_err(|e| format!("Wait timeout: {e}"))?;
                    spec.wake_at = Some(now + d);
                }
            }
        }
        spec.state = json!({
            "mode": mode.as_str(),
            "startedAt": now,
            "wakeAt": spec.wake_at,
            "correlationKey": spec.correlation_key,
            "receivedHandle": handle(&config, "receivedHandle", "received"),
            "timedOutHandle": handle(&config, "timedOutHandle", "timed_out"),
        });
        // A time already passed (a zero duration, or `until` in the past) does not wait.
        if spec.correlation_key.is_none() && spec.wake_at.is_some_and(|t| t <= now) {
            return Ok(NodeOutcome::Complete(resumed_output(&spec.state, ResumeReason::Timer)));
        }
        Ok(NodeOutcome::Suspend(spec))
    }

    async fn resume(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        state: Value,
        reason: ResumeReason,
    ) -> Result<NodeOutcome, String> {
        Ok(NodeOutcome::Complete(resumed_output(&state, reason)))
    }
}

fn resumed_output(state: &Value, reason: ResumeReason) -> Value {
    let mut out = json!({
        "startedAt": state["startedAt"],
        "resumedAt": Utc::now(),
    });
    let event = state["mode"] == "event";
    let str_of = |key: &str| state[key].as_str().unwrap_or_default().to_string();
    match reason {
        ResumeReason::Timer => {
            out["resumedBy"] = json!("timer");
            if event {
                out["timedOut"] = json!(true);
                out["selectedHandles"] = json!([str_of("timedOutHandle")]);
            } else {
                out["wakeAt"] = state["wakeAt"].clone();
            }
        }
        ResumeReason::Signal(payload) => {
            out["resumedBy"] = json!("signal");
            out["signal"] = payload;
            out["selectedHandles"] = json!([str_of("receivedHandle")]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}))
    }

    async fn run(config: Value) -> Result<NodeOutcome, String> {
        WaitExecutor.run(&ctx(), "wait", json!({}), config).await
    }

    #[test]
    fn parses_durations() {
        let secs = |v: Value| parse_duration(&v).unwrap().num_milliseconds() as f64 / 1000.0;
        assert_eq!(secs(json!("90s")), 90.0);
        assert_eq!(secs(json!("15m")), 900.0);
        assert_eq!(secs(json!("1h30m")), 5400.0);
        assert_eq!(secs(json!("2d 4h")), 187200.0);
        assert_eq!(secs(json!("1w")), 604800.0);
        assert_eq!(secs(json!("250ms")), 0.25);
        assert_eq!(secs(json!(45)), 45.0);
        assert_eq!(secs(json!("1.5")), 1.5);
        for bad in [json!(""), json!("abc"), json!("5x"), json!("m5"), json!(-1), json!(true)] {
            assert!(parse_duration(&bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn parses_timestamps() {
        let t = parse_timestamp(&json!("2026-01-02T03:04:05Z")).unwrap();
        assert_eq!(t.timestamp(), 1767323045);
        assert_eq!(parse_timestamp(&json!(1767323045000i64)).unwrap(), t);
        assert!(parse_timestamp(&json!("tomorrow")).is_err());
    }

    #[test]
    fn infers_mode() {
        assert_eq!(Mode::from_config(&json!({ "duration": "1m" })).unwrap(), Mode::Duration);
        assert_eq!(Mode::from_config(&json!({ "until": "2026-01-01T00:00:00Z" })).unwrap(), Mode::Until);
        assert_eq!(Mode::from_config(&json!({ "correlationKey": "k", "timeout": "1h" })).unwrap(), Mode::Event);
        assert!(Mode::from_config(&json!({ "mode": "forever" })).is_err());
    }

    #[tokio::test]
    async fn duration_suspends_with_a_timer() {
        let NodeOutcome::Suspend(spec) = run(json!({ "duration": "10m" })).await.unwrap() else {
            panic!("expected suspend");
        };
        let left = spec.wake_at.unwrap() - Utc::now();
        assert!(left > Duration::minutes(9) && left <= Duration::minutes(10));
        assert!(spec.correlation_key.is_none());
    }

    #[tokio::test]
    async fn past_time_completes_without_waiting() {
        let out = run(json!({ "until": "2000-01-01T00:00:00Z" })).await.unwrap();
        let NodeOutcome::Complete(out) = out else { panic!("expected complete") };
        assert_eq!(out["resumedBy"], "timer");
        assert!(out.get("selectedHandles").is_none());
        assert!(matches!(run(json!({ "duration": 0 })).await.unwrap(), NodeOutcome::Complete(_)));
    }

    #[tokio::test]
    async fn event_wait_selects_received_or_timed_out() {
        let config = json!({
            "correlationKey": "ticket-7",
            "filter": "signal.payload.status == 'ack'",
            "timeout": "30m",
            "timedOutHandle": "escalate"
        });
        let NodeOutcome::Suspend(spec) = run(config).await.unwrap() else { panic!("expected suspend") };
        assert_eq!(spec.correlation_key.as_deref(), Some("ticket-7"));
        assert_eq!(spec.filter.as_deref(), Some("signal.payload.status == 'ack'"));
        assert!(spec.wake_at.is_some());

        let ctx = ctx();
        let resume = |reason| WaitExecutor.resume(&ctx, "wait", spec.state.clone(), reason);
        let NodeOutcome::Complete(got) = resume(ResumeReason::Signal(json!({ "status": "ack" }))).await.unwrap() else {
            panic!()
        };
        assert_eq!(got["selectedHandles"], json!(["received"]));
        assert_eq!(got["signal"]["status"], "ack");

        let NodeOutcome::Complete(got) = resume(ResumeReason::Timer).await.unwrap() else { panic!() };
        assert_eq!(got["selectedHandles"], json!(["escalate"]));
        assert_eq!(got["timedOut"], true);
    }

    #[tokio::test]
    async fn event_wait_without_timeout_has_no_timer() {
        let NodeOutcome::Suspend(spec) = run(json!({ "mode": "event", "correlationKey": 42 })).await.unwrap() else {
            panic!()
        };
        assert_eq!(spec.correlation_key.as_deref(), Some("42"));
        assert!(spec.wake_at.is_none());
        assert!(run(json!({ "mode": "event" })).await.unwrap_err().contains("correlationKey"));
    }
}
