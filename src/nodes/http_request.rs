use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use tracing;

use crate::expression;

/// Default HTTP timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub struct HttpRequestExecutor {
    client: reqwest::Client,
}

impl Default for HttpRequestExecutor {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[async_trait]
impl NodeExecutor for HttpRequestExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        mut input: Value,
        mut config: Value,
    ) -> Result<Value, String> {
        let method = config
            .get("method")
            .or_else(|| config.get("Method"))
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        expression::interpolate_value(&mut input, &ctx.context)?;
        expression::interpolate_value(&mut config, &ctx.context)?;

        let url = config
            .get("url")
            .or_else(|| config.get("path"))
            .and_then(Value::as_str)
            .ok_or("HttpRequest config must have url or path")?;

        let body = input
            .get("body")
            .or_else(|| config.get("body"))
            .or_else(|| config.get("payload"))
            .cloned()
            .unwrap_or(Value::Null);
        let headers = input
            .get("headers")
            .or_else(|| config.get("header"))
            .or_else(|| config.get("headers"))
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let request = serde_json::json!({
            "method": method,
            "url": url,
            "headers": headers,
            "body": body
        });
        tracing::info!(
            execution_id = %ctx.execution_id,
            node_type = "httpRequest",
            method = %method,
            url = %url,
            "http request"
        );
        tracing::debug!(execution_id = %ctx.execution_id, request = ?request, "http request body");

        let mut req = match method.as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "PATCH" => self.client.patch(url),
            "DELETE" => self.client.delete(url),
            _ => self.client.get(url),
        };

        if body != Value::Null {
            req = match body {
                Value::String(s) => {
                    req = req.header("Content-Type", "application/json");
                    req.body(s.into_bytes())
                }
                _ => req.json(&body),
            };
        } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            req = req.body(vec![]);
        }
        if let Some(map) = headers.as_object() {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body_value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });

        Ok(serde_json::json!({
            "status": status,
            "body": body_value,
            "request": request
        }))
    }
}
