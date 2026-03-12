use crate::definition::{self, NodeSpec};
use crate::expression;
use crate::registry::NodeRegistry;
use crate::storage;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use uuid::Uuid;

/// Execution context passed to each node: full context (nodes, Webhook, env, current).
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub context: Value,
    pub workflow_id: Uuid,
    pub execution_id: Uuid,
    pub tenant_id: Option<Uuid>,
}

impl ExecutionContext {
    pub fn new(workflow_id: Uuid, execution_id: Uuid, initial_context: Value) -> Self {
        Self {
            context: initial_context,
            workflow_id,
            execution_id,
            tenant_id: None,
        }
    }

    /// Set current node output in context.nodes.<node_id>
    pub fn set_node_output(&mut self, node_id: &str, output: Value) {
        if !self.context.is_object() {
            self.context = Value::Object(serde_json::Map::new());
        }
        let obj = self.context.as_object_mut().unwrap();
        let nodes = obj
            .entry("nodes")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(n) = nodes.as_object_mut() {
            n.insert(node_id.to_string(), output);
        }
    }

    /// Set current node for expression resolution (current = last node output or input).
    pub fn set_current(&mut self, current: Value) {
        if let Some(obj) = self.context.as_object_mut() {
            obj.insert("current".to_string(), current);
        }
    }
}

/// Topological sort of node ids: nodes that have no incoming edges (or only from self) come first.
fn topological_order(node_specs: &[NodeSpec], edges: &[crate::definition::EdgeSpec]) -> Vec<String> {
    let node_ids: HashSet<String> = node_specs.iter().map(|n| n.id.clone()).collect();
    let mut in_degree: HashMap<String, usize> = node_ids.iter().cloned().map(|id| (id, 0)).collect();
    let mut out_edges: HashMap<String, Vec<String>> =
        node_ids.iter().cloned().map(|id| (id, Vec::new())).collect();

    for e in edges {
        if node_ids.contains(&e.source) && node_ids.contains(&e.target) && e.source != e.target {
            out_edges
                .get_mut(&e.source)
                .unwrap()
                .push(e.target.clone());
            *in_degree.get_mut(&e.target).unwrap() += 1;
        }
    }

    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        for target in out_edges.get(&id).unwrap_or(&vec![]) {
            if let Some(d) = in_degree.get_mut(target) {
                *d = d.saturating_sub(1);
                if *d == 0 {
                    queue.push_back(target.clone());
                }
            }
        }
    }
    order
}

/// Ensures context has nodes, env, and current so expressions can safely reference them.
fn ensure_context_shape(context: &mut Value) {
    if let Some(obj) = context.as_object_mut() {
        obj.entry("nodes")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("current")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("env").or_insert_with(|| {
            let mut env = serde_json::Map::new();
            for (k, v) in std::env::vars() {
                env.insert(k, Value::String(v));
            }
            Value::Object(env)
        });
    }
}

/// Run workflow to completion (or first failure). Updates execution and steps in DB.
pub async fn run_workflow(
    pool: &sqlx::PgPool,
    node_registry: Arc<dyn NodeRegistry>,
    workflow_id: Uuid,
    execution_id: Uuid,
    definition: &Value,
    initial_context: Value,
) -> Result<Value, String> {
    let (node_specs, edge_specs) = definition::parse_workflow(definition)?;
    let order = topological_order(&node_specs, &edge_specs);
    let nodes_by_id: HashMap<String, &NodeSpec> =
        node_specs.iter().map(|n| (n.id.clone(), n)).collect();

    let mut context = initial_context;
    if !context.is_object() {
        context = Value::Object(serde_json::Map::new());
    }
    ensure_context_shape(&mut context);

    let mut last_output = Value::Object(serde_json::Map::new());

    for node_id in order {
        let node = nodes_by_id
            .get(&node_id)
            .ok_or_else(|| format!("node not found: {}", node_id))?;
        let executor = node_registry
            .get(&node.node_type)
            .ok_or_else(|| format!("unknown node type: {}", node.node_type))?;

        let mut exec_ctx = ExecutionContext::new(workflow_id, execution_id, context.clone());
        exec_ctx.set_current(last_output.clone());

        let mut input = node.input.clone();
        let mut config = node.config.clone();
        expression::interpolate_value(&mut input, &exec_ctx.context).map_err(|e| e.to_string())?;
        expression::interpolate_value(&mut config, &exec_ctx.context).map_err(|e| e.to_string())?;

        tracing::info!(
            execution_id = %execution_id,
            node_id = %node_id,
            node_type = %node.node_type,
            context = %serde_json::to_string(&exec_ctx.context).unwrap_or_default(),
            "execution context before node execution"
        );

        match executor.execute(&exec_ctx, &node_id, input, config).await {
            Ok(output) => {
                last_output = output.clone();
                exec_ctx.set_node_output(&node_id, output.clone());
                context = exec_ctx.context;
                storage::update_execution(
                    pool,
                    execution_id,
                    "running",
                    &context,
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
                storage::insert_step(
                    pool,
                    execution_id,
                    &node_id,
                    "completed",
                    Some(&output),
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                storage::update_execution(
                    pool,
                    execution_id,
                    "failed",
                    &context,
                    Some(chrono::Utc::now()),
                )
                .await
                .map_err(|e2| e2.to_string())?;
                let _ = storage::insert_step(
                    pool,
                    execution_id,
                    &node_id,
                    "failed",
                    None,
                    Some(&e),
                )
                .await;
                return Err(e);
            }
        }
    }

    storage::update_execution(
        pool,
        execution_id,
        "completed",
        &context,
        Some(chrono::Utc::now()),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(context)
}
