//! W3C Trace Context (`traceparent`) helpers.
//!
//! The engine sits in the middle of the call chain: callers (e.g. the service-kit
//! worker) trigger a webhook with a `traceparent` header, and the engine fans out
//! to underlying services from its nodes. This module reads the incoming trace id,
//! and builds a `traceparent` for each outbound hop so the whole chain correlates.
//!
//! - [`resolve_trace_id`] — read the incoming trace id at the webhook boundary,
//!   generating a fresh **root** when none/invalid is present (the engine
//!   originates outbound calls, so every execution should be traceable).
//! - [`format_traceparent`] — build `00-<trace-id>-<span-id>-01` for an outbound
//!   request, with a fresh span id per hop.
//!
//! Header format (W3C): `00-<32-hex trace-id>-<16-hex span-id>-<2-hex flags>`.

use uuid::Uuid;

/// Header name carrying the W3C trace context.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// Extract and validate the `trace-id` (32 hex) from a W3C `traceparent` value.
/// Returns `None` when the shape is wrong, the version is `ff` (invalid), or the
/// trace id is all zeros (also invalid).
pub fn parse_trace_id(traceparent: &str) -> Option<String> {
    let mut it = traceparent.trim().split('-');
    let version = it.next()?;
    let trace_id = it.next()?;
    let parent_id = it.next()?;
    let flags = it.next()?;
    if it.next().is_some() {
        return None; // more than 4 segments
    }
    let is_hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex(version, 2) || !is_hex(trace_id, 32) || !is_hex(parent_id, 16) || !is_hex(flags, 2) {
        return None;
    }
    if version.eq_ignore_ascii_case("ff") {
        return None;
    }
    let tid = trace_id.to_ascii_lowercase();
    if tid.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(tid)
}

/// Generate a new root trace id: a random UUID as 32 lowercase hex chars.
pub fn new_trace_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Resolve the trace id for an incoming request: continue the caller's trace when
/// a valid `traceparent` is present, else start a fresh root trace.
pub fn resolve_trace_id(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_trace_id)
        .unwrap_or_else(new_trace_id)
}

/// Build a `traceparent` header value for an outbound hop that continues `trace_id`,
/// with a fresh random span id. `flags=01` (sampled) marks the trace as recorded.
pub fn format_traceparent(trace_id: &str) -> String {
    let span_id = &Uuid::new_v4().simple().to_string()[..16];
    format!("00-{trace_id}-{span_id}-01")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_traceparent() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            parse_trace_id(tp).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[test]
    fn rejects_malformed_and_zero() {
        assert_eq!(parse_trace_id("garbage"), None);
        assert_eq!(parse_trace_id("00-abc-00f067aa0ba902b7-01"), None);
        assert_eq!(
            parse_trace_id("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            None
        );
        assert_eq!(
            parse_trace_id("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            None
        );
    }

    #[test]
    fn format_roundtrips_the_trace_id() {
        let tid = "4bf92f3577b34da6a3ce929d0e0e4736";
        let tp = format_traceparent(tid);
        assert_eq!(parse_trace_id(&tp).as_deref(), Some(tid));
    }
}
