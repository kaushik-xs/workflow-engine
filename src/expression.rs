//! `{{ expression }}` templating using JMESPath only (safe, no arbitrary code).
//!
//! Execution: (1) Detect `{{ }}` (2) Extract expression (3) Compile JMESPath with cache
//! (4) Evaluate against context (5) Replace value. Used in headers, body, path, etc. for all node types.
//! Context shape: `{ "current": {}, "nodes": {}, "env": {} }` plus Webhook, etc.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

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
    let expr = get_compiled(expression.trim())?;
    let json_str = serde_json::to_string(context).map_err(|e| e.to_string())?;
    let variable = jmespath::Variable::from_json(&json_str).map_err(|e| e.to_string())?;
    let result = expr.search(variable).map_err(|e| e.to_string())?;
    serde_json::to_value(&result).map_err(|e| e.to_string())
}

/// Find all {{ expression }} placeholders in a string. Returns (start, end, expression text).
pub fn find_expressions(s: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if i + 4 <= bytes.len() && &bytes[i..i + 2] == b"{{" {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            let expr_start = i;
            while i < bytes.len() {
                if i + 2 <= bytes.len() && &bytes[i..i + 2] == b"}}" {
                    let expr = String::from_utf8_lossy(&bytes[expr_start..i]).trim().to_string();
                    out.push((start, i + 2, expr));
                    i += 2;
                    break;
                }
                i += 1;
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

/// Recursively interpolate all string values in a JSON value (in place).
pub fn interpolate_value(value: &mut Value, context: &Value) -> Result<(), String> {
    match value {
        Value::String(s) => {
            let new_s = interpolate_string(s, context)?;
            *s = new_s;
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
}
