use crate::nodes::NodeExecutor;
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
    pub fn new(pool: Option<Arc<sqlx::PgPool>>) -> Self {
        let mut map: HashMap<String, Arc<dyn NodeExecutor>> = HashMap::new();
        map.insert("HttpTrigger".to_string(), Arc::new(HttpTriggerExecutor));
        map.insert("HttpRequest".to_string(), Arc::new(HttpRequestExecutor::default()));
        let service_call: Arc<dyn NodeExecutor> = match pool {
            Some(p) => Arc::new(ServiceCallExecutor::new(p)),
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
