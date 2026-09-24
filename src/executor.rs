use crate::definition::{self, EdgeSpec, NodeSpec};
use crate::expression;
use crate::nodes::NodeExecutor;
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
    pub tenant: Option<String>,
    /// W3C trace id (32 hex) for this execution. Outbound nodes forward it as a
    /// `traceparent` header so downstream services join the same trace. `None`
    /// when no trace is in scope.
    pub trace_id: Option<String>,
}

impl ExecutionContext {
    pub fn new(workflow_id: Uuid, execution_id: Uuid, initial_context: Value) -> Self {
        Self {
            context: initial_context,
            workflow_id,
            execution_id,
            tenant: None,
            trace_id: None,
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
fn topological_order(node_specs: &[&NodeSpec], edges: &[EdgeSpec]) -> Vec<String> {
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

/// Ensures context has `nodes`, `current`, `global`, and `local` so expressions can
/// safely reference them. Existing values are never overwritten — `global` and `local`
/// are populated once at execution start (see [`build_initial_context`]) and persisted
/// in the execution context; this only backfills missing keys defensively.
///
/// The OS environment is intentionally NOT exposed. Use tenant-scoped `global` values
/// and workflow-scoped `local` variables instead.
fn ensure_context_shape(context: &mut Value) {
    if let Some(obj) = context.as_object_mut() {
        obj.entry("nodes")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("current")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("global")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        obj.entry("local")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

/// Build the persisted initial context for a new execution.
///
/// Starts from `base` (typically `{ "Webhook": {...} }`, plus internals like
/// `workflowCallDepth`), then:
///   1. loads the tenant's `global` key/value store (snapshotted into the context), and
///   2. seeds `local` from the workflow's declared variables — interpolating each default
///      against a context that already exposes `Webhook` and `global`.
///
/// The result is stored on the execution row, so step-mode runs and `SetVariable`
/// mutations read and update a single, stable `local` scope across steps.
pub async fn build_initial_context(
    pool: &sqlx::PgPool,
    tenant: &str,
    definition: &Value,
    base: Value,
) -> Result<Value, String> {
    let mut context = base;
    if !context.is_object() {
        context = Value::Object(serde_json::Map::new());
    }

    let globals = storage::get_globals_map(pool, tenant)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(obj) = context.as_object_mut() {
        obj.insert("global".to_string(), globals);
    }
    ensure_context_shape(&mut context);

    // Seed `local` by interpolating declared variable defaults against Webhook + global.
    let mut variables = definition::parse_variables(definition);
    expression::interpolate_value(&mut variables, &context)?;
    if let (Some(obj), Value::Object(vars)) = (context.as_object_mut(), variables) {
        obj.insert("local".to_string(), Value::Object(vars));
    }

    Ok(context)
}

/// Result of running one step (for step-by-step mode).
#[derive(Debug)]
pub struct RunNextStepResult {
    pub status: String,
    pub context: Value,
}

/// Node type of the loop container. Nodes whose `parent` is a Loop form its body.
const LOOP_NODE_TYPE: &str = "Loop";

/// What every node run needs besides the evolving context.
struct RunEnv<'a> {
    pool: &'a sqlx::PgPool,
    registry: &'a dyn NodeRegistry,
    workflow_id: Uuid,
    execution_id: Uuid,
    trace_id: Option<&'a str>,
    nodes: &'a [NodeSpec],
    edges: &'a [EdgeSpec],
}

/// Step tag for a run inside loops: the loop indices from the outermost loop inwards,
/// dot-joined ("0", "2.1"). Empty at the top level.
fn iteration_tag(path: &[usize]) -> String {
    path.iter().map(usize::to_string).collect::<Vec<_>>().join(".")
}

/// The nodes directly inside one scope (the top level, or one Loop's body) and their wiring.
struct ScopeGraph<'a> {
    order: Vec<String>,
    nodes_by_id: HashMap<String, &'a NodeSpec>,
    pred: HashMap<String, Vec<String>>,
    incoming: HashMap<String, Vec<EdgeSpec>>,
}

impl<'a> ScopeGraph<'a> {
    fn new(nodes: &'a [NodeSpec], edges: &[EdgeSpec], parent: Option<&str>) -> Self {
        let scoped: Vec<&NodeSpec> = nodes.iter().filter(|n| n.parent.as_deref() == parent).collect();
        let node_ids: HashSet<String> = scoped.iter().map(|n| n.id.clone()).collect();
        Self {
            order: topological_order(&scoped, edges),
            nodes_by_id: scoped.iter().map(|n| (n.id.clone(), *n)).collect(),
            pred: predecessors_by_node(edges, &node_ids),
            incoming: incoming_edges_by_node(edges, &node_ids),
        }
    }
}

/// Check that Loop nesting is well formed before anything runs: every parent is a Loop,
/// every Loop contains at least one node, and edges stay inside one scope. The only edge
/// allowed to cross is the Loop's own edge to a node directly inside it (the builder's
/// "start" connector), which marks an entry point and is otherwise ignored.
fn validate_loops(nodes: &[NodeSpec], edges: &[EdgeSpec]) -> Result<(), String> {
    let by_id: HashMap<&str, &NodeSpec> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for node in nodes {
        let mut parent = node.parent.as_deref();
        let mut depth = 0;
        while let Some(p) = parent {
            let parent_node = by_id
                .get(p)
                .ok_or_else(|| format!("node {} is inside unknown node {}", node.id, p))?;
            if parent_node.node_type != LOOP_NODE_TYPE {
                return Err(format!("node {} is inside {}, which is not a Loop", node.id, p));
            }
            depth += 1;
            if depth > nodes.len() {
                return Err(format!("node {} has circular parents", node.id));
            }
            parent = parent_node.parent.as_deref();
        }
        if node.node_type == LOOP_NODE_TYPE
            && !nodes.iter().any(|c| c.parent.as_deref() == Some(node.id.as_str()))
        {
            return Err(format!("Loop {} has no nodes inside it", node.id));
        }
    }
    for e in edges {
        let (Some(source), Some(target)) = (by_id.get(e.source.as_str()), by_id.get(e.target.as_str())) else {
            continue;
        };
        if source.parent == target.parent || target.parent.as_deref() == Some(source.id.as_str()) {
            continue;
        }
        return Err(format!(
            "edge {} -> {} crosses a Loop boundary: nodes inside a Loop can only connect to each other",
            e.source, e.target
        ));
    }
    Ok(())
}

/// Run one node, record its step and, at the top level, persist the execution context.
/// Inside a loop only the step is written; the context is persisted once the whole Loop
/// node finishes, so a long loop does not rewrite the full context per item.
/// Returns the updated context and the node's output.
async fn run_single_node(
    env: &RunEnv<'_>,
    context: Value,
    last_output: Value,
    node: &NodeSpec,
    iteration: &[usize],
) -> Result<(Value, Value), String> {
    let mut exec_ctx = ExecutionContext::new(env.workflow_id, env.execution_id, context);
    exec_ctx.trace_id = env.trace_id.map(str::to_string);
    exec_ctx.set_current(last_output);
    let tag = iteration_tag(iteration);
    let top_level = iteration.is_empty();

    tracing::info!(
        execution_id = %env.execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        iteration = %tag,
        "executing node"
    );

    let result = if node.node_type == LOOP_NODE_TYPE {
        run_loop(env, &mut exec_ctx, node, iteration).await
    } else {
        match env.registry.get(&node.node_type) {
            Some(executor) => execute_node(executor.as_ref(), &exec_ctx, node).await,
            None => Err(format!("unknown node type: {}", node.node_type)),
        }
    };

    match result {
        Ok(output) => {
            tracing::debug!(
                execution_id = %env.execution_id,
                node_id = %node.id,
                node_type = %node.node_type,
                output = ?output,
                "node executed successfully"
            );
            // A SetVariable node returns an object of variables to write into the `local`
            // scope; merge them so subsequent nodes (and later steps) see the updates.
            if node.node_type == "SetVariable" {
                if let Value::Object(vars) = &output {
                    merge_into_local(&mut exec_ctx.context, vars);
                }
            }
            exec_ctx.set_node_output(&node.id, output.clone());
            let context = exec_ctx.context;
            if top_level {
                storage::update_execution(env.pool, env.execution_id, "running", &context, None)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            storage::insert_step(env.pool, env.execution_id, &node.id, &tag, "completed", Some(&output), None)
                .await
                .map_err(|e| e.to_string())?;
            Ok((context, output))
        }
        Err(e) => {
            tracing::error!(
                execution_id = %env.execution_id,
                node_id = %node.id,
                node_type = %node.node_type,
                iteration = %tag,
                error = %e,
                "node execution failed"
            );
            if top_level {
                storage::update_execution(
                    env.pool,
                    env.execution_id,
                    "failed",
                    &exec_ctx.context,
                    Some(chrono::Utc::now()),
                )
                .await
                .map_err(|e2| e2.to_string())?;
            }
            let _ = storage::insert_step(env.pool, env.execution_id, &node.id, &tag, "failed", None, Some(&e)).await;
            // Inside a loop, name the failing node so the Loop's error points at it.
            Err(if top_level { e } else { format!("{}: {e}", node.id) })
        }
    }
}

/// Interpolate a node's input/config against its context and run it once.
async fn execute_node(
    executor: &dyn NodeExecutor,
    exec_ctx: &ExecutionContext,
    node: &NodeSpec,
) -> Result<Value, String> {
    let mut input = node.input.clone();
    let mut config = node.config.clone();
    tracing::debug!(
        execution_id = %exec_ctx.execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        input_before = ?input,
        config_before = ?config,
        "interpolating node input and config"
    );
    // One scope for both: converting the context is the expensive part.
    if expression::has_expressions(&input) || expression::has_expressions(&config) {
        let scope = expression::Scope::new(&exec_ctx.context)?;
        expression::interpolate_value_in(&mut input, &scope)?;
        expression::interpolate_value_in(&mut config, &scope)?;
    }
    tracing::debug!(
        execution_id = %exec_ctx.execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        input_after = ?input,
        config_after = ?config,
        "interpolated node input and config"
    );
    executor.execute(exec_ctx, &node.id, input, config).await
}

/// Resolve a Loop's `items` into the list to iterate. An expression that yields `null`
/// (e.g. an absent field) is an empty list, so "no attachments" is not an error.
fn resolve_items(config: &Value, context: &Value) -> Result<Vec<Value>, String> {
    let mut items = match config.get("items") {
        None => return Err("Loop needs `items`: an expression that returns a list".to_string()),
        Some(Value::String(s)) if s.trim().is_empty() => {
            return Err("Loop needs `items`: an expression that returns a list".to_string())
        }
        Some(v) => v.clone(),
    };
    expression::interpolate_value(&mut items, context)?;
    match items {
        Value::Array(list) => Ok(list),
        Value::Null => Ok(Vec::new()),
        other => Err(format!(
            "Loop items must be a list, got {}",
            match other {
                Value::Object(_) => "an object",
                Value::String(_) => "a string",
                Value::Number(_) => "a number",
                _ => "a boolean",
            }
        )),
    }
}

/// Run a Loop node: its body (the nodes whose `parent` is this Loop) runs once per item,
/// sequentially, stopping at the first failure.
///
/// Each iteration starts from the Loop's context plus `item` and `index` (the innermost
/// loop wins when nested). Entry nodes of the body get `current` = the item, and
/// `nodes.<id>` holds only that iteration's outputs. Changes to `local` carry over to
/// later iterations and past the loop, so SetVariable can accumulate.
///
/// Output: `{ "count", "results": [ { "<body node id>": <output>, ... } per item ] }`.
async fn run_loop(
    env: &RunEnv<'_>,
    exec_ctx: &mut ExecutionContext,
    node: &NodeSpec,
    iteration: &[usize],
) -> Result<Value, String> {
    let items = resolve_items(&node.config, &exec_ctx.context)?;
    let body_ids: Vec<&str> = env
        .nodes
        .iter()
        .filter(|n| n.parent.as_deref() == Some(node.id.as_str()))
        .map(|n| n.id.as_str())
        .collect();

    let mut results = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let mut context = exec_ctx.context.clone();
        if let Value::Object(map) = &mut context {
            map.insert("item".to_string(), item.clone());
            map.insert("index".to_string(), Value::from(index));
        }
        let mut path = iteration.to_vec();
        path.push(index);
        let (context, _) = run_scope(env, Some(node.id.as_str()), context, item, &path)
            .await
            .map_err(|e| format!("item {index}: {e}"))?;

        let outputs = context.get("nodes").and_then(Value::as_object);
        let result: serde_json::Map<String, Value> = body_ids
            .iter()
            .filter_map(|id| outputs.and_then(|o| o.get(*id)).map(|v| (id.to_string(), v.clone())))
            .collect();
        results.push(Value::Object(result));
        if let Some(local) = context.get("local").and_then(Value::as_object) {
            merge_into_local(&mut exec_ctx.context, local);
        }
    }
    Ok(serde_json::json!({ "count": results.len(), "results": results }))
}

type ScopeFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Value, Value), String>> + Send + 'a>>;

/// Run the nodes of one scope (the top level, or a Loop body for one item) in topological
/// order, skipping nodes on branches not taken. Returns the final context and last output.
/// Boxed because Loop bodies recurse back into it.
fn run_scope<'a>(
    env: &'a RunEnv<'a>,
    parent: Option<&'a str>,
    mut context: Value,
    mut last_output: Value,
    iteration: &'a [usize],
) -> ScopeFuture<'a> {
    Box::pin(async move {
        let graph = ScopeGraph::new(env.nodes, env.edges, parent);
        let tag = iteration_tag(iteration);
        for node_id in &graph.order {
            let node = graph
                .nodes_by_id
                .get(node_id)
                .ok_or_else(|| format!("node not found: {}", node_id))?;

            // Skip nodes reachable only through a branch port that was not taken. The
            // topological order guarantees every predecessor already ran or was skipped, so
            // `nodes` holds all the branch decisions needed to route this node.
            let reachable = {
                let empty = serde_json::Map::new();
                let node_outputs = context.get("nodes").and_then(Value::as_object).unwrap_or(&empty);
                node_reachable(node_id, &graph.incoming, node_outputs)
            };
            if !reachable {
                tracing::info!(
                    execution_id = %env.execution_id,
                    node_id = %node_id,
                    node_type = %node.node_type,
                    iteration = %tag,
                    "skipping node (branch not taken)"
                );
                storage::insert_step(env.pool, env.execution_id, node_id, &tag, "skipped", None, None)
                    .await
                    .map_err(|e| e.to_string())?;
                mark_skipped(&mut context, node_id);
                continue;
            }

            // Merge nodes receive all predecessor outputs keyed by source node id; others get previous node output.
            if node.node_type == "Merge" {
                let preds = graph.pred.get(node_id).cloned().unwrap_or_default();
                last_output = merged_predecessor_outputs(&context, &preds);
            }
            let (new_ctx, new_out) = run_single_node(env, context, last_output, node, iteration).await?;
            context = new_ctx;
            last_output = new_out;
        }
        Ok((context, last_output))
    })
}

/// Record a node id as skipped in `context.skipped` (deduplicated). Skipped nodes
/// produce no `nodes.<id>` output, so this list is how the executions view learns
/// which nodes were bypassed by a conditional branch (vs. never reached).
fn mark_skipped(context: &mut Value, node_id: &str) {
    if !context.is_object() {
        *context = Value::Object(serde_json::Map::new());
    }
    let obj = context.as_object_mut().unwrap();
    let arr = obj
        .entry("skipped")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(list) = arr.as_array_mut() {
        if !list.iter().any(|v| v.as_str() == Some(node_id)) {
            list.push(Value::String(node_id.to_string()));
        }
    }
}

/// Merge a set of variables into the context's `local` scope, creating it if absent.
fn merge_into_local(context: &mut Value, vars: &serde_json::Map<String, Value>) {
    if !context.is_object() {
        *context = Value::Object(serde_json::Map::new());
    }
    let obj = context.as_object_mut().unwrap();
    let local = obj
        .entry("local")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(local_obj) = local.as_object_mut() {
        for (k, v) in vars {
            local_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Build merged input for a Merge node: object keyed by predecessor node id with their outputs from context.
fn merged_predecessor_outputs(context: &Value, preds: &[String]) -> Value {
    let empty = serde_json::Map::new();
    let nodes = context
        .get("nodes")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let mut map = serde_json::Map::new();
    for p in preds {
        if let Some(out) = nodes.get(p) {
            // Put body content directly under node id when present (e.g. HTTP-style output); otherwise full output.
            let value = out
                .get("body")
                .cloned()
                .unwrap_or_else(|| out.clone());
            map.insert(p.clone(), value);
        }
    }
    Value::Object(map)
}

/// Run workflow to completion (or first failure). Updates execution and steps in DB.
pub async fn run_workflow(
    pool: &sqlx::PgPool,
    node_registry: Arc<dyn NodeRegistry>,
    workflow_id: Uuid,
    execution_id: Uuid,
    definition: &Value,
    initial_context: Value,
    trace_id: Option<String>,
) -> Result<Value, String> {
    let (node_specs, edge_specs) = definition::parse_workflow(definition)?;
    validate_loops(&node_specs, &edge_specs)?;
    let env = RunEnv {
        pool,
        registry: node_registry.as_ref(),
        workflow_id,
        execution_id,
        trace_id: trace_id.as_deref(),
        nodes: &node_specs,
        edges: &edge_specs,
    };

    let mut context = initial_context;
    if !context.is_object() {
        context = Value::Object(serde_json::Map::new());
    }
    ensure_context_shape(&mut context);

    let (context, _) = run_scope(&env, None, context, Value::Object(serde_json::Map::new()), &[]).await?;

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

/// Predecessors: for each node_id, the set of node ids that must complete before it (sources of edges targeting it).
fn predecessors_by_node(
    edges: &[crate::definition::EdgeSpec],
    node_ids: &HashSet<String>,
) -> HashMap<String, Vec<String>> {
    let mut pred: HashMap<String, Vec<String>> =
        node_ids.iter().cloned().map(|id| (id, Vec::new())).collect();
    for e in edges {
        if node_ids.contains(&e.source) && node_ids.contains(&e.target) && e.source != e.target {
            pred.get_mut(&e.target).unwrap().push(e.source.clone());
        }
    }
    pred
}

/// Incoming edges (with their `source_handle`) for each node. Unlike
/// [`predecessors_by_node`] this keeps the full edge so branch routing can inspect handles.
fn incoming_edges_by_node(
    edges: &[crate::definition::EdgeSpec],
    node_ids: &HashSet<String>,
) -> HashMap<String, Vec<crate::definition::EdgeSpec>> {
    let mut map: HashMap<String, Vec<crate::definition::EdgeSpec>> =
        node_ids.iter().cloned().map(|id| (id, Vec::new())).collect();
    for e in edges {
        if node_ids.contains(&e.source) && node_ids.contains(&e.target) && e.source != e.target {
            map.get_mut(&e.target).unwrap().push(e.clone());
        }
    }
    map
}

/// Whether an edge carries the flow, given the outputs of nodes that have run.
///
/// An edge is inactive when its source has not run (skipped or pending). A branch node
/// signals its taken port(s) via a `selectedHandles` array in its output: only edges
/// whose `source_handle` is listed stay active. An ordinary node (no `selectedHandles`)
/// activates all of its outgoing edges.
fn edge_active(edge: &crate::definition::EdgeSpec, node_outputs: &serde_json::Map<String, Value>) -> bool {
    match node_outputs.get(&edge.source) {
        None => false,
        Some(out) => match out.get("selectedHandles").and_then(Value::as_array) {
            Some(handles) => edge
                .source_handle
                .as_deref()
                .map(|h| handles.iter().any(|x| x.as_str() == Some(h)))
                .unwrap_or(false),
            None => true,
        },
    }
}

/// A node runs when it is an entry node (no incoming edges) or at least one of its
/// incoming edges is active. This drives branch skipping: a node reachable only through
/// a branch's untaken port has no active incoming edge and is skipped, which cascades to
/// anything downstream of it while a join reached by either branch still runs.
fn node_reachable(
    node_id: &str,
    incoming: &HashMap<String, Vec<crate::definition::EdgeSpec>>,
    node_outputs: &serde_json::Map<String, Value>,
) -> bool {
    match incoming.get(node_id) {
        Some(edges) if !edges.is_empty() => edges.iter().any(|e| edge_active(e, node_outputs)),
        _ => true,
    }
}

/// Run the next runnable step for a paused execution. Execution must be in `paused` status.
/// Loads steps from DB to determine which node to run next; updates execution and step; returns new status and context.
pub async fn run_next_step(
    pool: &sqlx::PgPool,
    node_registry: Arc<dyn NodeRegistry>,
    execution_id: Uuid,
    workflow_id: Uuid,
    definition: &Value,
    mut context: Value,
    trace_id: Option<String>,
) -> Result<RunNextStepResult, String> {
    let (node_specs, edge_specs) = definition::parse_workflow(definition)?;
    validate_loops(&node_specs, &edge_specs)?;
    let env = RunEnv {
        pool,
        registry: node_registry.as_ref(),
        workflow_id,
        execution_id,
        trace_id: trace_id.as_deref(),
        nodes: &node_specs,
        edges: &edge_specs,
    };
    // A step is one top-level node; a Loop runs its whole body within its step.
    let ScopeGraph { order, nodes_by_id, pred, incoming } = ScopeGraph::new(&node_specs, &edge_specs, None);

    let steps = storage::list_steps_by_execution(pool, execution_id)
        .await
        .map_err(|e| e.to_string())?;
    // A node is "resolved" once it has completed, failed, or been skipped.
    let mut resolved: HashSet<String> = steps
        .iter()
        .filter(|s| s.iteration.is_empty())
        .filter(|s| s.status == "completed" || s.status == "failed" || s.status == "skipped")
        .map(|s| s.node_id.clone())
        .collect();

    if !context.is_object() {
        context = Value::Object(serde_json::Map::new());
    }
    ensure_context_shape(&mut context);

    // Walk forward to the next runnable node, recording any branch-skipped nodes we pass
    // so a step that only lands on skipped nodes still makes progress toward completion.
    let next_node_id = loop {
        let candidate = order
            .iter()
            .find(|node_id| {
                !resolved.contains(*node_id)
                    && pred
                        .get(*node_id)
                        .map(|preds| preds.iter().all(|p| resolved.contains(p)))
                        .unwrap_or(true)
            })
            .cloned();
        let candidate = match candidate {
            Some(id) => id,
            None => {
                storage::update_execution(
                    pool,
                    execution_id,
                    "completed",
                    &context,
                    Some(chrono::Utc::now()),
                )
                .await
                .map_err(|e| e.to_string())?;
                return Ok(RunNextStepResult {
                    status: "completed".to_string(),
                    context: context.clone(),
                });
            }
        };
        let reachable = {
            let empty = serde_json::Map::new();
            let node_outputs = context.get("nodes").and_then(Value::as_object).unwrap_or(&empty);
            node_reachable(&candidate, &incoming, node_outputs)
        };
        if reachable {
            break candidate;
        }
        storage::insert_step(pool, execution_id, &candidate, "", "skipped", None, None)
            .await
            .map_err(|e| e.to_string())?;
        mark_skipped(&mut context, &candidate);
        resolved.insert(candidate);
    };

    let node = nodes_by_id
        .get(&next_node_id)
        .ok_or_else(|| format!("node not found: {}", next_node_id))?;
    let last_output = if node.node_type == "Merge" {
        let preds = pred.get(&next_node_id).cloned().unwrap_or_default();
        merged_predecessor_outputs(&context, &preds)
    } else {
        // The most recent node before this one that actually produced an output (skipped
        // nodes leave nothing in `nodes`, so they are naturally passed over).
        order
            .iter()
            .take_while(|id| *id != &next_node_id)
            .filter_map(|id| {
                context
                    .get("nodes")
                    .and_then(|n| n.as_object())
                    .and_then(|n| n.get(id))
                    .cloned()
            })
            .last()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    };

    let (new_context, _) = run_single_node(&env, context, last_output, node, &[]).await?;

    let steps_after = storage::list_steps_by_execution(pool, execution_id)
        .await
        .map_err(|e| e.to_string())?;
    let completed_after: HashSet<String> = steps_after
        .iter()
        .filter(|s| s.iteration.is_empty())
        .filter(|s| s.status == "completed" || s.status == "failed" || s.status == "skipped")
        .map(|s| s.node_id.clone())
        .collect();
    let more_remaining = order.iter().any(|id| !completed_after.contains(id));

    let status = if more_remaining {
        "paused"
    } else {
        "completed"
    };
    let finished_at = if more_remaining {
        None
    } else {
        Some(chrono::Utc::now())
    };
    storage::update_execution(pool, execution_id, status, &new_context, finished_at)
        .await
        .map_err(|e| e.to_string())?;

    Ok(RunNextStepResult {
        status: status.to_string(),
        context: new_context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_context_shape_adds_scopes_but_no_env() {
        let mut ctx = serde_json::json!({});
        ensure_context_shape(&mut ctx);
        assert_eq!(ctx["nodes"], serde_json::json!({}));
        assert_eq!(ctx["current"], serde_json::json!({}));
        assert_eq!(ctx["global"], serde_json::json!({}));
        assert_eq!(ctx["local"], serde_json::json!({}));
        // The OS environment must never be exposed to workflows.
        assert!(ctx.get("env").is_none());
    }

    #[test]
    fn ensure_context_shape_preserves_existing_scopes() {
        let mut ctx = serde_json::json!({
            "global": { "A": 1 },
            "local": { "counter": 5 }
        });
        ensure_context_shape(&mut ctx);
        assert_eq!(ctx["global"]["A"], 1);
        assert_eq!(ctx["local"]["counter"], 5);
    }

    #[test]
    fn merge_into_local_adds_and_overwrites() {
        let mut ctx = serde_json::json!({ "local": { "a": 1, "b": 2 } });
        let vars: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "b": 20, "c": 3 })).unwrap();
        merge_into_local(&mut ctx, &vars);
        assert_eq!(ctx["local"]["a"], 1); // untouched
        assert_eq!(ctx["local"]["b"], 20); // overwritten
        assert_eq!(ctx["local"]["c"], 3); // added
    }

    #[test]
    fn merge_into_local_creates_local_when_absent() {
        let mut ctx = serde_json::json!({});
        let vars: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "x": true })).unwrap();
        merge_into_local(&mut ctx, &vars);
        assert_eq!(ctx["local"]["x"], true);
    }

    fn edge(source: &str, target: &str, handle: Option<&str>) -> crate::definition::EdgeSpec {
        crate::definition::EdgeSpec {
            source: source.to_string(),
            target: target.to_string(),
            source_handle: handle.map(|h| h.to_string()),
        }
    }

    #[test]
    fn edge_active_for_ordinary_node_ignores_handle() {
        // A plain node (no `selectedHandles`) activates every outgoing edge.
        let outputs: serde_json::Map<String, Value> =
            serde_json::from_value(serde_json::json!({ "a": { "status": 200 } })).unwrap();
        assert!(edge_active(&edge("a", "b", None), &outputs));
        assert!(edge_active(&edge("a", "b", Some("whatever")), &outputs));
    }

    #[test]
    fn edge_active_only_for_selected_branch_handle() {
        let outputs: serde_json::Map<String, Value> = serde_json::from_value(
            serde_json::json!({ "if1": { "selectedHandles": ["true"] } }),
        )
        .unwrap();
        assert!(edge_active(&edge("if1", "yes", Some("true")), &outputs));
        assert!(!edge_active(&edge("if1", "no", Some("false")), &outputs));
        // A branch edge with no handle can never match.
        assert!(!edge_active(&edge("if1", "no", None), &outputs));
    }

    #[test]
    fn edge_from_unrun_source_is_inactive() {
        // Skipped/pending sources leave nothing in `nodes`, so their edges are inactive.
        let outputs = serde_json::Map::new();
        assert!(!edge_active(&edge("ghost", "b", None), &outputs));
    }

    #[test]
    fn node_reachability_across_a_branch_diamond() {
        // if1 --true--> yes --\
        //     --false-> no  ---> join
        let mut incoming: HashMap<String, Vec<crate::definition::EdgeSpec>> = HashMap::new();
        incoming.insert("if1".to_string(), vec![]); // entry
        incoming.insert("yes".to_string(), vec![edge("if1", "yes", Some("true"))]);
        incoming.insert("no".to_string(), vec![edge("if1", "no", Some("false"))]);
        incoming.insert(
            "join".to_string(),
            vec![edge("yes", "join", None), edge("no", "join", None)],
        );

        // if1 took the `true` port.
        let outputs: serde_json::Map<String, Value> = serde_json::from_value(
            serde_json::json!({ "if1": { "selectedHandles": ["true"] } }),
        )
        .unwrap();

        assert!(node_reachable("if1", &incoming, &outputs)); // entry always runs
        assert!(node_reachable("yes", &incoming, &outputs)); // taken branch
        assert!(!node_reachable("no", &incoming, &outputs)); // untaken branch skipped

        // After `yes` runs (and `no` is skipped), the join is still reachable via `yes`.
        let mut outputs = outputs;
        outputs.insert("yes".to_string(), serde_json::json!({ "status": 200 }));
        assert!(node_reachable("join", &incoming, &outputs));
    }

    fn node(id: &str, node_type: &str, parent: Option<&str>) -> NodeSpec {
        NodeSpec {
            id: id.to_string(),
            node_type: node_type.to_string(),
            config: serde_json::json!({}),
            input: serde_json::json!({}),
            parent: parent.map(str::to_string),
        }
    }

    #[test]
    fn scope_graph_keeps_loop_bodies_out_of_the_parent_scope() {
        let nodes = vec![
            node("trigger", "HttpTrigger", None),
            node("loop", "Loop", None),
            node("upload", "ServiceCall", Some("loop")),
            node("link", "ServiceCall", Some("loop")),
            node("done", "HttpRequest", None),
        ];
        let edges = vec![
            edge("trigger", "loop", None),
            edge("loop", "upload", None),
            edge("upload", "link", None),
            edge("loop", "done", None),
        ];
        validate_loops(&nodes, &edges).unwrap();

        let top = ScopeGraph::new(&nodes, &edges, None);
        assert_eq!(top.order, ["trigger", "loop", "done"]);

        let body = ScopeGraph::new(&nodes, &edges, Some("loop"));
        assert_eq!(body.order, ["upload", "link"]);
        // The Loop's "start" edge is not an incoming edge of the body entry node.
        assert!(body.incoming["upload"].is_empty());
        assert_eq!(body.pred["link"], ["upload"]);
    }

    #[test]
    fn validate_loops_rejects_bad_nesting_and_crossing_edges() {
        let err = |nodes: Vec<NodeSpec>, edges: Vec<EdgeSpec>| validate_loops(&nodes, &edges).unwrap_err();

        assert!(err(vec![node("loop", "Loop", None)], vec![]).contains("no nodes inside"));
        assert!(err(vec![node("a", "If", None), node("b", "ServiceCall", Some("a"))], vec![])
            .contains("not a Loop"));
        assert!(err(vec![node("b", "ServiceCall", Some("ghost"))], vec![]).contains("unknown node"));

        let body = || vec![node("loop", "Loop", None), node("in", "ServiceCall", Some("loop")), node("out", "HttpRequest", None)];
        assert!(err(body(), vec![edge("in", "out", None)]).contains("crosses a Loop boundary"));
        assert!(err(body(), vec![edge("out", "in", None)]).contains("crosses a Loop boundary"));
        // Back-edge from the body to its own Loop.
        assert!(err(body(), vec![edge("in", "loop", None)]).contains("crosses a Loop boundary"));

        // Nested loops are fine.
        let nested = vec![
            node("outer", "Loop", None),
            node("inner", "Loop", Some("outer")),
            node("call", "ServiceCall", Some("inner")),
        ];
        validate_loops(&nested, &[edge("outer", "inner", None), edge("inner", "call", None)]).unwrap();
    }

    #[test]
    fn resolve_items_accepts_lists_and_null() {
        let ctx = serde_json::json!({ "Webhook": { "body": { "files": [1, 2], "one": { "a": 1 } } } });
        let items = |v: Value| resolve_items(&serde_json::json!({ "items": v }), &ctx);

        assert_eq!(items("{{ Webhook.body.files }}".into()).unwrap(), [1, 2]);
        assert!(items("{{ Webhook.body.missing }}".into()).unwrap().is_empty());
        assert_eq!(items(serde_json::json!(["a"])).unwrap(), ["a"]);
        assert!(items("{{ Webhook.body.one }}".into()).unwrap_err().contains("got an object"));
        assert!(items("  ".into()).unwrap_err().contains("needs `items`"));
        assert!(resolve_items(&serde_json::json!({}), &ctx).unwrap_err().contains("needs `items`"));
    }

    #[test]
    fn iteration_tags_join_nested_indices() {
        assert_eq!(iteration_tag(&[]), "");
        assert_eq!(iteration_tag(&[3]), "3");
        assert_eq!(iteration_tag(&[2, 1]), "2.1");
    }
}
