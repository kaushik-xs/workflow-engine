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
#[derive(Debug, Clone)]
pub struct EdgeSpec {
    pub source: String,
    pub target: String,
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
            Some(EdgeSpec {
                source: src.to_string(),
                target: tgt.to_string(),
            })
        })
        .collect();

    Ok((node_specs, edge_specs))
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
