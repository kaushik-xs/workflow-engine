//! If node: two-way conditional branch.
//!
//! Evaluates a single condition and activates exactly one output port — `true` or
//! `false` (override the labels with `trueHandle` / `falseHandle`). The executor reads
//! the returned `selectedHandles` to activate only the matching outgoing edges and to
//! skip the branch that was not taken.
//!
//! Config keys (in node `data`; `{{ }}` expressions are interpolated before this runs):
//!   - `condition`: either a structured `{ left, operator, right }`, or any value whose
//!     truthiness decides the branch (e.g. `{{ current.body.ok }}`), or
//!   - top-level `left` + `operator` + `right`.
//!   - `trueHandle` / `falseHandle`: optional port-name overrides (default `"true"`/`"false"`).
//!
//! Operators are documented in [`super::condition`].

use super::condition;
use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct IfExecutor;

/// Resolve the boolean outcome from an already-interpolated `config`/`input`,
/// together with a JSON description of exactly what was evaluated so the
/// execution trace can show the operands, not just the result.
///
/// Precedence: a structured `condition` object with an `operator`, then a top-level
/// `operator`, then the plain truthiness of `condition`.
fn evaluate(config: &Value, input: &Value) -> Result<(bool, Value), String> {
    let condition = config.get("condition").or_else(|| input.get("condition"));

    if let Some(cond) = condition {
        if let Some(op) = cond.get("operator").and_then(Value::as_str) {
            let left = cond.get("left").cloned().unwrap_or(Value::Null);
            let right = cond.get("right").cloned().unwrap_or(Value::Null);
            let result = condition::compare(&left, op, &right)?;
            return Ok((result, json!({ "left": left, "operator": op, "right": right })));
        }
    }

    if let Some(op) = config.get("operator").and_then(Value::as_str) {
        let left = config.get("left").cloned().unwrap_or(Value::Null);
        let right = config.get("right").cloned().unwrap_or(Value::Null);
        let result = condition::compare(&left, op, &right)?;
        return Ok((result, json!({ "left": left, "operator": op, "right": right })));
    }

    match condition {
        Some(v) => Ok((
            condition::truthy(v),
            json!({ "value": v, "operator": "truthy" }),
        )),
        None => Err(
            "If: provide a `condition` (value or { left, operator, right }) or top-level `operator`"
                .to_string(),
        ),
    }
}

#[async_trait]
impl NodeExecutor for IfExecutor {
    async fn execute(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        input: Value,
        config: Value,
    ) -> Result<Value, String> {
        let (result, condition) = evaluate(&config, &input)?;
        let true_handle = config
            .get("trueHandle")
            .and_then(Value::as_str)
            .unwrap_or("true");
        let false_handle = config
            .get("falseHandle")
            .and_then(Value::as_str)
            .unwrap_or("false");
        let handle = if result { true_handle } else { false_handle };
        // `condition` records the operands/operator that produced `result`, so the
        // executions tab can show what was evaluated, not just the outcome.
        Ok(json!({
            "result": result,
            "condition": condition,
            "selectedHandles": [handle],
        }))
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
    async fn structured_condition_true_branch() {
        let config = json!({ "condition": { "left": 1500, "operator": "gt", "right": 1000 } });
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["result"], true);
        assert_eq!(out["selectedHandles"], json!(["true"]));
    }

    #[tokio::test]
    async fn top_level_operator_false_branch() {
        let config = json!({ "left": "a", "operator": "eq", "right": "b" });
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["result"], false);
        assert_eq!(out["selectedHandles"], json!(["false"]));
    }

    #[tokio::test]
    async fn plain_truthy_condition() {
        let config = json!({ "condition": "yes" });
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["true"]));

        let config = json!({ "condition": false });
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["false"]));
    }

    #[tokio::test]
    async fn custom_handle_labels() {
        let config = json!({ "condition": true, "trueHandle": "yes", "falseHandle": "no" });
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        assert_eq!(out["selectedHandles"], json!(["yes"]));
    }

    #[tokio::test]
    async fn missing_condition_errors() {
        let out = IfExecutor.execute(&ctx(), "n1", Value::Null, json!({})).await;
        assert!(out.is_err());
    }
}
