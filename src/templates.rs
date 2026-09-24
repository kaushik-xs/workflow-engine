//! Reusable node templates.
//!
//! A template is a named, versioned node definition kept per tenant in `node_templates`. A
//! workflow uses one through a reference node:
//!
//! ```json
//! { "id": "mkTicket", "type": "template",
//!   "data": { "template": "create-ticket", "version": 3, "params": { "ticketId": "{{ Webhook.body.id }}" } } }
//! ```
//!
//! References are expanded when an execution starts ([`resolve_definition`]), never copied
//! into the stored workflow, so the template stays the single source of truth. Without
//! `version` a reference follows the latest version; with it, it is pinned. The template's
//! config reads the node's params as `{{ params.<name> }}`; params are the only thing a
//! workflow can set, and their values may themselves be `{{ }}` expressions.
//!
//! The versions a run resolved are kept in its context under `templates`, so a step-mode run
//! keeps using them even if a new version is published while it is paused.

use crate::definition::to_pascal_case;
use crate::expression;
use crate::storage::{self, NodeTemplate, NodeTemplateContent, TemplateUsage};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Registry key of a reference node (`"type": "template"`).
pub const TEMPLATE_NODE_TYPE: &str = "Template";

/// Context key holding `{ node_id: { "template", "version" } }` for the run.
pub const LOCK_CONTEXT_KEY: &str = "templates";

/// Node types a template can wrap. Containers (Loop) and triggers are excluded.
pub const TEMPLATABLE_NODE_TYPES: &[&str] =
    &["HttpRequest", "ServiceCall", "WorkflowCall", "SetVariable", "If", "Switch"];

/// A workflow node that references a template.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateRef {
    pub node_id: String,
    pub slug: String,
    /// `None` follows the latest version.
    pub version: Option<i32>,
    pub params: Map<String, Value>,
}

impl TemplateRef {
    pub fn usage(&self) -> TemplateUsage {
        TemplateUsage {
            node_id: self.node_id.clone(),
            slug: self.slug.clone(),
            version: self.version,
        }
    }
}

fn nodes_of(definition: &Value) -> Option<&Vec<Value>> {
    definition
        .get("data")
        .and_then(|d| d.get("nodes"))
        .or_else(|| definition.get("nodes"))
        .and_then(Value::as_array)
}

fn nodes_of_mut(definition: &mut Value) -> Option<&mut Vec<Value>> {
    if definition.get("data").and_then(|d| d.get("nodes")).is_some() {
        definition.get_mut("data")?.get_mut("nodes")?.as_array_mut()
    } else {
        definition.get_mut("nodes")?.as_array_mut()
    }
}

fn is_template_node(node: &Value) -> bool {
    node.get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| to_pascal_case(t) == TEMPLATE_NODE_TYPE)
}

fn parse_version(v: Option<&Value>) -> Result<Option<i32>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() || s.trim() == "latest" => Ok(None),
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .and_then(|n| i32::try_from(n).ok())
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| format!("invalid template version: {v}")),
    }
}

