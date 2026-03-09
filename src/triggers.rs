use axum::body::Bytes;
use axum::http::HeaderMap;
use serde_json::Value;

pub fn webhook_context_from_request(body: Bytes, headers: &HeaderMap) -> Value {

    let body_value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut headers_map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        if let Ok(s) = v.to_str() {
            headers_map.insert(k.to_string(), Value::String(s.to_string()));
        }
    }
    let webhook = serde_json::json!({
        "body": body_value,
        "headers": headers_map
    });
    serde_json::json!({ "Webhook": webhook })
}
