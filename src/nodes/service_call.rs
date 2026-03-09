use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::expression;
use crate::storage;

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

        if let Some(url_val) = config.get("url").and_then(|v| v.as_str()) {
            let method = config
                .get("method")
                .or_else(|| config.get("Method"))
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();
            let body = input.get("body").cloned().unwrap_or(Value::Null);
            let headers = input
                .get("headers")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

            let mut req = match method.as_str() {
                "GET" => self.client.get(url_val),
                "POST" => self.client.post(url_val),
                "PUT" => self.client.put(url_val),
                "PATCH" => self.client.patch(url_val),
                "DELETE" => self.client.delete(url_val),
                _ => self.client.get(url_val),
            };

            if body != Value::Null {
                req = req.json(&body);
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

            return Ok(serde_json::json!({
                "status": status,
                "body": body_value
            }));
        }

        let slug = config
            .get("serviceSlug")
            .or_else(|| config.get("service"))
            .and_then(Value::as_str)
            .ok_or("ServiceCall config must have url or (serviceSlug/service)")?;
        let name = config
            .get("name")
            .or_else(|| config.get("operation"))
            .or_else(|| config.get("path"))
            .and_then(Value::as_str)
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or("ServiceCall config must have name, operation or path when not using url")?;

        let pool = self
            .pool
            .as_ref()
            .ok_or("ServiceCall requires a database pool for slug lookup")?;
        let row = storage::get_service_by_slug(pool.as_ref(), slug)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown service: {}", slug))?;

        let base = row.base_url.trim_end_matches('/');
        let path = format!("/{}/{}", slug, name);
        let url = format!("{}{}", base, path);

        let method = config
            .get("method")
            .or_else(|| config.get("Method"))
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        let body = input.get("body").cloned().unwrap_or(Value::Null);
        let headers = input
            .get("headers")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let mut req = match method.as_str() {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.get(&url),
        };

        if body != Value::Null {
            req = req.json(&body);
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
            "body": body_value
        }))
    }
}
