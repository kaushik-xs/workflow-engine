//! `{{ expression }}` templating using JMESPath only (safe, no arbitrary code).
//!
//! Execution: (1) Detect `{{ }}` (2) Extract expression (3) Compile JMESPath with cache
//! (4) Evaluate against context (5) Replace value. Used in headers, body, path, etc. for all node types.
//! Context shape: `{ "current": {}, "nodes": {}, "env": {} }` plus Webhook, etc.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing;

/// Global cache of compiled JMESPath expressions. JMESPath is pure data lookup only (no arbitrary code).
static COMPILED_CACHE: std::sync::OnceLock<Mutex<HashMap<String, jmespath::Expression<'static>>>> =
    std::sync::OnceLock::new();

fn compiled_cache() -> &'static Mutex<HashMap<String, jmespath::Expression<'static>>> {
    COMPILED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Compile JMESPath expression, using cache on hit. Expressions are JMESPath-only (safe, no arbitrary code).
fn get_compiled(expr_str: &str) -> Result<jmespath::Expression<'static>, String> {
    let key = expr_str.to_string();
    let cache = compiled_cache();
    let mut guard = cache.lock().map_err(|e| e.to_string())?;
    if let Some(expr) = guard.get(&key) {
        return Ok(expr.clone());
    }
    let expr = jmespath::compile(expr_str).map_err(|e| e.to_string())?;
    guard.insert(key, expr.clone());
    Ok(expr)
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
            "env": { "HOME": "/home" }
        });
        assert_eq!(evaluate("current.status", &ctx).unwrap(), serde_json::json!("ok"));
        assert_eq!(evaluate("nodes.n1.price", &ctx).unwrap(), serde_json::json!(42));
        assert_eq!(evaluate("env.HOME", &ctx).unwrap(), serde_json::json!("/home"));
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
}
