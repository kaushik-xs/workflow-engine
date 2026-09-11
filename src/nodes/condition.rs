//! Shared condition evaluation for branching nodes (`If`, `Switch`).
//!
//! Values reaching these helpers have already been through `{{ }}` interpolation
//! (see [`crate::executor::run_single_node`]), so they are concrete JSON values, not
//! templates. Comparisons are intentionally loose: a number and its numeric-string form
//! compare equal, and ordering falls back to lexical order for non-numeric operands.

use serde_json::Value;
use std::cmp::Ordering;

/// Evaluate `left <operator> right`. Operator names accept a few common aliases
/// (e.g. `eq`/`==`/`equals`). Unary operators (`exists`, `empty`, …) ignore `right`.
pub fn compare(left: &Value, operator: &str, right: &Value) -> Result<bool, String> {
    match operator.trim() {
        "eq" | "==" | "equals" => Ok(loose_eq(left, right)),
        "ne" | "!=" | "notEquals" | "not_equals" => Ok(!loose_eq(left, right)),
        "gt" | ">" => Ok(num_cmp(left, right)? == Ordering::Greater),
        "gte" | ">=" => Ok(num_cmp(left, right)? != Ordering::Less),
        "lt" | "<" => Ok(num_cmp(left, right)? == Ordering::Less),
        "lte" | "<=" => Ok(num_cmp(left, right)? != Ordering::Greater),
        "contains" => Ok(contains(left, right)),
        "notContains" | "not_contains" => Ok(!contains(left, right)),
        "in" => Ok(contains(right, left)),
        "notIn" | "not_in" => Ok(!contains(right, left)),
        "startsWith" | "starts_with" => Ok(as_str(left).starts_with(&as_str(right))),
        "endsWith" | "ends_with" => Ok(as_str(left).ends_with(&as_str(right))),
        "exists" | "truthy" | "notEmpty" | "not_empty" => Ok(truthy(left)),
        "notExists" | "not_exists" | "falsy" | "empty" => Ok(!truthy(left)),
        other => Err(format!("unknown operator: {other}")),
    }
}

/// Loose truthiness of an interpolated value. Empty collections/strings, zero, null,
/// and the strings `"false"`/`"0"` (any case) are falsy; everything else is truthy.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => {
            let t = s.trim();
            !(t.is_empty() || t.eq_ignore_ascii_case("false") || t == "0")
        }
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Equality with numeric coercion: `5 == "5"` is true; otherwise structural equality.
fn loose_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (to_f64(a), to_f64(b)) {
        return x == y;
    }
    a == b
}

/// Ordering for `<`/`>` family. Numeric when both operands are numbers or numeric
/// strings; otherwise a lexical comparison of their string forms.
fn num_cmp(a: &Value, b: &Value) -> Result<Ordering, String> {
    match (to_f64(a), to_f64(b)) {
        (Some(x), Some(y)) => x
            .partial_cmp(&y)
            .ok_or_else(|| "cannot order NaN values".to_string()),
        _ => Ok(as_str(a).cmp(&as_str(b))),
    }
}

/// True when `haystack` contains `needle`: substring for strings, membership for
/// arrays (loose equality), key presence for objects.
fn contains(haystack: &Value, needle: &Value) -> bool {
    match haystack {
        Value::String(s) => s.contains(&as_str(needle)),
        Value::Array(items) => items.iter().any(|item| loose_eq(item, needle)),
        Value::Object(map) => map.contains_key(&as_str(needle)),
        _ => false,
    }
}

/// Parse a value as f64 when it is a number or a numeric string.
fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Render a value as a plain string for text operators. Strings pass through; null
/// becomes empty; other scalars/collections use their JSON form.
fn as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_comparisons_and_coercion() {
        assert!(compare(&json!(1500), "gt", &json!(1000)).unwrap());
        assert!(compare(&json!("1500"), "gte", &json!(1500)).unwrap());
        assert!(compare(&json!(3), "lt", &json!(10)).unwrap());
        assert!(!compare(&json!(10), "lt", &json!(10)).unwrap());
        assert!(compare(&json!("5"), "eq", &json!(5)).unwrap());
        assert!(compare(&json!("abc"), "ne", &json!(5)).unwrap());
    }

    #[test]
    fn string_and_collection_operators() {
        assert!(compare(&json!("hello world"), "contains", &json!("world")).unwrap());
        assert!(compare(&json!(["a", "b"]), "contains", &json!("a")).unwrap());
        assert!(compare(&json!("b"), "in", &json!(["a", "b"])).unwrap());
        assert!(compare(&json!("prefix-x"), "startsWith", &json!("prefix")).unwrap());
        assert!(compare(&json!({ "k": 1 }), "contains", &json!("k")).unwrap());
    }

    #[test]
    fn unary_truthiness_operators() {
        assert!(compare(&json!("non-empty"), "exists", &Value::Null).unwrap());
        assert!(compare(&json!(""), "empty", &Value::Null).unwrap());
        assert!(compare(&json!([]), "empty", &Value::Null).unwrap());
        assert!(!truthy(&json!("false")));
        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!(0.5)));
        assert!(truthy(&json!({ "a": 1 })));
    }

    #[test]
    fn unknown_operator_errors() {
        assert!(compare(&json!(1), "spaceship", &json!(2)).is_err());
    }
}
