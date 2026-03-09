use crate::nodes::NodeExecutor;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::nodes::{HttpRequestExecutor, HttpTriggerExecutor, ServiceCallExecutor};

pub trait NodeRegistry: Send + Sync {
    fn get(&self, node_type: &str) -> Option<Arc<dyn NodeExecutor>>;
}

/// Default registry with HttpTrigger, HttpRequest, ServiceCall registered.
pub struct DefaultNodeRegistry {
    map: HashMap<String, Arc<dyn NodeExecutor>>,
}

impl DefaultNodeRegistry {
    pub fn new(service_registry: Option<Arc<dyn ServiceRegistry>>) -> Self {
        let mut map: HashMap<String, Arc<dyn NodeExecutor>> = HashMap::new();
        map.insert("HttpTrigger".to_string(), Arc::new(HttpTriggerExecutor));
        map.insert("HttpRequest".to_string(), Arc::new(HttpRequestExecutor::default()));
        let service_call: Arc<dyn NodeExecutor> = match service_registry {
            Some(r) => Arc::new(ServiceCallExecutor::new(r)),
            None => Arc::new(ServiceCallExecutor::default()),
        };
        map.insert("ServiceCall".to_string(), service_call);
        Self { map }
    }

    pub fn register(&mut self, node_type: &str, executor: Arc<dyn NodeExecutor>) {
        self.map.insert(node_type.to_string(), executor);
    }
}

impl NodeRegistry for DefaultNodeRegistry {
    fn get(&self, node_type: &str) -> Option<Arc<dyn NodeExecutor>> {
        self.map.get(node_type).cloned().or_else(|| {
            let pascal = crate::definition::to_pascal_case(node_type);
            self.map.get(&pascal).cloned()
        })
    }
}

/// Internal service handler for ServiceCall node.
#[async_trait]
pub trait ServiceHandler: Send + Sync {
    async fn call(
        &self,
        service: &str,
        operation: &str,
        input: Value,
    ) -> Result<Value, String>;
}

/// Registry of internal services (service_slug -> handler).
pub trait ServiceRegistry: Send + Sync {
    fn get(&self, service: &str) -> Option<Arc<dyn ServiceHandler>>;
}

pub struct DefaultServiceRegistry {
    map: HashMap<String, Arc<dyn ServiceHandler>>,
}

impl DefaultServiceRegistry {
    pub fn new() -> Self {
        let mut map: HashMap<String, Arc<dyn ServiceHandler>> = HashMap::new();
        map.insert(
            "authrs".to_string(),
            Arc::new(StubAuthService),
        );
        Self { map }
    }

    pub fn register(&mut self, service: &str, handler: Arc<dyn ServiceHandler>) {
        self.map.insert(service.to_string(), handler);
    }
}

impl ServiceRegistry for DefaultServiceRegistry {
    fn get(&self, service: &str) -> Option<Arc<dyn ServiceHandler>> {
        self.map.get(service).cloned()
    }
}

/// Stub handler for authrs (and any unregistered service returns empty object).
struct StubAuthService;

#[async_trait]
impl ServiceHandler for StubAuthService {
    async fn call(
        &self,
        _service: &str,
        _operation: &str,
        _input: Value,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({}))
    }
}
