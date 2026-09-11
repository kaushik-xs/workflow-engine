//! SetVariable node: writes values into the workflow's `local` scope during execution.
//!
//! The returned object is merged into `context.local` by the executor, so downstream
//! nodes and later steps can read the updated `{{ local.* }}` values. Existing `local`
//! keys are overwritten; keys not mentioned are left untouched.
//!
//! Config keys (in node `data`), values may use `{{ }}` expressions (interpolated before
//! this node runs):
//!   - `variables`  an object of `{ key: value, ... }` to set (preferred), or
//!   - `key` + `value`  set a single variable.

use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;

pub struct SetVariableExecutor;

#[async_trait]
impl NodeExecutor for SetVariableExecutor {
    async fn execute(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        input: Value,
        config: Value,
    ) -> Result<Value, String> {
        // Prefer an explicit `variables` object; fall back to a single key/value pair.
        let vars = config
            .get("variables")
            .or_else(|| input.get("variables"))
            .cloned();

        let map = match vars {
            Some(Value::Object(map)) => map,
            Some(_) => return Err("SetVariable: `variables` must be an object".to_string()),
            None => {
                let key = config
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("SetVariable: provide a `variables` object or a `key` + `value`")?;
                let value = config.get("value").cloned().unwrap_or(Value::Null);
                let mut m = serde_json::Map::new();
                m.insert(key.to_string(), value);
                m
            }
        };

        Ok(Value::Object(map))
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
    async fn returns_variables_object() {
        let config = serde_json::json!({ "variables": { "a": 1, "b": "two" } });
        let out = SetVariableExecutor
            .execute(&ctx(), "n1", Value::Null, config)
            .await
            .unwrap();
        assert_eq!(out["a"], 1);
        assert_eq!(out["b"], "two");
    }

    #[tokio::test]
    async fn supports_single_key_value() {
        let config = serde_json::json!({ "key": "count", "value": 7 });
        let out = SetVariableExecutor
            .execute(&ctx(), "n1", Value::Null, config)
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({ "count": 7 }));
    }

    #[tokio::test]
    async fn errors_without_variables_or_key() {
        let out = SetVariableExecutor
            .execute(&ctx(), "n1", Value::Null, serde_json::json!({}))
            .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn errors_when_variables_not_object() {
        let config = serde_json::json!({ "variables": "nope" });
        let out = SetVariableExecutor
            .execute(&ctx(), "n1", Value::Null, config)
            .await;
        assert!(out.is_err());
    }
}
