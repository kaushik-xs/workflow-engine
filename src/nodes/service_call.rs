use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tracing;

use crate::expression;
use crate::storage;

/// Build request log (method, url, headers, body) for steps/executions and tracing.
fn request_log(
    method: &str,
    url: &str,
    config: &Value,
    input: &Value,
) -> Value {
    let body = config
        .get("body")
        .cloned()
        .or_else(|| input.get("body").cloned());
    let raw_body = config
        .get("rawBody")
        .or_else(|| input.get("rawBody"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let body_for_log: Value = raw_body
        .map(Value::String)
        .or(body)
        .unwrap_or(Value::Null);

    let mut headers = input
        .get("headers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    if let Some(config_headers) = config.get("headers").and_then(Value::as_object) {
        for (k, v) in config_headers {
            headers.insert(k.clone(), v.clone());
        }
    }

    serde_json::json!({
        "method": method,
        "url": url,
        "headers": Value::Object(headers),
        "body": body_for_log
    })
}

const DEFAULT_TIMEOUT_SECS: u64 = 30;

pub struct ServiceCallExecutor {
    client: reqwest::Client,
    pool: Option<Arc<sqlx::PgPool>>,
}

impl Default for ServiceCallExecutor {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .build()
                .expect("reqwest client"),
            pool: None,
        }
    }
}

impl ServiceCallExecutor {
    pub fn new(pool: Arc<sqlx::PgPool>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
                .build()
                .expect("reqwest client"),
            pool: Some(pool),
        }
    }
}

#[async_trait]
impl NodeExecutor for ServiceCallExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        mut input: Value,
        mut config: Value,
    ) -> Result<Value, String> {
        expression::interpolate_value(&mut input, &ctx.context)?;
        expression::interpolate_value(&mut config, &ctx.context)?;

        let apply_headers_and_body = |req: reqwest::RequestBuilder, config: &Value, input: &Value| {
            let body = config
                .get("body")
                .cloned()
                .or_else(|| input.get("body").cloned());
            let raw_body = config
                .get("rawBody")
                .or_else(|| input.get("rawBody"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            let mut headers = input
                .get("headers")
                .and_then(Value::as_object)
                .map(|m| m.clone())
                .unwrap_or_else(serde_json::Map::new);
            if let Some(config_headers) = config.get("headers").and_then(Value::as_object) {
                for (k, v) in config_headers {
                    headers.insert(k.clone(), v.clone());
                }
            }

            let mut req = req;
            if let Some(ref raw) = raw_body {
                req = req.body(raw.clone());
            } else if let Some(ref b) = body {
                if *b != Value::Null {
                    req = req.json(b);
                }
            }
            for (k, v) in &headers {
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
            req
        };

        if let Some(url_val) = config.get("url").and_then(|v| v.as_str()) {
            let method = config
                .get("method")
                .or_else(|| config.get("Method"))
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();

            let request = request_log(&method, url_val, &config, &input);
            tracing::info!(
                execution_id = %ctx.execution_id,
                node_type = "serviceCall",
                method = %method,
                url = %url_val,
                "service call request"
            );
            tracing::debug!(execution_id = %ctx.execution_id, request = ?request, "service call request body");

            let mut req = match method.as_str() {
                "GET" => self.client.get(url_val),
                "POST" => self.client.post(url_val),
                "PUT" => self.client.put(url_val),
                "PATCH" => self.client.patch(url_val),
                "DELETE" => self.client.delete(url_val),
                _ => self.client.get(url_val),
            };
            req = apply_headers_and_body(req, &config, &input);

            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            let body_value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Value::String(String::from_utf8_lossy(&bytes).into_owned())
            });

            tracing::debug!(
                execution_id = %ctx.execution_id,
                node_type = "serviceCall",
                status = status,
                response_body = ?body_value,
                "service call response (direct url)"
            );

            return Ok(serde_json::json!({
                "status": status,
                "body": body_value,
                "request": request
            }));
        }

        let slug = config
            .get("serviceSlug")
            .or_else(|| config.get("service"))
            .and_then(Value::as_str)
            .ok_or("ServiceCall config must have url or (serviceSlug/service)")?;

        let pool = self
            .pool
            .as_ref()
            .ok_or("ServiceCall requires a database pool for slug lookup")?;
        let row = storage::get_service_by_slug(pool.as_ref(), slug)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown service: {}", slug))?;

        let path_from_node = config
            .get("path")
            .or_else(|| config.get("operation"))
            .or_else(|| config.get("name"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or("ServiceCall config must have path (or operation/name) when using serviceSlug")?;
        let path = if path_from_node.starts_with('/') {
            path_from_node
        } else {
            format!("/{}", path_from_node)
        };

        let base = row.base_url.trim_end_matches('/');
        let url = format!("{}{}", base, path);

        let method = config
            .get("method")
            .or_else(|| config.get("Method"))
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();

        let request = request_log(&method, &url, &config, &input);
        tracing::info!(
            execution_id = %ctx.execution_id,
            node_type = "serviceCall",
            method = %method,
            url = %url,
            "service call request"
        );
        tracing::debug!(execution_id = %ctx.execution_id, request = ?request, "service call request body");

        let mut req = match method.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.get(&url),
        };
        req = apply_headers_and_body(req, &config, &input);

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body_value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });

        tracing::debug!(
            execution_id = %ctx.execution_id,
            node_type = "serviceCall",
            status = status,
            response_body = ?body_value,
            "service call response (service slug)"
        );

        Ok(serde_json::json!({
            "status": status,
            "body": body_value,
            "request": request
        }))
    }
}
