//! What started a run, and the context it starts with.
//!
//! Every run's context carries `Trigger` (`{ type, ... }`) saying how it was started, and its
//! input under `Webhook` (`body` + `headers`) whatever started it, so a workflow written for
//! webhook calls runs unchanged when scheduled or called from another workflow. The trigger
//! type and schedule are also recorded on the execution row (see [`RunOrigin`]).

use axum::body::Bytes;
use axum::http::HeaderMap;
use chrono::{DateTime, FixedOffset};
use serde_json::{json, Value};
use uuid::Uuid;

/// How a run was started; stored in `workflow_executions.trigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `POST /webhook/:id`.
    Webhook,
    /// A schedule fired (or was run on demand).
    Schedule,
    /// A WorkflowCall node in another run.
    WorkflowCall,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Schedule => "schedule",
            Self::WorkflowCall => "workflow_call",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "webhook" => Ok(Self::Webhook),
            "schedule" => Ok(Self::Schedule),
            "workflow_call" => Ok(Self::WorkflowCall),
            other => Err(format!(
                "invalid trigger '{other}': expected webhook, schedule or workflow_call"
            )),
        }
    }
}

/// What started a run, as recorded on its execution row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOrigin {
    pub trigger: TriggerKind,
    /// The schedule that started it (`Schedule` runs only).
    pub schedule_id: Option<Uuid>,
}

impl RunOrigin {
    pub const WEBHOOK: Self = Self { trigger: TriggerKind::Webhook, schedule_id: None };
    pub const WORKFLOW_CALL: Self = Self { trigger: TriggerKind::WorkflowCall, schedule_id: None };

    pub fn schedule(schedule_id: Uuid) -> Self {
        Self { trigger: TriggerKind::Schedule, schedule_id: Some(schedule_id) }
    }
}

pub fn webhook_context_from_request(body: Bytes, headers: &HeaderMap) -> Value {
    let body_value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut headers_map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        if let Ok(s) = v.to_str() {
            headers_map.insert(k.to_string(), Value::String(s.to_string()));
        }
    }
    json!({
        "Webhook": { "body": body_value, "headers": headers_map },
        "Trigger": { "type": TriggerKind::Webhook.as_str() },
    })
}

/// Context for a run started by a schedule: its `input` becomes `Webhook.body`.
/// `scheduled_for` is the slot it fired for, in the schedule's timezone; `None` when the
/// schedule was run on demand.
pub fn schedule_context(
    input: Value,
    schedule_id: Uuid,
    schedule_name: &str,
    scheduled_for: Option<DateTime<FixedOffset>>,
) -> Value {
    json!({
        "Webhook": { "body": input, "headers": {} },
        "Trigger": {
            "type": TriggerKind::Schedule.as_str(),
            "schedule_id": schedule_id,
            "schedule_name": schedule_name,
            "scheduled_for": scheduled_for.map(|t| t.to_rfc3339()),
        },
    })
}

/// Context for a run started by a WorkflowCall node in `parent_execution_id`.
pub fn workflow_call_context(body: Value, headers: Value, parent_execution_id: Uuid) -> Value {
    json!({
        "Webhook": { "body": body, "headers": headers },
        "Trigger": {
            "type": TriggerKind::WorkflowCall.as_str(),
            "parent_execution_id": parent_execution_id,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_context_has_body_headers_and_trigger() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "abc".parse().unwrap());
        let ctx = webhook_context_from_request(Bytes::from_static(br#"{"id":7}"#), &headers);
        assert_eq!(ctx["Webhook"]["body"], json!({ "id": 7 }));
        assert_eq!(ctx["Webhook"]["headers"]["x-request-id"], "abc");
        assert_eq!(ctx["Trigger"], json!({ "type": "webhook" }));
    }

    #[test]
    fn webhook_context_keeps_null_body_for_non_json() {
        let ctx = webhook_context_from_request(Bytes::from_static(b"not json"), &HeaderMap::new());
        assert_eq!(ctx["Webhook"]["body"], Value::Null);
    }

    #[test]
    fn schedule_context_puts_input_in_webhook_body() {
        let id = Uuid::new_v4();
        let slot = DateTime::parse_from_rfc3339("2026-10-13T09:00:00+05:30").unwrap();
        let ctx = schedule_context(json!({ "region": "IN" }), id, "daily-report-in", Some(slot));
        assert_eq!(ctx["Webhook"]["body"], json!({ "region": "IN" }));
        assert_eq!(ctx["Webhook"]["headers"], json!({}));
        assert_eq!(
            ctx["Trigger"],
            json!({
                "type": "schedule",
                "schedule_id": id,
                "schedule_name": "daily-report-in",
                "scheduled_for": "2026-10-13T09:00:00+05:30",
            })
        );
    }

    #[test]
    fn on_demand_schedule_run_has_no_slot() {
        let ctx = schedule_context(json!({}), Uuid::new_v4(), "s", None);
        assert_eq!(ctx["Trigger"]["scheduled_for"], Value::Null);
    }

    #[test]
    fn trigger_kind_round_trips() {
        for kind in [TriggerKind::Webhook, TriggerKind::Schedule, TriggerKind::WorkflowCall] {
            assert_eq!(TriggerKind::parse(kind.as_str()), Ok(kind));
        }
        assert!(TriggerKind::parse("cron").is_err());
    }
}
