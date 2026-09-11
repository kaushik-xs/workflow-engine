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
    pub tenant: Option<String>,
}

impl ExecutionContext {
    pub fn new(workflow_id: Uuid, execution_id: Uuid, initial_context: Value) -> Self {
        Self {
            context: initial_context,
            workflow_id,
            execution_id,
            tenant: None,
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

/// Run a single node and persist execution + step. Returns updated (context, last_output) or Err on failure.
async fn run_single_node(
    pool: &sqlx::PgPool,
    node_registry: &dyn NodeRegistry,
    workflow_id: Uuid,
    execution_id: Uuid,
    context: Value,
    last_output: Value,
    node: &NodeSpec,
) -> Result<(Value, Value), String> {
    let executor = node_registry
        .get(&node.node_type)
        .ok_or_else(|| format!("unknown node type: {}", node.node_type))?;

    let mut exec_ctx = ExecutionContext::new(workflow_id, execution_id, context);
    exec_ctx.set_current(last_output);

    let mut input = node.input.clone();
    let mut config = node.config.clone();
    tracing::debug!(
        execution_id = %execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        input_before = ?input,
        config_before = ?config,
        "interpolating node input and config"
    );
    expression::interpolate_value(&mut input, &exec_ctx.context).map_err(|e| e.to_string())?;
    expression::interpolate_value(&mut config, &exec_ctx.context).map_err(|e| e.to_string())?;
    tracing::debug!(
        execution_id = %execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        input_after = ?input,
        config_after = ?config,
        "interpolated node input and config"
    );

    tracing::info!(
        execution_id = %execution_id,
        node_id = %node.id,
        node_type = %node.node_type,
        "executing node"
    );

    match executor
        .execute(&exec_ctx, &node.id, input, config)
        .await
    {
        Ok(output) => {
            tracing::debug!(
                execution_id = %execution_id,
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
            storage::update_execution(pool, execution_id, "running", &context, None)
                .await
                .map_err(|e| e.to_string())?;
            storage::insert_step(pool, execution_id, &node.id, "completed", Some(&output), None)
                .await
                .map_err(|e| e.to_string())?;
            Ok((context, output))
        }
        Err(e) => {
            tracing::error!(
                execution_id = %execution_id,
                node_id = %node.id,
                node_type = %node.node_type,
                error = %e,
                "node execution failed"
            );
            storage::update_execution(
                pool,
                execution_id,
                "failed",
                &exec_ctx.context,
                Some(chrono::Utc::now()),
            )
            .await
            .map_err(|e2| e2.to_string())?;
            let _ = storage::insert_step(
                pool,
                execution_id,
                &node.id,
                "failed",
                None,
                Some(&e),
            )
            .await;
            Err(e)
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
) -> Result<Value, String> {
    let (node_specs, edge_specs) = definition::parse_workflow(definition)?;
    let order = topological_order(&node_specs, &edge_specs);
    let nodes_by_id: HashMap<String, &NodeSpec> =
        node_specs.iter().map(|n| (n.id.clone(), n)).collect();
    let node_ids: HashSet<String> = node_specs.iter().map(|n| n.id.clone()).collect();
    let pred = predecessors_by_node(&edge_specs, &node_ids);
    let incoming = incoming_edges_by_node(&edge_specs, &node_ids);

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

        // Skip nodes reachable only through a branch port that was not taken. The
        // topological order guarantees every predecessor already ran or was skipped, so
        // `nodes` holds all the branch decisions needed to route this node.
        let reachable = {
            let empty = serde_json::Map::new();
            let node_outputs = context.get("nodes").and_then(Value::as_object).unwrap_or(&empty);
            node_reachable(&node_id, &incoming, node_outputs)
        };
        if !reachable {
            tracing::info!(
                execution_id = %execution_id,
                node_id = %node_id,
                node_type = %node.node_type,
                "skipping node (branch not taken)"
            );
            storage::insert_step(pool, execution_id, &node_id, "skipped", None, None)
                .await
                .map_err(|e| e.to_string())?;
            continue;
        }

        // Merge nodes receive all predecessor outputs keyed by source node id; others get previous node output.
        if node.node_type == "Merge" {
            let preds = pred.get(&node_id).cloned().unwrap_or_default();
            last_output = merged_predecessor_outputs(&context, &preds);
        }
        let (new_ctx, new_out) = run_single_node(
            pool,
            node_registry.as_ref(),
            workflow_id,
            execution_id,
            context,
            last_output,
            node,
        )
        .await?;
        context = new_ctx;
        last_output = new_out;
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
) -> Result<RunNextStepResult, String> {
    let (node_specs, edge_specs) = definition::parse_workflow(definition)?;
    let order = topological_order(&node_specs, &edge_specs);
    let node_ids: HashSet<String> = node_specs.iter().map(|n| n.id.clone()).collect();
    let nodes_by_id: HashMap<String, &NodeSpec> =
        node_specs.iter().map(|n| (n.id.clone(), n)).collect();
    let pred = predecessors_by_node(&edge_specs, &node_ids);
    let incoming = incoming_edges_by_node(&edge_specs, &node_ids);

    let steps = storage::list_steps_by_execution(pool, execution_id)
        .await
        .map_err(|e| e.to_string())?;
    // A node is "resolved" once it has completed, failed, or been skipped.
    let mut resolved: HashSet<String> = steps
        .iter()
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
        storage::insert_step(pool, execution_id, &candidate, "skipped", None, None)
            .await
            .map_err(|e| e.to_string())?;
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

    let (new_context, _) = run_single_node(
        pool,
        node_registry.as_ref(),
        workflow_id,
        execution_id,
        context,
        last_output,
        node,
    )
    .await?;

    let steps_after = storage::list_steps_by_execution(pool, execution_id)
        .await
        .map_err(|e| e.to_string())?;
    let completed_after: HashSet<String> = steps_after
        .iter()
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
}
