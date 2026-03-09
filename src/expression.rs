use serde_json::Value;

/// Compile JMESPath expression.
fn compile_cached(expr_str: &str) -> Result<jmespath::Expression<'_>, String> {
    jmespath::compile(expr_str).map_err(|e| e.to_string())
}

/// Evaluate a single JMESPath expression against context (JSON value).
pub fn evaluate(expression: &str, context: &Value) -> Result<Value, String> {
    let expr = compile_cached(expression.trim())?;
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
