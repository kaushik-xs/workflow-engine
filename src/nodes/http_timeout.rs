//! Per-request timeout shared by the outbound HTTP nodes (`HttpRequest`, `ServiceCall`).
//!
//! The optional `timeout` config field takes the same durations as `Wait` and
//! `BrowserAutomation` (a number of seconds, or e.g. `"90s"`, `"5m"`, `"1h"`) and accepts
//! `{{ }}` expressions. It covers the whole call, from connecting until the response body has
//! been read. Absent or empty means [`DEFAULT_TIMEOUT_SECS`]; at most [`MAX_TIMEOUT_SECS`],
//! since a run holds its worker for as long as the call takes.

use serde_json::Value;
use std::time::Duration;

use super::wait::parse_duration;

/// Timeout when the node sets none.
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Longest timeout a node may set.
pub(super) const MAX_TIMEOUT_SECS: u64 = 3600;

/// The node's `timeout`, or the default. `node` names the node type in errors.
pub(super) fn request_timeout(config: &Value, node: &str) -> Result<Duration, String> {
    let value = match config.get("timeout") {
        None | Some(Value::Null) => return Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
        Some(Value::String(s)) if s.trim().is_empty() => {
            return Ok(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        }
        Some(v) => v,
    };
    let parsed = parse_duration(value)
        .map_err(|e| format!("{node} `timeout`: {e}"))?
        .to_std()
        .map_err(|_| format!("{node} `timeout` must be positive"))?;
    if parsed.is_zero() {
        return Err(format!("{node} `timeout` must be positive"));
    }
    if parsed > Duration::from_secs(MAX_TIMEOUT_SECS) {
        return Err(format!(
            "{node} `timeout` {} is longer than the maximum of {}",
            describe(parsed),
            describe(Duration::from_secs(MAX_TIMEOUT_SECS))
        ));
    }
    Ok(parsed)
}

/// The error for a failed call: says so plainly when the timeout was what ended it.
pub(super) fn call_error(e: reqwest::Error, timeout: Duration, node: &str) -> String {
    if e.is_timeout() {
        format!(
            "{node} timed out after {} (set `timeout` to allow longer)",
            describe(timeout)
        )
    } else {
        e.to_string()
    }
}

/// `1500ms`, `90s`, `2m`, `1h` — the shortest exact form.
pub(super) fn describe(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let secs = d.as_secs();
    match secs {
        s if s.is_multiple_of(3600) => format!("{}h", s / 3600),
        s if s.is_multiple_of(60) => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Result<Duration, String> {
        request_timeout(&json!({ "timeout": v }), "HttpRequest")
    }

    #[test]
    fn absent_or_empty_uses_the_default() {
        let default = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        assert_eq!(request_timeout(&json!({}), "HttpRequest").unwrap(), default);
        assert_eq!(parse(Value::Null).unwrap(), default);
        assert_eq!(parse(json!("  ")).unwrap(), default);
    }

    #[test]
    fn accepts_seconds_and_duration_strings() {
        assert_eq!(parse(json!(90)).unwrap(), Duration::from_secs(90));
        assert_eq!(parse(json!("120")).unwrap(), Duration::from_secs(120));
        assert_eq!(parse(json!("5m")).unwrap(), Duration::from_secs(300));
        assert_eq!(parse(json!("1m30s")).unwrap(), Duration::from_secs(90));
        assert_eq!(parse(json!("250ms")).unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn rejects_zero_too_long_and_garbage() {
        assert!(parse(json!(0)).unwrap_err().contains("must be positive"));
        assert!(parse(json!("2h")).unwrap_err().contains("maximum of 1h"));
        let err = parse(json!("soon")).unwrap_err();
        assert!(
            err.starts_with("HttpRequest `timeout`: invalid duration"),
            "{err}"
        );
    }

    #[test]
    fn describes_durations_compactly() {
        assert_eq!(describe(Duration::from_millis(1500)), "1500ms");
        assert_eq!(describe(Duration::from_secs(90)), "90s");
        assert_eq!(describe(Duration::from_secs(120)), "2m");
        assert_eq!(describe(Duration::from_secs(3600)), "1h");
    }
}
