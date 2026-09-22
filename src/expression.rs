//! `{{ expression }}` templating using JMESPath only (safe, no arbitrary code).
//!
//! Execution: (1) Detect `{{ }}` (2) Extract expression (3) Compile JMESPath with cache
//! (4) Evaluate against context (5) Replace value. Used in headers, body, path, etc. for all node types.
//! Context shape: `{ "current": {}, "nodes": {}, "global": {}, "local": {} }` plus Webhook, etc.

use jmespath::functions::{ArgumentType, CustomFunction, Signature};
use jmespath::{ErrorReason, Rcvar, Runtime};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use tracing;

/// Global cache of compiled JMESPath expressions. JMESPath is pure data lookup only (no arbitrary code).
static COMPILED_CACHE: std::sync::OnceLock<Mutex<HashMap<String, jmespath::Expression<'static>>>> =
    std::sync::OnceLock::new();

/// Runtime carrying the JMESPath built-ins plus our custom functions (e.g. `parse_json`).
///
/// `jmespath::compile` uses the crate's default runtime, which only knows the built-in functions.
/// To expose custom functions we compile against this runtime instead. It is `'static`, so compiled
/// expressions borrowing it are `Expression<'static>` and can be cached.
static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        let mut rt = Runtime::new();
        rt.register_builtin_functions();

        // parse_json(string) -> value
        // Parse a JSON-encoded string into a real value (object/array/number/bool/null/string).
        // Useful when an incoming field holds JSON as text, e.g. `parse_json(Webhook.body.assignees)`
        // to turn the string `"[\"id1\",\"id2\"]"` into an array before indexing.
        rt.register_function(
            "parse_json",
            Box::new(CustomFunction::new(
                Signature::new(vec![ArgumentType::String], None),
                Box::new(|args: &[Rcvar], _ctx| {
                    // Signature validation guarantees a single string argument.
                    let raw = args[0].as_string().expect("validated as string");
                    match jmespath::Variable::from_json(raw) {
                        Ok(v) => Ok(Rcvar::new(v)),
                        Err(e) => Err(jmespath::JmespathError::new(
                            raw,
                            0,
                            ErrorReason::Parse(format!("parse_json: invalid JSON: {e}")),
                        )),
                    }
                }),
            )),
        );

        // zip(array, array, ...) -> array of tuples, truncated to the shortest input (like Python zip).
        // Standard JMESPath cannot correlate two parallel arrays by index (`[*]` exposes only the
        // current element, never its position), so this closes that gap. Given two parallel arrays
        // `zip([a, b], [1, 2])` yields `[[a, 1], [b, 2]]`; callers then shape each pair via a
        // multiselect hash `[*].{step: [0], ticketId: [1]}` or `[*].merge([0], {ticketId: [1]})`.
        rt.register_function(
            "zip",
            Box::new(CustomFunction::new(
                // At least two arrays; the variadic slot allows three or more.
                Signature::new(
                    vec![ArgumentType::Array, ArgumentType::Array],
                    Some(ArgumentType::Array),
                ),
                Box::new(|args: &[Rcvar], _ctx| {
                    // Signature validation guarantees every argument is an array.
                    let arrays: Vec<&Vec<Rcvar>> = args
                        .iter()
                        .map(|a| a.as_array().expect("validated as array"))
                        .collect();
                    let len = arrays.iter().map(|a| a.len()).min().unwrap_or(0);
                    let mut out: Vec<Rcvar> = Vec::with_capacity(len);
                    for i in 0..len {
                        let tuple: Vec<Rcvar> = arrays.iter().map(|a| a[i].clone()).collect();
                        out.push(Rcvar::new(jmespath::Variable::Array(tuple)));
                    }
                    Ok(Rcvar::new(jmespath::Variable::Array(out)))
                }),
            )),
        );

        // broadcast(array_or_object_or_null, object) -> array
        // Merge a fixed set of fields into EVERY element of an array. Closes the gap that
        // JMESPath projections drop the outer scope: inside `parent[*]` you can read the
        // parent's `id`, but `parent[*].children[*]` can no longer see it. `broadcast` lets
        // you stamp parent-level fields onto each child row while still at parent scope, e.g.
        //   items[*].broadcast(notifyWorkflowSteps, {id: id})[]
        // yields one flat array of child rows, each carrying its parent's id.
        //
        // First arg is tolerant of shape: an array is used as-is; a single object is treated
        // as a one-element array; null/absent yields `[]`. The fields object (second arg) is
        // merged over each element, so its keys win on collision (same order as built-in `merge`).
        // Elements must be objects; a non-object element is an error.
        rt.register_function(
            "broadcast",
            Box::new(CustomFunction::new(
                Signature::new(vec![ArgumentType::Any, ArgumentType::Object], None),
                Box::new(|args: &[Rcvar], _ctx| {
                    // Normalize arg0 to a list of elements: array as-is, object -> [object],
                    // null -> []. Any other scalar type cannot be broadcast into.
                    let elements: Vec<Rcvar> = if let Some(arr) = args[0].as_array() {
                        arr.clone()
                    } else if args[0].as_object().is_some() {
                        vec![args[0].clone()]
                    } else if args[0].is_null() {
                        Vec::new()
                    } else {
                        return Err(jmespath::JmespathError::new(
                            "",
                            0,
                            ErrorReason::Parse(
                                "broadcast: first argument must be an array, object, or null"
                                    .to_string(),
                            ),
                        ));
                    };
                    // Signature validation guarantees arg1 is an object.
                    let fields = args[1].as_object().expect("validated as object");

                    let mut out: Vec<Rcvar> = Vec::with_capacity(elements.len());
                    for el in elements {
                        let base = el.as_object().ok_or_else(|| {
                            jmespath::JmespathError::new(
                                "",
                                0,
                                ErrorReason::Parse(
                                    "broadcast: every element must be an object".to_string(),
                                ),
                            )
                        })?;
                        // Element fields first, then the broadcast fields override on collision.
                        let mut merged: BTreeMap<String, Rcvar> = base.clone();
                        for (k, v) in fields.iter() {
                            merged.insert(k.clone(), v.clone());
                        }
                        out.push(Rcvar::new(jmespath::Variable::Object(merged)));
                    }
                    Ok(Rcvar::new(jmespath::Variable::Array(out)))
                }),
            )),
        );

        rt
    })
}