/// The template references in a workflow definition.
pub fn find_refs(definition: &Value) -> Result<Vec<TemplateRef>, String> {
    let Some(nodes) = nodes_of(definition) else {
        return Ok(Vec::new());
    };
    let mut refs = Vec::new();
    for node in nodes.iter().filter(|n| is_template_node(n)) {
        let node_id = node.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let data = node.get("data").cloned().unwrap_or(Value::Null);
        let slug = data
            .get("template")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("node {node_id}: data.template is required"))?
            .to_string();
        let version = parse_version(data.get("version")).map_err(|e| format!("node {node_id}: {e}"))?;
        let params = match data.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err(format!("node {node_id}: data.params must be an object")),
        };
        refs.push(TemplateRef { node_id, slug, version, params });
    }
    Ok(refs)
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_slug(s: &str) -> bool {
    s.len() <= 100
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Names referenced as `params.<name>` inside the `{{ }}` expressions of `value`.
fn referenced_params(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::String(s) => {
            for (_, _, expr) in expression::find_expressions(s) {
                let bytes = expr.as_bytes();
                let mut from = 0;
                while let Some(pos) = expr[from..].find("params.") {
                    let at = from + pos;
                    let preceded_by_ident = at > 0
                        && matches!(bytes[at - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.');
                    let rest = &expr[at + "params.".len()..];
                    let name: String =
                        rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if !preceded_by_ident && !name.is_empty() {
                        out.insert(name);
                    }
                    from = at + "params.".len();
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| referenced_params(v, out)),
        Value::Object(m) => m.values().for_each(|v| referenced_params(v, out)),
        _ => {}
    }
}

/// Validate a template before it is published, normalizing its node type to PascalCase.
///
/// `params` declares the parameters: `{ "<name>": { "required"?: bool, "default"?: any,
/// "label"?: string, "description"?: string } }`. Every `{{ params.x }}` in the config must
/// be declared.
pub fn validate_content(slug: &str, content: &mut NodeTemplateContent) -> Result<(), String> {
    if !is_slug(slug) {
        return Err(format!(
            "invalid slug {slug:?}: use lowercase letters, digits, '-' or '_' (max 100 chars)"
        ));
    }
    content.node_type = to_pascal_case(content.node_type.trim());
    if !TEMPLATABLE_NODE_TYPES.contains(&content.node_type.as_str()) {
        return Err(format!(
            "node_type {:?} cannot be a template (allowed: {})",
            content.node_type,
            TEMPLATABLE_NODE_TYPES.join(", ")
        ));
    }
    if !content.config.is_object() {
        return Err("config must be an object".into());
    }
    if content.params.is_null() {
        content.params = Value::Object(Map::new());
    }
    let params = content.params.as_object().ok_or("params must be an object")?;
    for (name, spec) in params {
        if !is_identifier(name) {
            return Err(format!("invalid param name {name:?}: use letters, digits and '_'"));
        }
        let spec = spec
            .as_object()
            .ok_or_else(|| format!("param {name}: must be an object"))?;
        if spec.get("required").is_some_and(|r| !r.is_boolean()) {
            return Err(format!("param {name}: required must be true or false"));
        }
        for key in ["label", "description"] {
            if spec.get(key).is_some_and(|v| !v.is_string() && !v.is_null()) {
                return Err(format!("param {name}: {key} must be a string"));
            }
        }
    }
    let mut used = HashSet::new();
    referenced_params(&content.config, &mut used);
    let mut undeclared: Vec<_> = used.into_iter().filter(|p| !params.contains_key(p)).collect();
    if !undeclared.is_empty() {
        undeclared.sort();
        return Err(format!("config uses undeclared params: {}", undeclared.join(", ")));
    }
    Ok(())
}

fn is_required(spec: &Value) -> bool {
    spec.get("required").and_then(Value::as_bool).unwrap_or(false) && spec.get("default").is_none()
}

/// Save-time check of the params a node gives a template: all required ones present and no
/// undeclared ones.
pub fn check_params(schema: &Value, given: &Map<String, Value>) -> Result<(), String> {
    let empty = Map::new();
    let schema = schema.as_object().unwrap_or(&empty);
    let mut unknown: Vec<_> = given.keys().filter(|k| !schema.contains_key(*k)).cloned().collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!("unknown params: {}", unknown.join(", ")));
    }
    missing_required(schema, given)
}

fn missing_required(schema: &Map<String, Value>, given: &Map<String, Value>) -> Result<(), String> {
    let mut missing: Vec<_> = schema
        .iter()
        .filter(|(k, spec)| is_required(spec) && given.get(*k).is_none_or(Value::is_null))
        .map(|(k, _)| k.clone())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        missing.sort();
        Err(format!("missing required params: {}", missing.join(", ")))
    }
}

/// The params a run uses: declared defaults overlaid with the node's values. Values for
/// params the template no longer declares are dropped.
pub fn effective_params(schema: &Value, given: &Map<String, Value>) -> Result<Value, String> {
    let empty = Map::new();
    let schema = schema.as_object().unwrap_or(&empty);
    missing_required(schema, given)?;
    let mut out = Map::new();
    for (name, spec) in schema {
        match given.get(name).filter(|v| !v.is_null()) {
            Some(v) => out.insert(name.clone(), v.clone()),
            None => out.insert(name.clone(), spec.get("default").cloned().unwrap_or(Value::Null)),
        };
    }
    Ok(Value::Object(out))
}

/// Replace each reference node with its template's node. The node keeps its id, `parentId`
/// and edges; its type and config come from the template, plus `templateParams` (read by
/// the executor as `params`) and `template` (`{ slug, version }`, for traceability).
pub fn expand(
    definition: &Value,
    refs: &[TemplateRef],
    picked: &HashMap<String, &NodeTemplate>,
) -> Result<Value, String> {
    let mut out = definition.clone();
    let by_id: HashMap<&str, &TemplateRef> = refs.iter().map(|r| (r.node_id.as_str(), r)).collect();
    let Some(nodes) = nodes_of_mut(&mut out) else {
        return Ok(out);
    };
    for node in nodes.iter_mut().filter(|n| is_template_node(n)) {
        let id = node.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let r = by_id.get(id.as_str()).ok_or_else(|| format!("node {id}: unresolved template"))?;
        let t = picked
            .get(&id)
            .ok_or_else(|| format!("node {id}: template {} not found", r.slug))?;
        let params = effective_params(&t.params, &r.params)
            .map_err(|e| format!("node {id} (template {} v{}): {e}", t.slug, t.version))?;
        let obj = node.as_object_mut().ok_or("node must be an object")?;
        obj.insert("type".into(), Value::String(t.node_type.clone()));
        obj.insert("data".into(), t.config.clone());
        obj.insert("templateParams".into(), params);
        obj.insert(
            "template".into(),
            serde_json::json!({ "slug": t.slug, "version": t.version }),
        );
    }
    Ok(out)
}

/// A definition with its template references expanded. Borrows the original when it has
/// none, so workflows without templates are not copied.
#[derive(Debug, Clone)]
pub struct Resolved<'a> {
    pub definition: Cow<'a, Value>,
    /// `{ node_id: { "template": slug, "version": n } }` for the references expanded.
    pub lock: Value,
}

