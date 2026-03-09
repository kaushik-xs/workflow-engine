use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;

/// HttpTrigger node: no-op at execute time; context is already set by the HTTP trigger layer.
pub struct HttpTriggerExecutor;

#[async_trait]
impl NodeExecutor for HttpTriggerExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        _input: Value,
        _config: Value,
    ) -> Result<Value, String> {
        let webhook = ctx
            .context
            .get("Webhook")
            .cloned()
            .unwrap_or(Value::Object(serde_json::Map::new()));
        Ok(webhook)
    }
}
