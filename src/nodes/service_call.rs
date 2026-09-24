use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tracing;

use super::http_body::{build_form_parts, form_summary, to_reqwest_form, BodyMode, FormPart};
use crate::expression;
use crate::storage;

/// Resolve a node's `rawBody` into the string to send as the HTTP body.
///
/// `rawBody` is normally a template string. But the executor pre-interpolates the whole config
/// before the node runs, and a `rawBody` that is a *sole* `{{ expr }}` gets replaced with the raw
/// typed result — e.g. a JMESPath projection like `data[*].{...}` becomes a real JSON array/object,
/// no longer a string. Treating only `Value::String` as a body would then send nothing. Serialize
/// those typed values instead so the reshaped payload is sent as JSON. `null`/missing means no body.
fn raw_body_string(config: &Value, input: &Value) -> Option<String> {
    config
        .get("rawBody")
        .or_else(|| input.get("rawBody"))
        .and_then(|v| match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
}

/// Build request log (method, url, headers, body) for steps/executions and tracing.
fn request_log(
    method: &str,
    url: &str,
    config: &Value,
    input: &Value,
    body_mode: BodyMode,
    form_parts: Option<&[FormPart]>,
) -> Value {
    let body_for_log = if let Some(parts) = form_parts {
        form_summary(parts)
    } else if body_mode == BodyMode::None {
        Value::Null
    } else {
        let body = config
            .get("body")
            .cloned()
            .or_else(|| input.get("body").cloned());
        raw_body_string(config, input)
            .map(Value::String)
            .or(body)
            .unwrap_or(Value::Null)
    };

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
        "bodyMode": body_mode.as_str(),
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
        // Capture the raw body template before interpolation. Flat string interpolation would
        // stringify a quoted whole-value placeholder (e.g. `"items": "{{ users[*]... }}"`) into
        // `"[{...}]"`; JSON-aware interpolation injects the real array instead.
        let raw_body_template = config
            .get("rawBody")
            .or_else(|| input.get("rawBody"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        expression::interpolate_value(&mut input, &ctx.context)?;
        expression::interpolate_value(&mut config, &ctx.context)?;

        if let Some(tpl) = raw_body_template {
            let rendered = expression::interpolate_json_body(&tpl, &ctx.context)?;
            if let Value::Object(map) = &mut config {
                map.insert("rawBody".to_string(), Value::String(rendered));
            }
        }

        let body_mode = BodyMode::parse(
            config
                .get("bodyMode")
                .or_else(|| input.get("bodyMode"))
                .unwrap_or(&Value::Null),
        )?;
        // Form-data rows live in `rawBody` (what the builder edits), falling back to `body`.
        let form_parts = match body_mode {
            BodyMode::FormData => Some(build_form_parts(
                config
                    .get("rawBody")
                    .or_else(|| input.get("rawBody"))
                    .or_else(|| config.get("body"))
                    .or_else(|| input.get("body"))
                    .unwrap_or(&Value::Null),
            )?),
            _ => None,
        };
        if body_mode == BodyMode::Raw && raw_body_string(&config, &input).is_none() {
            let has_other_body = config.get("body").or_else(|| input.get("body")).is_some_and(|b| !b.is_null());
            if has_other_body {
                return Err("ServiceCall bodyMode \"raw\" requires rawBody to be a string".to_string());
            }
        }

        // Continue this execution's trace on the outbound call (fresh span-id per hop).
        let traceparent = ctx
            .trace_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(crate::trace::format_traceparent);

        let apply_headers_and_body = |req: reqwest::RequestBuilder,
                                      config: &Value,
                                      input: &Value,
                                      form: Option<reqwest::multipart::Form>| {
            let body = config
                .get("body")
                .cloned()
                .or_else(|| input.get("body").cloned());
            let raw_body = raw_body_string(config, input);

            let mut headers = input
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(config_headers) = config.get("headers").and_then(Value::as_object) {
                for (k, v) in config_headers {
                    headers.insert(k.clone(), v.clone());
                }
            }
            let user_content_type = headers.keys().any(|k| k.eq_ignore_ascii_case("content-type"));

            // User headers go on before the body: reqwest appends headers, so a default set
            // first would be sent alongside the user's. Form-data owns Content-Type because it
            // must carry the generated boundary.
            let mut req = req;
            for (k, v) in &headers {
                if body_mode == BodyMode::FormData && k.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
            match body_mode {
                BodyMode::None => {}
                BodyMode::FormData => {
                    if let Some(form) = form {
                        req = req.multipart(form);
                    }
                }
                BodyMode::Raw => {
                    if let Some(raw) = raw_body {
                        if !user_content_type {
                            req = req.header("Content-Type", "text/plain");
                        }
                        req = req.body(raw);
                    }
                }
                BodyMode::Legacy => {
                    if let Some(raw) = raw_body {
                        req = req.body(raw);
                    } else if let Some(ref b) = body {
                        if *b != Value::Null {
                            req = req.json(b);
                        }
                    }
                }
            }
            // Add traceparent unless the workflow author set one explicitly.
            if let Some(ref tp) = traceparent {
                let user_set = headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case(crate::trace::TRACEPARENT_HEADER));
                if !user_set {
                    req = req.header(crate::trace::TRACEPARENT_HEADER, tp);
                }
            }
            req
        };
        let request_log_for = |method: &str, url: &str| {
            request_log(method, url, &config, &input, body_mode, form_parts.as_deref())
        };
        let form = form_parts.clone().map(to_reqwest_form).transpose()?;

        if let Some(url_val) = config.get("url").and_then(|v| v.as_str()) {
            let method = config
                .get("method")
                .or_else(|| config.get("Method"))
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();

            let request = request_log_for(&method, url_val);
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
            req = apply_headers_and_body(req, &config, &input, form);

            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            let body_value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));

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
            .ok_or(
                "ServiceCall config must have path (or operation/name) when using serviceSlug",
            )?;
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

        let request = request_log_for(&method, &url);
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
        req = apply_headers_and_body(req, &config, &input, form);

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body_value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::http_body::test_support::{capture_one, header_values};
    use serde_json::json;
    use uuid::Uuid;

    #[tokio::test]
    async fn formdata_rows_in_raw_body_send_multipart() {
        let (url, server) = capture_one().await;
        let context = json!({
            "local": { "parentTicketId": "T-1" },
            "Webhook": { "body": { "ticketAttachments": [
                { "attachmentName": "a.pdf", "fileType": "application/pdf", "contentBase64": "JVBERi0xLjQ=" }
            ] } }
        });
        let mut config = json!({
            "method": "POST",
            "url": url,
            "headers": { "Content-Type": "multipart/form-data; boundary=RegereFormBoundary", "X-Tenant-ID": "t1" },
            "bodyMode": "formdata",
            "rawBody": [
                { "key": "ticketId", "type": "text", "value": "{{ local.parentTicketId }}" },
                { "key": "attachmentName", "type": "text", "value": "{{ Webhook.body.ticketAttachments[*].attachmentName }}" },
                { "key": "attachmentPath", "type": "file", "value": "{{ Webhook.body.ticketAttachments[*].{filename: attachmentName, contentType: fileType, base64: contentBase64} }}" }
            ]
        });
        // The executor interpolates config before the node runs; mirror that here.
        expression::interpolate_value(&mut config, &context).unwrap();
        let ctx = ExecutionContext::new(Uuid::nil(), Uuid::nil(), context);
        let out = ServiceCallExecutor::default().execute(&ctx, "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();

        let content_types = header_values(&head, "content-type");
        assert_eq!(content_types.len(), 1, "{head}");
        let boundary = content_types[0].strip_prefix("multipart/form-data; boundary=").expect(content_types[0]);
        assert_ne!(boundary, "RegereFormBoundary");
        assert_eq!(header_values(&head, "x-tenant-id"), ["t1"]);
        let body = String::from_utf8(body).unwrap();
        assert_eq!(
            body,
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"ticketId\"\r\n\r\nT-1\r\n\
                 --{b}\r\nContent-Disposition: form-data; name=\"attachmentName\"\r\n\r\na.pdf\r\n\
                 --{b}\r\nContent-Disposition: form-data; name=\"attachmentPath\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4\r\n\
                 --{b}--\r\n",
                b = boundary
            )
        );
        assert_eq!(out["request"]["bodyMode"], "formdata");
        assert_eq!(out["request"]["body"][2]["size"], 8);
    }

    #[tokio::test]
    async fn legacy_json_body_keeps_single_user_content_type() {
        let (url, server) = capture_one().await;
        let config = json!({
            "method": "POST",
            "url": url,
            "headers": { "Content-Type": "application/vnd.api+json" },
            "body": { "a": 1 }
        });
        let ctx = ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}));
        ServiceCallExecutor::default().execute(&ctx, "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert_eq!(header_values(&head, "content-type"), ["application/vnd.api+json"]);
        assert_eq!(body, b"{\"a\":1}");
    }

    #[tokio::test]
    async fn raw_mode_defaults_to_text_plain_and_none_mode_drops_body() {
        let ctx = ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}));
        let (url, server) = capture_one().await;
        let config = json!({ "method": "POST", "url": url, "bodyMode": "raw", "rawBody": "hello" });
        ServiceCallExecutor::default().execute(&ctx, "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert_eq!(header_values(&head, "content-type"), ["text/plain"]);
        assert_eq!(body, b"hello");

        let (url, server) = capture_one().await;
        let config = json!({ "method": "POST", "url": url, "bodyMode": "none", "rawBody": "{\"a\":1}" });
        let out = ServiceCallExecutor::default().execute(&ctx, "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert!(header_values(&head, "content-type").is_empty(), "{head}");
        assert!(body.is_empty());
        assert_eq!(out["request"]["body"], Value::Null);
    }

    #[test]
    fn raw_body_string_passes_through_a_plain_string() {
        let config = json!({ "rawBody": "{\"a\":1}" });
        let input = json!({});
        assert_eq!(
            raw_body_string(&config, &input).as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn raw_body_string_serializes_pre_interpolated_array() {
        // The bug: the executor pre-interpolates a sole `{{ data[*].{...} }}` rawBody into a real
        // JSON array, so it is no longer a string. It must still be sent as a JSON array body.
        let config = json!({ "rawBody": [ { "order": 1 }, { "order": 2 } ] });
        let input = json!({});
        let sent = raw_body_string(&config, &input).expect("array rawBody must produce a body");
        let parsed: Value = serde_json::from_str(&sent).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["order"], 1);
    }

    #[test]
    fn raw_body_string_serializes_pre_interpolated_object() {
        let config = json!({ "rawBody": { "projectId": "p1" } });
        let sent = raw_body_string(&config, &json!({})).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&sent).unwrap()["projectId"],
            "p1"
        );
    }

    #[test]
    fn raw_body_string_is_none_for_null_or_missing() {
        assert_eq!(
            raw_body_string(&json!({ "rawBody": null }), &json!({})),
            None
        );
        assert_eq!(raw_body_string(&json!({}), &json!({})), None);
    }

    #[test]
    fn raw_body_string_falls_back_to_input() {
        let config = json!({});
        let input = json!({ "rawBody": "[]" });
        assert_eq!(raw_body_string(&config, &input).as_deref(), Some("[]"));
    }
}