/// The version a reference should use: the one recorded in `lock` (a resumed step-mode
/// run), else the pinned one, else `None` for latest.
fn wanted_version(r: &TemplateRef, lock: Option<&Value>) -> Option<i32> {
    lock.and_then(|l| l.get(&r.node_id))
        .filter(|e| e.get("template").and_then(Value::as_str) == Some(r.slug.as_str()))
        .and_then(|e| e.get("version"))
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .or(r.version)
}

fn pick<'a>(candidates: &'a [NodeTemplate], slug: &str, version: Option<i32>) -> Option<&'a NodeTemplate> {
    candidates.iter().find(|t| {
        t.slug == slug
            && match version {
                Some(v) => t.version == v,
                None => t.is_latest,
            }
    })
}

async fn load(
    pool: &sqlx::PgPool,
    tenant: &str,
    refs: &[TemplateRef],
    lock: Option<&Value>,
) -> Result<HashMap<String, NodeTemplate>, String> {
    let slugs: Vec<String> = refs.iter().map(|r| r.slug.clone()).collect::<HashSet<_>>().into_iter().collect();
    let versions: Vec<i32> = refs.iter().filter_map(|r| wanted_version(r, lock)).collect();
    let candidates = storage::fetch_node_templates(pool, tenant, &slugs, &versions)
        .await
        .map_err(|e| e.to_string())?;
    let mut picked = HashMap::new();
    for r in refs {
        let version = wanted_version(r, lock);
        let t = pick(&candidates, &r.slug, version).ok_or_else(|| match version {
            Some(v) => format!("node {}: template {} v{v} not found", r.node_id, r.slug),
            None => format!("node {}: template {} not found", r.node_id, r.slug),
        })?;
        picked.insert(r.node_id.clone(), t.clone());
    }
    Ok(picked)
}

/// Expand every template reference in `definition` for a run. `lock` is the `templates`
/// entry of a run's context, to re-resolve exactly the versions it started with.
pub async fn resolve_definition<'a>(
    pool: &sqlx::PgPool,
    tenant: &str,
    definition: &'a Value,
    lock: Option<&Value>,
) -> Result<Resolved<'a>, String> {
    let refs = find_refs(definition)?;
    if refs.is_empty() {
        return Ok(Resolved { definition: Cow::Borrowed(definition), lock: Value::Object(Map::new()) });
    }
    let loaded = load(pool, tenant, &refs, lock).await?;
    let picked: HashMap<String, &NodeTemplate> = loaded.iter().map(|(k, v)| (k.clone(), v)).collect();
    let definition = expand(definition, &refs, &picked)?;
    let lock = loaded
        .iter()
        .map(|(id, t)| (id.clone(), serde_json::json!({ "template": t.slug, "version": t.version })))
        .collect();
    Ok(Resolved { definition: Cow::Owned(definition), lock: Value::Object(lock) })
}

/// Record the versions a run resolved in its context (under [`LOCK_CONTEXT_KEY`]), so it
/// is visible in the execution and step mode can re-resolve the same versions.
pub fn record_lock(context: &mut Value, lock: Value) {
    let has_entries = lock.as_object().is_some_and(|l| !l.is_empty());
    if let (true, Some(obj)) = (has_entries, context.as_object_mut()) {
        obj.insert(LOCK_CONTEXT_KEY.to_string(), lock);
    }
}

