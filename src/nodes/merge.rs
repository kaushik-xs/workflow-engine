//! Merge node: waits for all predecessor outputs and produces a single object
//! keyed by source node id. mergeType "default" only.

use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;

pub struct MergeExecutor;

#[async_trait]
impl NodeExecutor for MergeExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        _input: Value,
        config: Value,
    ) -> Result<Value, String> {
        let merge_type = config
            .get("mergeType")
            .and_then(Value::as_str)
            .unwrap_or("default");

        match merge_type {
            "default" => {
                // Executor sets context.current to { node_id: output } for each predecessor.
                let merged = ctx
                    .context
                    .get("current")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                // Return HTTP-response-shaped object: status + body (merged data).
                Ok(serde_json::json!({
                    "status": 200,
                    "body": merged
                }))
            }
            _ => Err(format!("unsupported mergeType: {}", merge_type)),
        }
    }
}
