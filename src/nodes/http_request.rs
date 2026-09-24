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

/// How the request `body` is encoded, chosen by the optional `bodyMode` config field. It lives
/// beside `body` rather than inside it so a real payload is never mistaken for an envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyMode {
    /// No `bodyMode`: objects/arrays go as JSON, strings as-is (default `application/json`).
    Legacy,
    /// No body is sent, even if `body` is set.
    None,
    /// `body` is a string sent as-is (default `text/plain`).
    Raw,
    /// `body` is a list of Postman-style rows sent as `multipart/form-data`.
    FormData,
}

impl BodyMode {
    fn parse(v: &Value) -> Result<Self, String> {
        match v {
            Value::Null => Ok(Self::Legacy),
            Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "" => Ok(Self::Legacy),
                "none" => Ok(Self::None),
                "raw" => Ok(Self::Raw),
                "formdata" | "form-data" | "multipart" => Ok(Self::FormData),
                other => Err(format!(
                    "HttpRequest bodyMode \"{other}\" is not supported (use none, raw or formdata)"
                )),
            },
            other => Err(format!("HttpRequest bodyMode must be a string, got {other}")),
        }
    }

    fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Legacy => None,
            Self::None => Some("none"),
            Self::Raw => Some("raw"),
            Self::FormData => Some("formdata"),
        }
    }
}

#[derive(Debug, PartialEq)]
enum FormPart {
    Text { key: String, value: String },
    File { key: String, filename: Option<String>, content_type: String, bytes: Vec<u8> },
}

/// Expand form-data rows (`{ key, type: "text" | "file", value, disabled? }`) into parts.
/// A value that resolves to an array yields one part per element under the same key, so a
/// `{{ list[*].field }}` expression loops without any templating; null/empty values yield none.
/// File values are `{ base64, filename?, contentType? }` objects (or arrays of them) and are
/// decoded so the server receives the real bytes.
fn build_form_parts(body: &Value) -> Result<Vec<FormPart>, String> {
    let rows = match body {
        Value::Array(rows) => rows,
        Value::Null => return Ok(Vec::new()),
        _ => return Err("HttpRequest bodyMode \"formdata\" requires body to be an array of { key, type, value } rows".to_string()),
    };
    let mut parts = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let obj = row
            .as_object()
            .ok_or_else(|| format!("formdata row {i} must be an object"))?;
        if obj.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let key = obj
            .get("key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| format!("formdata row {i} must have a non-empty string \"key\""))?;
        let value = obj.get("value").unwrap_or(&Value::Null);
        match obj.get("type").and_then(Value::as_str).unwrap_or("text") {
            "text" => {
                let items: Vec<&Value> = match value {
                    Value::Array(items) => items.iter().collect(),
                    v => vec![v],
                };
                for item in items {
                    let text = match item {
                        Value::Null => continue,
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    parts.push(FormPart::Text { key: key.to_string(), value: text });
                }
            }
            "file" => {
                let items: Vec<&Value> = match value {
                    Value::Array(items) => items.iter().collect(),
                    v => vec![v],
                };
                for item in items {
                    if item.is_null() {
                        continue;
                    }
                    parts.push(file_part(key, item).map_err(|e| format!("formdata row {i} (\"{key}\"): {e}"))?);
                }
            }
            other => return Err(format!("formdata row {i} has unsupported type \"{other}\" (use text or file)")),
        }
    }
    Ok(parts)
}

fn file_part(key: &str, item: &Value) -> Result<FormPart, String> {
    let obj = item
        .as_object()
        .ok_or("file value must be an object { base64, filename?, contentType? }")?;
    let encoded = obj
        .get("base64")
        .and_then(Value::as_str)
        .ok_or("file value must have a string \"base64\"")?;
    let str_field = |name: &str| obj.get(name).and_then(Value::as_str).filter(|s| !s.is_empty());
    Ok(FormPart::File {
        key: key.to_string(),
        filename: str_field("filename").map(str::to_string),
        content_type: str_field("contentType").unwrap_or("application/octet-stream").to_string(),
        bytes: decode_base64(encoded)?,
    })
}

/// Decode standard base64, tolerating whitespace/line breaks, missing padding and a
/// `data:<type>;base64,` prefix.
fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    use base64::engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig};
    use base64::{alphabet, Engine};
    const ENGINE: GeneralPurpose = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let s = match (s.starts_with("data:"), s.find(";base64,")) {
        (true, Some(idx)) => &s[idx + ";base64,".len()..],
        _ => s,
    };
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    ENGINE.decode(cleaned).map_err(|e| format!("invalid base64: {e}"))
}

fn to_reqwest_form(parts: Vec<FormPart>) -> Result<reqwest::multipart::Form, String> {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        form = match part {
            FormPart::Text { key, value } => form.text(key, value),
            FormPart::File { key, filename, content_type, bytes } => {
                let mut p = reqwest::multipart::Part::bytes(bytes)
                    .mime_str(&content_type)
                    .map_err(|e| format!("invalid contentType \"{content_type}\" for \"{key}\": {e}"))?;
                if let Some(name) = filename {
                    p = p.file_name(name);
                }
                form.part(key, p)
            }
        };
    }
    Ok(form)
}

/// What the step output records for a form-data body: file contents are replaced by their
/// size so large uploads are not copied into the execution history.
fn form_summary(parts: &[FormPart]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|p| match p {
                FormPart::Text { key, value } => serde_json::json!({ "key": key, "type": "text", "value": value }),
                FormPart::File { key, filename, content_type, bytes } => serde_json::json!({
                    "key": key,
                    "type": "file",
                    "filename": filename,
                    "contentType": content_type,
                    "size": bytes.len()
                }),
            })
            .collect(),
    )
}

