//! Process-wide logging setup.
//!
//! JSON by default: every event is emitted as one JSON line, and span fields —
//! notably `trace_id` from the execution span — are flattened onto each line, so
//! **every log carries the trace id** with no change at the call site.
//!
//! Override the format with `LOG_FORMAT` for local development:
//! - unset / `json` → structured JSON (default)
//! - `text`         → the compact single-line human format
//! - `pretty`       → the multi-line human format
//!
//! Level filtering is unchanged: driven by the passed directive (from `RUST_LOG`).

/// Initialize the global subscriber. `default_filter` is the `EnvFilter` directive
/// used when `RUST_LOG` is not set. Call once, early in `main`.
pub fn init(default_filter: &str) {
    let default_filter = default_filter.to_string();
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter.clone()))
    };

    match std::env::var("LOG_FORMAT").ok().as_deref() {
        Some("text") => {
            tracing_subscriber::fmt().with_env_filter(filter()).init();
        }
        Some("pretty") => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(filter())
                .init();
        }
        // Default: JSON. `flatten_event` puts event fields at the top level;
        // `with_current_span` + `with_span_list` include the enclosing spans'
        // fields (so `trace_id` rides along on every line).
        _ => {
            tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_env_filter(filter())
                .init();
        }
    }
}
