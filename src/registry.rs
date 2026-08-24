use crate::nodes::NodeExecutor;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crate::nodes::{
    HttpRequestExecutor, HttpTriggerExecutor, MergeExecutor, ServiceCallExecutor,
    WorkflowCallExecutor,
};

pub trait NodeRegistry: Send + Sync {
    fn get(&self, node_type: &str) -> Option<Arc<dyn NodeExecutor>>;
}

/// Default registry with HttpTrigger, HttpRequest, Merge, ServiceCall registered.
/// WorkflowCall needs a reference to the registry itself, so it is only wired up by
/// [`DefaultNodeRegistry::new_arc`] (see below).
pub struct DefaultNodeRegistry {
    map: HashMap<String, Arc<dyn NodeExecutor>>,
}

impl DefaultNodeRegistry {
    pub fn new(pool: Option<Arc<sqlx::PgPool>>) -> Self {
        let mut map: HashMap<String, Arc<dyn NodeExecutor>> = HashMap::new();
        map.insert("HttpTrigger".to_string(), Arc::new(HttpTriggerExecutor));
        map.insert("HttpRequest".to_string(), Arc::new(HttpRequestExecutor::default()));
        map.insert("Merge".to_string(), Arc::new(MergeExecutor));
        let service_call: Arc<dyn NodeExecutor> = match pool {
            Some(p) => Arc::new(ServiceCallExecutor::new(p)),
            None => Arc::new(ServiceCallExecutor::default()),
        };
        map.insert("ServiceCall".to_string(), service_call);
        Self { map }
    }

    /// Build the registry as an `Arc`, additionally wiring node types that need a
    /// reference back to the registry itself — currently `WorkflowCall`, which
    /// recursively executes other workflows through this same registry.
    pub fn new_arc(pool: Arc<sqlx::PgPool>) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<DefaultNodeRegistry>| {
            let mut registry = DefaultNodeRegistry::new(Some(pool.clone()));
            let weak_registry: Weak<dyn NodeRegistry> = weak.clone();
            registry.register(
                "WorkflowCall",
                Arc::new(WorkflowCallExecutor::new(pool.clone(), weak_registry)),
            );
            registry
        })
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