/// Compile JMESPath expression, using cache on hit. Expressions are JMESPath-only (safe, no arbitrary code).
fn get_compiled(expr_str: &str) -> Result<jmespath::Expression<'static>, String> {
    let key = expr_str.to_string();
    let cache = compiled_cache();
    let mut guard = cache.lock().map_err(|e| e.to_string())?;
    if let Some(expr) = guard.get(&key) {
        return Ok(expr.clone());
    }
    let expr = runtime().compile(expr_str).map_err(|e| e.to_string())?;
    guard.insert(key, expr.clone());
    Ok(expr)
}

fn compiled_cache() -> &'static Mutex<HashMap<String, jmespath::Expression<'static>>> {
    COMPILED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Evaluate a single JMESPath expression against context (JSON value).
pub fn evaluate(expression: &str, context: &Value) -> Result<Value, String> {
    let expr_str = expression.trim();
    tracing::debug!(expression = %expr_str, "evaluating jmespath expression");

    let expr = get_compiled(expr_str)?;
    let json_str = serde_json::to_string(context).map_err(|e| e.to_string())?;
    let variable = jmespath::Variable::from_json(&json_str).map_err(|e| e.to_string())?;
    let result = expr.search(variable).map_err(|e| {
        let msg = e.to_string();
        tracing::debug!(expression = %expr_str, error = %msg, "jmespath evaluation error");
        msg
    })?;
    let value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
    tracing::debug!(expression = %expr_str, result = ?value, "jmespath evaluation result");
    Ok(value)
}

/// Find all {{ expression }} placeholders in a string. Returns (start, end, expression text).
///
/// The closing `}}` is matched at brace-depth 0 so that JMESPath multiselect hashes — which
/// contain (and often end in) single `{`/`}` — are captured whole. Braces inside JMESPath string
/// literals (`` ` ``, `'`, `"`) are ignored so literal `{`/`}` don't skew the depth count.
pub fn find_expressions(s: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if i + 2 <= n && &bytes[i..i + 2] == b"{{" {
            let start = i;
            i += 2;
            while i < n && bytes[i] == b' ' {
                i += 1;
            }
            let expr_start = i;
            let mut depth: i32 = 0;
            let mut close: Option<usize> = None;
            while i < n {
                let c = bytes[i];
                // Skip over string literals so braces inside them don't affect depth.
                if c == b'`' || c == b'\'' || c == b'"' {
                    let quote = c;
                    i += 1;
                    while i < n {
                        if bytes[i] == b'\\' && i + 1 < n {
                            i += 2;
                            continue;
                        }
                        if bytes[i] == quote {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                if c == b'}' && depth == 0 && i + 1 < n && bytes[i + 1] == b'}' {
                    close = Some(i);
                    break;
                }
                if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                }
                i += 1;
            }
            match close {
                Some(pos) => {
                    let expr = String::from_utf8_lossy(&bytes[expr_start..pos]).trim().to_string();
                    out.push((start, pos + 2, expr));
                    i = pos + 2;
                }
                // No closing `}}` — leave the rest as literal text.
                None => break,
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Replace all {{ expr }} in a string with evaluated values from context.
pub fn interpolate_string(s: &str, context: &Value) -> Result<String, String> {
    let places = find_expressions(s);
    if places.is_empty() {
        return Ok(s.to_string());
    }
    let mut result = String::new();
    let mut last = 0;
    for (start, end, expr) in places {
        result.push_str(&s[last..start]);
        let value = evaluate(&expr, context)?;
        if value.is_string() {
            result.push_str(value.as_str().unwrap_or(""));
        } else {
            result.push_str(&value.to_string());
        }
        last = end;
    }
    result.push_str(&s[last..]);
    Ok(result)
}

/// If the whole string is a single `{{ expr }}` with no surrounding text, return the inner
/// expression. Used to substitute the raw typed result (array/object/number/bool/null) rather
/// than its stringified form.
fn sole_expression(s: &str) -> Option<String> {
    let places = find_expressions(s);
    if places.len() == 1 && places[0].0 == 0 && places[0].1 == s.len() {
        Some(places[0].2.clone())
    } else {
        None
    }
}

/// Recursively interpolate all string values in a JSON value (in place).
///
/// When a string leaf is exactly a single `{{ expr }}` (nothing before or after), it is replaced
/// with the raw typed JMESPath result — so expressions can inject real arrays/objects/numbers into
/// structured fields. Strings with surrounding text (e.g. `"Bearer {{token}}"`) or multiple
/// placeholders keep the string-splicing behaviour.
pub fn interpolate_value(value: &mut Value, context: &Value) -> Result<(), String> {
    match value {
        Value::String(s) => {
            if let Some(expr) = sole_expression(s) {
                *value = evaluate(&expr, context)?;
            } else {
                let new_s = interpolate_string(s, context)?;
                *s = new_s;
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                interpolate_value(v, context)?;
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                interpolate_value(v, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Interpolate a raw JSON body template into a JSON string.
///
/// If `raw` parses as JSON on its own, typed substitution ([`interpolate_value`]) is applied to the
/// parsed structure — so a quoted whole-value placeholder like `"items": "{{ expr }}"` injects a real
/// array/object/number instead of a stringified copy — and the result is reserialized. If `raw` is
/// not valid JSON before substitution (e.g. an unquoted `"count": {{ expr }}`), it falls back to flat
/// string interpolation, which stringifies scalar results in place.
pub fn interpolate_json_body(raw: &str, context: &Value) -> Result<String, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(mut v) => {
            interpolate_value(&mut v, context)?;
            serde_json::to_string(&v).map_err(|e| e.to_string())
        }
        Err(_) => interpolate_string(raw, context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_expressions_detects_placeholders() {
        let s = "hello {{ current.status }} and {{ Webhook.body.customer_name }}";
        let found = find_expressions(s);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].2, "current.status");
        assert_eq!(found[1].2, "Webhook.body.customer_name");
    }

    #[test]
    fn find_expressions_handles_hash_ending_in_braces() {
        // A multiselect hash ends in `}}`, which must not be mistaken for the template terminator.
        let s = "{{ users[*].{a: b, c: {d: e}} }}";
        let found = find_expressions(s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].2, "users[*].{a: b, c: {d: e}}");
        assert_eq!(found[0].0, 0);
        assert_eq!(found[0].1, s.len());
    }

    #[test]
    fn find_expressions_ignores_braces_in_literals() {
        let s = "{{ users[*].{m: `{}`, n: '}}'} }}";
        let found = find_expressions(s);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].2, "users[*].{m: `{}`, n: '}}'}");
    }

    #[test]
    fn evaluate_jmespath_against_context() {
        let ctx = serde_json::json!({
            "current": { "status": "ok" },
            "nodes": { "n1": { "price": 42 } },
            "global": { "API_BASE": "https://api.example.com" },
            "local": { "counter": 3 }
        });
        assert_eq!(evaluate("current.status", &ctx).unwrap(), serde_json::json!("ok"));
        assert_eq!(evaluate("nodes.n1.price", &ctx).unwrap(), serde_json::json!(42));
        assert_eq!(evaluate("global.API_BASE", &ctx).unwrap(), serde_json::json!("https://api.example.com"));
        assert_eq!(evaluate("local.counter", &ctx).unwrap(), serde_json::json!(3));
    }

    #[test]
    fn parse_json_parses_string_to_typed_value() {
        // A field holding a JSON array as text becomes a real array we can index into.
        let ctx = serde_json::json!({
            "Webhook": { "body": { "assignees": "[\"id-1\",\"id-2\"]" } }
        });
        assert_eq!(
            evaluate("parse_json(Webhook.body.assignees)", &ctx).unwrap(),
            serde_json::json!(["id-1", "id-2"])
        );
        assert_eq!(
            evaluate("parse_json(Webhook.body.assignees)[0]", &ctx).unwrap(),
            serde_json::json!("id-1")
        );

        // A field holding a JSON object as text, then projecting a member.
        let ctx = serde_json::json!({
            "Webhook": { "body": { "assignee": "{\"id\":\"abc\",\"name\":\"A\"}" } }
        });
        assert_eq!(
            evaluate("parse_json(Webhook.body.assignee).id", &ctx).unwrap(),
            serde_json::json!("abc")
        );

        // Usable inside an interpolated URL/query string.
        assert_eq!(
            interpolate_string("id=={{ parse_json(Webhook.body.assignee).id }}", &ctx).unwrap(),
            "id==abc"
        );
    }

    #[test]
    fn parse_json_errors_on_malformed_input() {
        let ctx = serde_json::json!({ "Webhook": { "body": { "assignees": "not json" } } });
        assert!(evaluate("parse_json(Webhook.body.assignees)", &ctx).is_err());
    }

    #[test]
    fn zip_correlates_parallel_arrays_by_index() {
        let ctx = serde_json::json!({
            "current": {
                "nmSteps": [{ "name": "s0" }, { "name": "s1" }, { "name": "s2" }],
                "nmTicketIds": ["id0", "id1", "id2"]
            }
        });

        // Raw pairing: [[step, id], ...].
        assert_eq!(
            evaluate("zip(current.nmSteps, current.nmTicketIds)", &ctx).unwrap(),
            serde_json::json!([
                [{ "name": "s0" }, "id0"],
                [{ "name": "s1" }, "id1"],
                [{ "name": "s2" }, "id2"]
            ])
        );

        // Shape each pair into an object via multiselect hash.
        assert_eq!(
            evaluate(
                "zip(current.nmSteps, current.nmTicketIds)[*].{step: [0], ticketId: [1]}",
                &ctx
            )
            .unwrap(),
            serde_json::json!([
                { "step": { "name": "s0" }, "ticketId": "id0" },
                { "step": { "name": "s1" }, "ticketId": "id1" },
                { "step": { "name": "s2" }, "ticketId": "id2" }
            ])
        );

        // Merge ticketId into each step object: [{...step, ticketId}, ...].
        assert_eq!(
            evaluate(
                "zip(current.nmSteps, current.nmTicketIds)[*].merge([0], {ticketId: [1]})",
                &ctx
            )
            .unwrap(),
            serde_json::json!([
                { "name": "s0", "ticketId": "id0" },
                { "name": "s1", "ticketId": "id1" },
                { "name": "s2", "ticketId": "id2" }
            ])
        );
    }

    #[test]
    fn zip_truncates_to_shortest_and_accepts_three_arrays() {
        let ctx = serde_json::json!({
            "a": [1, 2, 3],
            "b": ["x", "y"],
            "c": [true, false, true, false]
        });

        // Truncates to the shortest input (length 2).
        assert_eq!(
            evaluate("zip(a, b, c)", &ctx).unwrap(),
            serde_json::json!([[1, "x", true], [2, "y", false]])
        );
    }

    #[test]
    fn broadcast_stamps_fields_onto_each_element() {
        let ctx = serde_json::json!({
            "id": "parent-1",
            "rows": [{ "step": "a" }, { "step": "b" }]
        });

        // Parent id is broadcast into every row.
        assert_eq!(
            evaluate("broadcast(rows, {id: id})", &ctx).unwrap(),
            serde_json::json!([
                { "step": "a", "id": "parent-1" },
                { "step": "b", "id": "parent-1" }
            ])
        );
    }

    #[test]
    fn broadcast_fields_win_on_collision() {
        let ctx = serde_json::json!({
            "id": "parent-1",
            "rows": [{ "id": "child-own" }]
        });

        // The broadcast object overrides an element's existing key (merge order).
        assert_eq!(
            evaluate("broadcast(rows, {id: id})", &ctx).unwrap(),
            serde_json::json!([{ "id": "parent-1" }])
        );
    }

    #[test]
    fn broadcast_tolerates_object_and_null_first_arg() {
        // A single object is treated as a one-element array.
        let obj_ctx = serde_json::json!({ "id": "p", "one": { "k": 1 } });
        assert_eq!(
            evaluate("broadcast(one, {id: id})", &obj_ctx).unwrap(),
            serde_json::json!([{ "k": 1, "id": "p" }])
        );

        // A null / absent field yields an empty array (not an error).
        let null_ctx = serde_json::json!({ "id": "p" });
        assert_eq!(
            evaluate("broadcast(missing, {id: id})", &null_ctx).unwrap(),
            serde_json::json!([])
        );
    }

    #[test]
    fn broadcast_flattens_children_across_parents() {
        // The real use case: one combined flat array, each row carrying its parent id.
        let ctx = serde_json::json!({
            "items": [
                { "id": "p1", "notify": [{ "n": 1 }, { "n": 2 }] },
                { "id": "p2", "notify": [{ "n": 3 }] }
            ]
        });

        assert_eq!(
            evaluate("items[*].broadcast(notify, {id: id})[]", &ctx).unwrap(),
            serde_json::json!([
                { "n": 1, "id": "p1" },
                { "n": 2, "id": "p1" },
                { "n": 3, "id": "p2" }
            ])
        );
    }

    #[test]
    fn interpolate_string_replaces_placeholders() {
        let ctx = serde_json::json!({ "current": { "status": "running" } });
        let s = "status is {{ current.status }}";
        assert_eq!(interpolate_string(s, &ctx).unwrap(), "status is running");
    }

    #[test]
    fn interpolate_value_recursively() {
        let ctx = serde_json::json!({ "Webhook": { "body": { "customer_name": "Acme" } } });
        let mut val = serde_json::json!({ "greeting": "Hello {{ Webhook.body.customer_name }}" });
        interpolate_value(&mut val, &ctx).unwrap();
        assert_eq!(val["greeting"], "Hello Acme");
    }

    #[test]
    fn sole_expression_detects_whole_value() {
        assert_eq!(sole_expression("{{ current.body.users }}"), Some("current.body.users".to_string()));
        assert_eq!(sole_expression("{{current.body.users}}"), Some("current.body.users".to_string()));
        // surrounding text -> not a sole expression
        assert_eq!(sole_expression("Bearer {{ token }}"), None);
        // multiple placeholders -> not sole
        assert_eq!(sole_expression("{{ a }}{{ b }}"), None);
        // no placeholder
        assert_eq!(sole_expression("plain"), None);
    }

    #[test]
    fn interpolate_value_typed_substitution() {
        let ctx = serde_json::json!({
            "current": { "body": { "ids": [1, 2, 3], "meta": { "k": "v" }, "count": 7, "ok": true } }
        });

        // Whole-value expression yields a real array, not "[1,2,3]"
        let mut arr = serde_json::json!("{{ current.body.ids }}");
        interpolate_value(&mut arr, &ctx).unwrap();
        assert_eq!(arr, serde_json::json!([1, 2, 3]));

        // Object, number, bool preserve their types
        let mut obj = serde_json::json!("{{ current.body.meta }}");
        interpolate_value(&mut obj, &ctx).unwrap();
        assert_eq!(obj, serde_json::json!({ "k": "v" }));

        let mut num = serde_json::json!("{{ current.body.count }}");
        interpolate_value(&mut num, &ctx).unwrap();
        assert_eq!(num, serde_json::json!(7));

        // Surrounding text still stringifies (backward compatible)
        let mut mixed = serde_json::json!("count={{ current.body.count }}");
        interpolate_value(&mut mixed, &ctx).unwrap();
        assert_eq!(mixed, serde_json::json!("count=7"));
    }

    #[test]
    fn interpolate_value_builds_bulk_email_items() {
        // Mirrors the real message-flow use case: reshape a users array into a notifications payload.
        let ctx = serde_json::json!({
            "current": { "body": { "users": [
                { "email": "sridhar.r@regere.ai", "firstName": "Sridhar" },
                { "email": "chandirasegaran.i+1@regere.ai", "firstName": "Chandirasegaran" }
            ] } }
        });

        let mut body = serde_json::json!({
            "items": "{{ current.body.users[*].{channel: 'email', channel_name: 'default', idempotency_key: `null`, metadata: `{}`, priority: `5`, recipient: {address: email, display_name: `null`, locale: `null`}, sync: `false`, template_code: 'milestone-open-intimation', variables: {name: firstName}} }}"
        });
        interpolate_value(&mut body, &ctx).unwrap();

        let items = body["items"].as_array().expect("items should be a real array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["channel"], "email");
        assert_eq!(items[0]["priority"], 5);
        assert_eq!(items[0]["idempotency_key"], serde_json::Value::Null);
        assert_eq!(items[0]["metadata"], serde_json::json!({}));
        assert_eq!(items[0]["sync"], false);
        assert_eq!(items[0]["recipient"]["address"], "sridhar.r@regere.ai");
        assert_eq!(items[0]["recipient"]["display_name"], serde_json::Value::Null);
        assert_eq!(items[0]["variables"]["name"], "Sridhar");
        assert_eq!(items[1]["recipient"]["address"], "chandirasegaran.i+1@regere.ai");
        assert_eq!(items[1]["variables"]["name"], "Chandirasegaran");
    }

    #[test]
    fn interpolate_json_body_injects_real_array_for_quoted_placeholder() {
        // The exact message-flow/bulk case: a quoted whole-value placeholder must render as a real
        // JSON array so the service receives a sequence, not a string containing `"[{...}]"`.
        let ctx = serde_json::json!({
            "current": { "body": { "users": [
                { "email": "sridhar.r@regere.ai", "firstName": "Sridhar", "lastName": "R" },
                { "email": "chandirasegaran.i+1@regere.ai", "firstName": "Chandirasegaran", "lastName": "Ilangovane" }
            ] } }
        });

        let raw = r#"{ "items": "{{ current.body.users[*].{channel: 'email', recipient: {address: email}, template_code: 'milestone-open-intimation', variables: {name: join(' ', [firstName, lastName])}} }}" }"#;

        let rendered = interpolate_json_body(raw, &ctx).unwrap();
        let parsed: Value = serde_json::from_str(&rendered).unwrap();

        let items = parsed["items"].as_array().expect("items must be a real array, not a string");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["channel"], "email");
        assert_eq!(items[0]["recipient"]["address"], "sridhar.r@regere.ai");
        assert_eq!(items[0]["variables"]["name"], "Sridhar R");
        assert_eq!(items[1]["variables"]["name"], "Chandirasegaran Ilangovane");
    }

    #[test]
    fn interpolate_json_body_falls_back_for_unquoted_scalar() {
        // An unquoted placeholder is not valid JSON before substitution, so it falls back to string
        // interpolation — which still yields valid JSON for scalar results.
        let ctx = serde_json::json!({ "current": { "body": { "count": 7 } } });
        let raw = r#"{ "count": {{ current.body.count }} }"#;
        let rendered = interpolate_json_body(raw, &ctx).unwrap();
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["count"], 7);
    }
}
