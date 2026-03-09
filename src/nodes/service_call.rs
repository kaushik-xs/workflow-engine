use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::expression;
use crate::registry::ServiceRegistry;

pub struct ServiceCallExecutor {
    service_registry: Option<Arc<dyn ServiceRegistry>>,
}

impl Default for ServiceCallExecutor {
    fn default() -> Self {
        Self {
            service_registry: None,
        }
    }
}

impl ServiceCallExecutor {
    pub fn new(service_registry: Arc<dyn ServiceRegistry>) -> Self {
        Self {
            service_registry: Some(service_registry),
        }
    }
}

#[async_trait]
impl NodeExecutor for ServiceCallExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        mut input: Value,
        config: Value,
    ) -> Result<Value, String> {
        let service = config
            .get("serviceSlug")
            .or_else(|| config.get("service"))
            .and_then(Value::as_str)
            .ok_or("ServiceCall config must have serviceSlug or service")?;
        let operation = config
            .get("operation")
            .or_else(|| config.get("path"))
            .and_then(Value::as_str)
            .map(|s| s.trim_start_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or("ServiceCall config must have operation or path")?;

        expression::interpolate_value(&mut input, &ctx.context)?;

        let handler = self
            .service_registry
            .as_ref()
            .and_then(|r| r.get(service))
            .ok_or_else(|| format!("unknown service: {}", service))?;

        handler.call(service, &operation, input).await
    }
}
