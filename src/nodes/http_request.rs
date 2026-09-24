use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use tracing;

use super::http_body::{build_form_parts, form_summary, has_header, to_reqwest_form, BodyMode};
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

        let body_mode = BodyMode::parse(
            input
                .get("bodyMode")
                .or_else(|| config.get("bodyMode"))
                .unwrap_or(&Value::Null),
        )?;
        let form_parts = match body_mode {
            BodyMode::FormData => Some(build_form_parts(&body)?),
            _ => None,
        };
        if body_mode == BodyMode::Raw && !matches!(body, Value::String(_) | Value::Null) {
            return Err("HttpRequest bodyMode \"raw\" requires body to be a string".to_string());
        }

        let request = serde_json::json!({
            "method": method,
            "url": url,
            "headers": headers,
            "bodyMode": body_mode.as_str(),
            "body": match &form_parts {
                Some(parts) => form_summary(parts),
                None => body.clone(),
            }
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

        // User headers go on first so body defaults below can see whether a Content-Type was
        // given (reqwest appends headers, so setting it twice would send two values). For
        // form-data the engine owns Content-Type because it must carry the generated boundary.
        let user_content_type = has_header(&headers, "content-type");
        if let Some(map) = headers.as_object() {
            for (k, v) in map {
                if body_mode == BodyMode::FormData && k.eq_ignore_ascii_case("content-type") {
                    continue;
                }
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
        }

        let has_body = match body_mode {
            BodyMode::None => false,
            BodyMode::FormData => true,
            BodyMode::Legacy | BodyMode::Raw => body != Value::Null,
        };
        if let Some(parts) = form_parts {
            req = req.multipart(to_reqwest_form(parts)?);
        } else if has_body {
            req = match body {
                Value::String(s) => {
                    if !user_content_type {
                        let default = if body_mode == BodyMode::Raw { "text/plain" } else { "application/json" };
                        req = req.header("Content-Type", default);
                    }
                    req.body(s.into_bytes())
                }
                _ => req.json(&body),
            };
        } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            req = req.body(vec![]);
        }
        // Continue this execution's trace on the outbound call, unless the workflow
        // author set a traceparent header explicitly (fresh span-id per hop).
        if let Some(tid) = ctx.trace_id.as_deref().filter(|s| !s.is_empty()) {
            let user_set = headers
                .as_object()
                .map(|m| m.keys().any(|k| k.eq_ignore_ascii_case(crate::trace::TRACEPARENT_HEADER)))
                .unwrap_or(false);
            if !user_set {
                req = req.header(
                    crate::trace::TRACEPARENT_HEADER,
                    crate::trace::format_traceparent(tid),
                );
            }
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let body_value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });

        tracing::debug!(
            execution_id = %ctx.execution_id,
            node_type = "httpRequest",
            status = status,
            response_body = ?body_value,
            "http request response"
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
    use serde_json::json;
    use crate::nodes::http_body::test_support::{capture_one, header_values};
    use uuid::Uuid;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}))
    }

    #[tokio::test]
    async fn formdata_sends_multipart_with_single_generated_content_type() {
        let (url, server) = capture_one().await;
        let config = json!({
            "method": "POST",
            "url": url,
            "headers": { "content-type": "multipart/form-data; boundary=Ignored", "X-Api-Key": "k" },
            "bodyMode": "formdata",
            "body": [
                { "key": "ticketId", "value": "T-1" },
                { "key": "attachmentName", "value": ["a.pdf", "b.png"] },
                { "key": "attachmentPath", "type": "file", "value": [
                    { "filename": "a.pdf", "contentType": "application/pdf", "base64": "JVBERi0xLjQ=" }
                ] }
            ]
        });
        let out = HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();

        let content_types = header_values(&head, "content-type");
        assert_eq!(content_types.len(), 1, "{head}");
        let boundary = content_types[0].strip_prefix("multipart/form-data; boundary=").expect(content_types[0]);
        assert_ne!(boundary, "Ignored");
        assert_eq!(header_values(&head, "x-api-key"), ["k"]);

        let body = String::from_utf8(body).unwrap();
        let expected = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"ticketId\"\r\n\r\nT-1\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"attachmentName\"\r\n\r\na.pdf\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"attachmentName\"\r\n\r\nb.png\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"attachmentPath\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4\r\n\
             --{b}--\r\n",
            b = boundary
        );
        assert_eq!(body, expected);
        assert_eq!(out["request"]["bodyMode"], "formdata");
        assert_eq!(out["request"]["body"][3], json!({ "key": "attachmentPath", "type": "file", "filename": "a.pdf", "contentType": "application/pdf", "size": 8 }));
    }

    #[tokio::test]
    async fn string_body_keeps_user_content_type_without_duplicate() {
        let (url, server) = capture_one().await;
        let config = json!({
            "method": "POST",
            "url": url,
            "headers": { "Content-Type": "multipart/form-data; boundary=X" },
            "body": "--X\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--X--\r\n"
        });
        HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert_eq!(header_values(&head, "content-type"), ["multipart/form-data; boundary=X"]);
        assert!(body.starts_with(b"--X\r\n"));
    }

    #[tokio::test]
    async fn legacy_string_body_still_defaults_to_json() {
        let (url, server) = capture_one().await;
        let config = json!({ "method": "POST", "url": url, "body": "{\"a\":1}" });
        HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert_eq!(header_values(&head, "content-type"), ["application/json"]);
        assert_eq!(body, b"{\"a\":1}");
    }

    #[tokio::test]
    async fn raw_mode_defaults_to_text_plain_and_none_mode_drops_body() {
        let (url, server) = capture_one().await;
        let config = json!({ "method": "POST", "url": url, "bodyMode": "raw", "body": "hello" });
        HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert_eq!(header_values(&head, "content-type"), ["text/plain"]);
        assert_eq!(body, b"hello");

        let (url, server) = capture_one().await;
        let config = json!({ "method": "POST", "url": url, "bodyMode": "none", "body": { "a": 1 } });
        HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap();
        let (head, body) = server.await.unwrap();
        assert!(header_values(&head, "content-type").is_empty(), "{head}");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn raw_mode_rejects_non_string_body() {
        let config = json!({ "method": "POST", "url": "http://127.0.0.1:9/", "bodyMode": "raw", "body": { "a": 1 } });
        let err = HttpRequestExecutor::default().execute(&ctx(), "n1", Value::Null, config).await.unwrap_err();
        assert!(err.contains("requires body to be a string"));
    }
}
