use serde_json::Value;

/// Normalized node spec for the executor (derived from React Flow node).
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub id: String,
    pub node_type: String,
    pub config: Value,
    pub input: Value,
}

/// Edge for execution order.
///
/// `source_handle` identifies which output port of the source node the edge leaves
/// (React Flow's `sourceHandle`). It is `None` for ordinary single-output nodes and
/// carries the port name (e.g. `"true"`/`"false"` for an `If`, or a case label for a
/// `Switch`) for branching nodes, so the executor can activate only the taken branch.
#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub source: String,
    pub target: String,
    pub source_handle: Option<String>,
}

/// Parse React Flow workflow JSON into normalized nodes and edges.
/// Expects shape: { "data": { "nodes": [...], "edges": [...] } } or top-level "nodes"/"edges".
pub fn parse_workflow(definition: &Value) -> Result<(Vec<NodeSpec>, Vec<EdgeSpec>), String> {
    let (nodes_arr, edges_arr) = get_nodes_and_edges(definition)?;
    let nodes = nodes_arr
        .as_array()
        .ok_or("nodes must be an array")?;
    let edges = edges_arr
        .as_array()
        .ok_or("edges must be an array")?;

    let node_specs: Vec<NodeSpec> = nodes
        .iter()
        .filter_map(|n| node_to_spec(n).ok())
        .collect();

    let edge_specs: Vec<EdgeSpec> = edges
        .iter()
        .filter_map(|e| {
            let src = e.get("source")?.as_str()?;
            let tgt = e.get("target")?.as_str()?;
            // React Flow emits `sourceHandle`; treat empty string as no handle.
            let source_handle = e
                .get("sourceHandle")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            Some(EdgeSpec {
                source: src.to_string(),
                target: tgt.to_string(),
                source_handle,
            })
        })
        .collect();

    Ok((node_specs, edge_specs))
}

/// Extract the workflow's declared local variables (an object of `key -> default value`).
///
/// Looked up at `data.variables` first, then top-level `variables`. Values may contain
/// `{{ }}` expressions; they are interpolated at execution start (against `Webhook` and
/// `global`) to seed the `local` scope. Returns an empty object when none are declared.
pub fn parse_variables(definition: &Value) -> Value {
    let vars = definition
        .get("data")
        .and_then(|d| d.get("variables"))
        .or_else(|| definition.get("variables"));
    match vars {
        Some(v) if v.is_object() => v.clone(),
        _ => Value::Object(serde_json::Map::new()),
    }
}

fn get_nodes_and_edges(definition: &Value) -> Result<(&Value, &Value), String> {
    if let Some(data) = definition.get("data") {
        let nodes = data.get("nodes").ok_or("data.nodes required")?;
        let edges = data.get("edges").ok_or("data.edges required")?;
        return Ok((nodes, edges));
    }
    let nodes = definition.get("nodes").ok_or("definition.nodes or data.nodes required")?;
    let edges = definition.get("edges").ok_or("definition.edges or data.edges required")?;
    Ok((nodes, edges))
}

fn node_to_spec(node: &Value) -> Result<NodeSpec, String> {
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .ok_or("node.id required")?
        .to_string();
    let raw_type = node
        .get("type")
        .and_then(Value::as_str)
        .ok_or("node.type required")?;
    let node_type = to_pascal_case(raw_type);
    let data = node.get("data").cloned().unwrap_or(Value::Object(serde_json::Map::new()));
    let config = data.clone();
    let input = data.get("input").cloned().unwrap_or(Value::Object(serde_json::Map::new()));
    Ok(NodeSpec {
        id,
        node_type,
        config,
        input,
    })
}

/// Normalize React Flow node type to registry key: httpTrigger -> HttpTrigger
pub fn to_pascal_case(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut capitalize = true;
    for c in s.chars() {
        if c == '_' || c == ' ' || c == '-' {
            capitalize = true;
        } else if capitalize {
            out.extend(c.to_uppercase());
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variables_reads_data_variables() {
        let def = serde_json::json!({
            "data": { "nodes": [], "edges": [], "variables": { "retries": 3, "greeting": "hi" } }
        });
        let vars = parse_variables(&def);
        assert_eq!(vars["retries"], 3);
        assert_eq!(vars["greeting"], "hi");
    }

    #[test]
    fn parse_variables_reads_top_level_variables() {
        let def = serde_json::json!({
            "nodes": [], "edges": [], "variables": { "flag": true }
        });
        assert_eq!(parse_variables(&def)["flag"], true);
    }

    #[test]
    fn parse_workflow_reads_edge_source_handle() {
        let def = serde_json::json!({
            "data": {
                "nodes": [
                    { "id": "a", "type": "if", "data": {} },
                    { "id": "b", "type": "httpRequest", "data": {} },
                    { "id": "c", "type": "httpRequest", "data": {} }
                ],
                "edges": [
                    { "source": "a", "target": "b", "sourceHandle": "true" },
                    { "source": "a", "target": "c", "sourceHandle": "false" },
                    // Empty handle is normalized to None.
                    { "source": "b", "target": "c", "sourceHandle": "" }
                ]
            }
        });
        let (_nodes, edges) = parse_workflow(&def).unwrap();
        assert_eq!(edges[0].source_handle.as_deref(), Some("true"));
        assert_eq!(edges[1].source_handle.as_deref(), Some("false"));
        assert_eq!(edges[2].source_handle, None);
    }

    #[test]
    fn parse_variables_defaults_to_empty_object() {
        let def = serde_json::json!({ "data": { "nodes": [], "edges": [] } });
        assert_eq!(parse_variables(&def), serde_json::json!({}));
        // Non-object variables are ignored.
        let def = serde_json::json!({ "variables": "nope" });
        assert_eq!(parse_variables(&def), serde_json::json!({}));
    }
}
