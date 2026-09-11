//! Switch node: multi-way conditional branch.
//!
//! Evaluates a list of cases against a subject `value` and activates the matching
//! output port(s). In the default `first` mode only the first matching case's handle is
//! activated; in `all` mode every matching case is. When nothing matches, the `default`
//! handle is activated. The executor reads the returned `selectedHandles` to route the
//! flow and skip branches that were not taken.
//!
//! Config keys (in node `data`; `{{ }}` expressions are interpolated before this runs):
//!   - `value`: the subject to test (optional when every case uses `condition`).
//!   - `cases`: array of case objects, each one of:
//!       * `{ handle, value }`            — matches when `value == case.value`,
//!       * `{ handle, operator, value }`  — matches when `compare(value, operator, case.value)`,
//!       * `{ handle, condition }`        — matches when `condition` is truthy.
//!     `handle` names the output port; when omitted it falls back to the case `value`
//!     (stringified) or `case_<index>`.
//!   - `mode`: `"first"` (default) or `"all"`.
//!   - `default` / `defaultHandle`: port used when no case matches (default `"default"`).
//!
//! Operators are documented in [`super::condition`].

use super::condition;
use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct SwitchExecutor;

/// The port name for a case: explicit `handle`, else the case `value` as a string,
/// else a positional `case_<index>`.
fn case_handle(case: &Value, index: usize) -> String {
    if let Some(h) = case.get("handle").and_then(Value::as_str) {
        return h.to_string();
    }
    match case.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => format!("case_{index}"),
    }
}

/// Whether a single case matches the subject `value`.
fn case_matches(case: &Value, value: &Value) -> Result<bool, String> {
    if let Some(op) = case.get("operator").and_then(Value::as_str) {
        let right = case.get("value").cloned().unwrap_or(Value::Null);
        return condition::compare(value, op, &right);
    }
    if let Some(cond) = case.get("condition") {
        return Ok(condition::truthy(cond));
    }
    match case.get("value") {
        Some(expected) => condition::compare(value, "eq", expected),
        None => Err("Switch: each case needs `value`, `operator`+`value`, or `condition`".to_string()),
    }
}

#[async_trait]
impl NodeExecutor for SwitchExecutor {
    async fn execute(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        input: Value,
        config: Value,
    ) -> Result<Value, String> {
        let value = config
            .get("value")
            .or_else(|| input.get("value"))
            .cloned()
            .unwrap_or(Value::Null);

        let cases = config
            .get("cases")
            .or_else(|| input.get("cases"))
            .and_then(Value::as_array)
            .ok_or("Switch: `cases` must be an array")?;

        let all_mode = config.get("mode").and_then(Value::as_str) == Some("all");

        let mut matched: Vec<String> = Vec::new();
        for (i, case) in cases.iter().enumerate() {
            if case_matches(case, &value)? {
                matched.push(case_handle(case, i));
                if !all_mode {
                    break;
                }
            }
        }

        if matched.is_empty() {
            let default_handle = config
                .get("defaultHandle")
                .or_else(|| config.get("default"))
                .and_then(Value::as_str)
                .unwrap_or("default");
            matched.push(default_handle.to_string());
        }

        Ok(json!({ "matched": matched.clone(), "selectedHandles": matched }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(Uuid::nil(), Uuid::nil(), serde_json::json!({}))
    }

    #[tokio::test]
    async fn first_match_wins_by_default() {
        let config = json!({
            "value": "gold",
            "cases": [
                { "value": "silver", "handle": "s" },
                { "value": "gold", "handle": "g" },
                { "value": "gold", "handle": "g2" }
            ]
        });
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["g"]));
    }

    #[tokio::test]
    async fn all_mode_collects_every_match() {
        let config = json!({
            "value": 5,
            "mode": "all",
            "cases": [
                { "operator": "gt", "value": 1, "handle": "gt1" },
                { "operator": "lt", "value": 10, "handle": "lt10" },
                { "operator": "gt", "value": 100, "handle": "gt100" }
            ]
        });
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["gt1", "lt10"]));
    }

    #[tokio::test]
    async fn falls_back_to_default_handle() {
        let config = json!({
            "value": "bronze",
            "default": "none",
            "cases": [ { "value": "gold", "handle": "g" } ]
        });
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["none"]));
    }

    #[tokio::test]
    async fn handle_defaults_to_case_value() {
        let config = json!({
            "value": "gold",
            "cases": [ { "value": "gold" } ]
        });
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["gold"]));
    }

    #[tokio::test]
    async fn condition_based_cases() {
        let config = json!({
            "cases": [
                { "condition": false, "handle": "a" },
                { "condition": true, "handle": "b" }
            ]
        });
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["b"]));
    }

    #[tokio::test]
    async fn missing_cases_errors() {
        let out = SwitchExecutor.execute(&ctx(), "n1", Value::Null, json!({ "value": 1 })).await;
        assert!(out.is_err());
    }
}