/// Validate a workflow's template references before it is saved: each template (and pinned
/// version) must exist and get valid params. Returns the usages to record.
pub async fn validate_workflow(
    pool: &sqlx::PgPool,
    tenant: &str,
    definition: &Value,
) -> Result<Vec<TemplateUsage>, String> {
    let refs = find_refs(definition)?;
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let loaded = load(pool, tenant, &refs, None).await?;
    for r in &refs {
        let t = &loaded[&r.node_id];
        check_params(&t.params, &r.params)
            .map_err(|e| format!("node {} (template {} v{}): {e}", r.node_id, t.slug, t.version))?;
    }
    Ok(refs.iter().map(TemplateRef::usage).collect())
}

/// Nodes that follow the latest version and would break if `params` became the latest
/// schema (a required param they do not set), as `"<workflow> v<n> / <node>: <reason>"`.
pub fn breaking_usages(slug: &str, params: &Value, usages: &[storage::TemplateUsageRow]) -> Vec<String> {
    let mut out = Vec::new();
    for u in usages.iter().filter(|u| u.version.is_none()) {
        let given = find_refs(&u.definition)
            .ok()
            .and_then(|refs| refs.into_iter().find(|r| r.node_id == u.node_id && r.slug == slug))
            .map(|r| r.params)
            .unwrap_or_default();
        let empty = Map::new();
        if let Err(e) = missing_required(params.as_object().unwrap_or(&empty), &given) {
            out.push(format!("{} v{} / {}: {e}", u.workflow_name, u.workflow_version, u.node_id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template(slug: &str, version: i32, is_latest: bool, config: Value, params: Value) -> NodeTemplate {
        NodeTemplate {
            id: uuid::Uuid::new_v4(),
            tenant: "t".into(),
            slug: slug.into(),
            version,
            is_latest,
            name: None,
            description: None,
            node_type: "ServiceCall".into(),
            config,
            params,
            created_at: chrono::Utc::now(),
        }
    }

    fn content(node_type: &str, config: Value, params: Value) -> NodeTemplateContent {
        NodeTemplateContent { name: None, description: None, node_type: node_type.into(), config, params }
    }

    #[test]
    fn find_refs_reads_slug_version_and_params() {
        let def = json!({ "data": { "edges": [], "nodes": [
            { "id": "a", "type": "httpTrigger", "data": {} },
            { "id": "b", "type": "template", "data": { "template": "create-ticket", "version": 3, "params": { "id": "{{ Webhook.body.id }}" } } },
            { "id": "c", "type": "template", "data": { "template": "notify", "version": "latest" } }
        ] } });
        let refs = find_refs(&def).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].slug, "create-ticket");
        assert_eq!(refs[0].version, Some(3));
        assert_eq!(refs[0].params["id"], "{{ Webhook.body.id }}");
        assert_eq!(refs[1].version, None);
        assert!(refs[1].params.is_empty());
    }

    #[test]
    fn find_refs_rejects_bad_references() {
        let missing = json!({ "nodes": [{ "id": "b", "type": "template", "data": {} }], "edges": [] });
        assert!(find_refs(&missing).unwrap_err().contains("data.template is required"));
        let bad_version = json!({ "nodes": [{ "id": "b", "type": "template", "data": { "template": "x", "version": 0 } }], "edges": [] });
        assert!(find_refs(&bad_version).is_err());
    }

    #[test]
    fn validate_content_normalizes_type_and_checks_params() {
        let mut ok = content(
            "serviceCall",
            json!({ "rawBody": { "id": "{{ params.ticketId }}", "tenant": "{{ global.TENANT }}" } }),
            json!({ "ticketId": { "required": true, "label": "Ticket" } }),
        );
        validate_content("create-ticket", &mut ok).unwrap();
        assert_eq!(ok.node_type, "ServiceCall");

        let mut undeclared = content("serviceCall", json!({ "x": "{{ params.nope }}" }), json!({}));
        assert!(validate_content("s", &mut undeclared).unwrap_err().contains("nope"));

        let mut loop_node = content("loop", json!({}), json!({}));
        assert!(validate_content("s", &mut loop_node).is_err());

        let mut bad_slug = content("serviceCall", json!({}), json!({}));
        assert!(validate_content("Bad Slug", &mut bad_slug).is_err());

        let mut bad_param = content("serviceCall", json!({}), json!({ "bad-name": {} }));
        assert!(validate_content("s", &mut bad_param).is_err());
    }

    #[test]
    fn referenced_params_ignores_lookalikes() {
        let mut used = HashSet::new();
        referenced_params(
            &json!({ "a": "{{ params.one }} and {{ nodes.x.params.two }}", "b": ["{{ myparams.three }}"] }),
            &mut used,
        );
        assert_eq!(used, HashSet::from(["one".to_string()]));
    }

    #[test]
    fn params_apply_defaults_and_require_values() {
        let schema = json!({ "id": { "required": true }, "priority": { "default": "low" }, "note": {} });
        let given: Map<String, Value> = json!({ "id": "{{ Webhook.body.id }}" }).as_object().unwrap().clone();
        assert_eq!(
            effective_params(&schema, &given).unwrap(),
            json!({ "id": "{{ Webhook.body.id }}", "priority": "low", "note": null })
        );
        assert!(effective_params(&schema, &Map::new()).unwrap_err().contains("id"));
        // Save-time checks also reject params the template does not declare.
        let extra: Map<String, Value> = json!({ "id": 1, "zzz": 2 }).as_object().unwrap().clone();
        assert!(check_params(&schema, &extra).unwrap_err().contains("zzz"));
        // Runtime drops them instead, so removing a param does not break running workflows.
        assert!(effective_params(&schema, &extra).unwrap().get("zzz").is_none());
    }

    #[test]
    fn expand_replaces_reference_nodes_and_keeps_structure() {
        let def = json!({ "data": { "persistence": "full", "edges": [{ "source": "loop", "target": "after" }], "nodes": [
            { "id": "loop", "type": "loop", "data": { "items": "{{ Webhook.body.rows }}" } },
            { "id": "t", "type": "template", "parentId": "loop", "position": { "x": 1, "y": 2 },
              "data": { "template": "create-ticket", "params": { "id": "{{ item.id }}" } } }
        ] } });
        let refs = find_refs(&def).unwrap();
        let tpl = template("create-ticket", 2, true, json!({ "serviceSlug": "core", "rawBody": { "id": "{{ params.id }}" } }), json!({ "id": { "required": true } }));
        let picked = HashMap::from([("t".to_string(), &tpl)]);
        let out = expand(&def, &refs, &picked).unwrap();
        let node = &out["data"]["nodes"][1];
        assert_eq!(node["type"], "ServiceCall");
        assert_eq!(node["parentId"], "loop");
        assert_eq!(node["position"], json!({ "x": 1, "y": 2 }));
        assert_eq!(node["data"]["serviceSlug"], "core");
        assert_eq!(node["templateParams"], json!({ "id": "{{ item.id }}" }));
        assert_eq!(node["template"], json!({ "slug": "create-ticket", "version": 2 }));
        assert_eq!(out["data"]["persistence"], "full");
        assert_eq!(out["data"]["edges"], def["data"]["edges"]);
    }

    #[test]
    fn lock_overrides_latest_for_resumed_runs() {
        let r = TemplateRef { node_id: "t".into(), slug: "s".into(), version: None, params: Map::new() };
        assert_eq!(wanted_version(&r, None), None);
        let lock = json!({ "t": { "template": "s", "version": 1 } });
        assert_eq!(wanted_version(&r, Some(&lock)), Some(1));
        // A lock for a different template (the node was repointed) is ignored.
        let stale = json!({ "t": { "template": "other", "version": 1 } });
        assert_eq!(wanted_version(&r, Some(&stale)), None);

        let candidates = vec![
            template("s", 1, false, json!({}), json!({})),
            template("s", 2, true, json!({}), json!({})),
        ];
        assert_eq!(pick(&candidates, "s", None).unwrap().version, 2);
        assert_eq!(pick(&candidates, "s", Some(1)).unwrap().version, 1);
        assert!(pick(&candidates, "s", Some(3)).is_none());
    }

    #[test]
    fn breaking_usages_flags_floating_nodes_missing_new_required_params() {
        let def = json!({ "nodes": [
            { "id": "a", "type": "template", "data": { "template": "s", "params": { "id": 1 } } },
            { "id": "b", "type": "template", "data": { "template": "s", "version": 1 } }
        ], "edges": [] });
        let usage = |node_id: &str, version: Option<i32>| storage::TemplateUsageRow {
            workflow_id: uuid::Uuid::nil(),
            workflow_name: "wf".into(),
            workflow_version: 1,
            workflow_is_latest: true,
            node_id: node_id.into(),
            version,
            definition: def.clone(),
        };
        let usages = vec![usage("a", None), usage("b", Some(1))];
        assert!(breaking_usages("s", &json!({ "id": { "required": true } }), &usages).is_empty());
        let broken = breaking_usages("s", &json!({ "id": {}, "priority": { "required": true } }), &usages);
        assert_eq!(broken, vec!["wf v1 / a: missing required params: priority".to_string()]);
    }
}