fn has_header(headers: &Value, name: &str) -> bool {
    headers
        .as_object()
        .map(|m| m.keys().any(|k| k.eq_ignore_ascii_case(name)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}))
    }

    /// Accept one HTTP request on a local port, reply `{}` and hand back (head, body) bytes.
    async fn capture_one() -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/upload", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            (head, buf[head_end..head_end + len].to_vec())
        });
        (url, handle)
    }

    fn header_values<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
            .collect()
    }

    #[test]
    fn body_mode_parses_known_values_and_rejects_others() {
        assert_eq!(BodyMode::parse(&Value::Null).unwrap(), BodyMode::Legacy);
        assert_eq!(BodyMode::parse(&json!("form-data")).unwrap(), BodyMode::FormData);
        assert_eq!(BodyMode::parse(&json!("RAW")).unwrap(), BodyMode::Raw);
        assert!(BodyMode::parse(&json!("xml")).unwrap_err().contains("not supported"));
    }

    #[test]
    fn form_rows_expand_arrays_and_decode_files() {
        let body = json!([
            { "key": "ticketId", "type": "text", "value": "T-1" },
            { "key": "attachmentName", "value": ["a.pdf", "b.png", null] },
            { "key": "count", "value": 3 },
            { "key": "skipped", "value": "x", "disabled": true },
            { "key": "attachmentPath", "type": "file", "value": [
                { "filename": "a.pdf", "contentType": "application/pdf", "base64": "JVBERi0x\nLjQ=" },
                { "base64": "data:image/png;base64,iVBORw" }
            ] },
            { "key": "none", "type": "file", "value": null }
        ]);
        let parts = build_form_parts(&body).unwrap();
        let text = |k: &str, v: &str| FormPart::Text { key: k.into(), value: v.into() };
        assert_eq!(parts[..4], [text("ticketId", "T-1"), text("attachmentName", "a.pdf"), text("attachmentName", "b.png"), text("count", "3")]);
        assert_eq!(
            parts[4],
            FormPart::File { key: "attachmentPath".into(), filename: Some("a.pdf".into()), content_type: "application/pdf".into(), bytes: b"%PDF-1.4".to_vec() }
        );
        assert_eq!(
            parts[5],
            FormPart::File { key: "attachmentPath".into(), filename: None, content_type: "application/octet-stream".into(), bytes: vec![0x89, b'P', b'N', b'G'] }
        );
        assert_eq!(parts.len(), 6);
    }

    #[test]
    fn form_rows_loop_over_webhook_attachments_after_interpolation() {
        let context = json!({
            "local": { "parentTicketId": "T-10023" },
            "Webhook": { "body": { "ticketAttachments": [
                { "attachmentName": "a.pdf", "fileType": "application/pdf", "contentBase64": "JVBERi0xLjQ=" },
                { "attachmentName": "b.txt", "fileType": "text/plain", "contentBase64": "aGk=" }
            ] } }
        });
        let src = "not_null(Webhook.body.attachments, Webhook.body.ticketAttachments, `[]`)";
        let mut body = json!([
            { "key": "ticketId", "type": "text", "value": "{{ local.parentTicketId }}" },
            { "key": "attachmentName", "type": "text", "value": format!("{{{{ {src}[*].attachmentName }}}}") },
            { "key": "attachmentPath", "type": "file", "value": format!("{{{{ {src}[*].{{filename: attachmentName, contentType: fileType, base64: contentBase64}} }}}}") }
        ]);
        expression::interpolate_value(&mut body, &context).unwrap();
        let summary = form_summary(&build_form_parts(&body).unwrap());
        assert_eq!(summary, json!([
            { "key": "ticketId", "type": "text", "value": "T-10023" },
            { "key": "attachmentName", "type": "text", "value": "a.pdf" },
            { "key": "attachmentName", "type": "text", "value": "b.txt" },
            { "key": "attachmentPath", "type": "file", "filename": "a.pdf", "contentType": "application/pdf", "size": 8 },
            { "key": "attachmentPath", "type": "file", "filename": "b.txt", "contentType": "text/plain", "size": 2 }
        ]));

        // No attachments at all: only the ticketId part remains.
        let mut ctx_empty = context.clone();
        ctx_empty["Webhook"]["body"] = json!({});
        let mut empty = json!([
            { "key": "ticketId", "value": "{{ local.parentTicketId }}" },
            { "key": "attachmentPath", "type": "file", "value": format!("{{{{ {src}[*].{{base64: contentBase64}} }}}}") }
        ]);
        expression::interpolate_value(&mut empty, &ctx_empty).unwrap();
        assert_eq!(build_form_parts(&empty).unwrap().len(), 1);
    }

    #[test]
    fn form_rows_report_bad_input() {
        assert!(build_form_parts(&json!({ "key": "a" })).unwrap_err().contains("array"));
        assert!(build_form_parts(&json!([{ "value": "x" }])).unwrap_err().contains("key"));
        assert!(build_form_parts(&json!([{ "key": "f", "type": "blob" }])).unwrap_err().contains("unsupported type"));
        let err = build_form_parts(&json!([{ "key": "f", "type": "file", "value": { "base64": "!!!" } }])).unwrap_err();
        assert!(err.contains("row 0") && err.contains("invalid base64"), "{err}");
        assert!(build_form_parts(&json!([{ "key": "f", "type": "file", "value": "abc" }])).is_err());
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
