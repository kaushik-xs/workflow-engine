use async_trait::async_trait;
use serde_json::Value;

pub use crate::executor::ExecutionContext;
pub use http_request::HttpRequestExecutor;
pub use http_trigger::HttpTriggerExecutor;
pub use service_call::ServiceCallExecutor;

mod http_request;
mod http_trigger;
mod service_call;

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        node_id: &str,
        input: Value,
        config: Value,
    ) -> Result<Value, String>;
}
