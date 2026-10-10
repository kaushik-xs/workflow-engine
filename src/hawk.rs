//! Client for hawk (`hawkd`), the browser-automation service the `BrowserAutomation` node
//! and the `/browser-automations` routes run on.
//!
//! Only hawkd's public HTTP API is used: `GET /v1/catalog`, `POST /v1/runs`,
//! `GET /v1/runs/{id}`, `POST /v1/runs/{id}/cancel`, `GET /v1/runs/{id}/artifacts/{name}`
//! and the session routes. Nothing in hawk is changed for the engine.
//!
//! Where hawkd is: the `hawk` entry of the service registry when there is one, otherwise
//! `HAWK_URL`, otherwise `http://127.0.0.1:7878` (hawkd's default bind).

use serde_json::Value;
use std::sync::Arc;

use crate::storage;

/// Service registry slug that overrides `HAWK_URL`.
pub const SERVICE_SLUG: &str = "hawk";
pub const DEFAULT_URL: &str = "http://127.0.0.1:7878";

/// Per-request timeout. Runs are started and polled, never held open, so this stays short.
const REQUEST_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HawkError {
    /// hawkd could not be reached, or its answer could not be read.
    Unreachable(String),
    /// hawkd answered with an error status; the message is its `error` field.
    Status(u16, String),
}

impl std::fmt::Display for HawkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HawkError::Unreachable(e) => write!(f, "hawk unreachable: {e}"),
            HawkError::Status(status, message) => write!(f, "hawk answered {status}: {message}"),
        }
    }
}

impl std::error::Error for HawkError {}

impl HawkError {
    pub fn status(&self) -> Option<u16> {
        match self {
            HawkError::Status(s, _) => Some(*s),
            HawkError::Unreachable(_) => None,
        }
    }
}

/// An artifact's bytes, as hawkd served them.
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

pub struct HawkClient {
    http: reqwest::Client,
    pool: Option<Arc<sqlx::PgPool>>,
    /// Fixed base URL, skipping the registry and `HAWK_URL` (tests).
    fixed: Option<String>,
}

impl HawkClient {
    pub fn new(pool: Option<Arc<sqlx::PgPool>>) -> Self {
        Self { http: http_client(), pool, fixed: None }
    }

    pub fn with_base_url(url: impl Into<String>) -> Self {
        Self { http: http_client(), pool: None, fixed: Some(url.into()) }
    }

    /// hawkd's base URL, looked up on every call so a registry change applies without a restart.
    pub async fn base_url(&self) -> String {
        if let Some(url) = &self.fixed {
            return url.clone();
        }
        if let Some(pool) = &self.pool {
            match storage::get_service_by_slug(pool, SERVICE_SLUG).await {
                Ok(Some(row)) if !row.base_url.trim().is_empty() => return row.base_url,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "service registry lookup for hawk failed; using HAWK_URL"),
            }
        }
        std::env::var("HAWK_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_string())
    }

    /// `base` joined with `segments`, each percent-encoded as one path segment.
    pub fn url(base: &str, segments: &[&str]) -> Result<reqwest::Url, HawkError> {
        let mut url = reqwest::Url::parse(base).map_err(|e| HawkError::Unreachable(format!("bad hawk URL {base:?}: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| HawkError::Unreachable(format!("bad hawk URL {base:?}")))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, HawkError> {
        let resp = req.send().await.map_err(|e| HawkError::Unreachable(e.to_string()))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(text);
        Err(HawkError::Status(status, message))
    }

    async fn json(&self, method: reqwest::Method, segments: &[&str], body: Option<&Value>) -> Result<Value, HawkError> {
        let url = Self::url(&self.base_url().await, segments)?;
        let mut req = self.http.request(method, url);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = self.send(req).await?;
        let bytes = resp.bytes().await.map_err(|e| HawkError::Unreachable(e.to_string()))?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| HawkError::Unreachable(format!("hawk sent invalid JSON: {e}")))
    }

    /// Published step automations: `[{ id, engine, process, version, name, manifest, params, publishedAt }]`.
    pub async fn catalog(&self) -> Result<Vec<Value>, HawkError> {
        let v = self.json(reqwest::Method::GET, &["v1", "catalog"], None).await?;
        Ok(v.get("steps").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Start a run (`POST /v1/runs`). Returns the run; `status` is `running` unless it ended at once.
    pub async fn start_run(&self, request: &Value) -> Result<Value, HawkError> {
        self.json(reqwest::Method::POST, &["v1", "runs"], Some(request)).await
    }

    pub async fn get_run(&self, id: &str) -> Result<Value, HawkError> {
        self.json(reqwest::Method::GET, &["v1", "runs", id], None).await
    }

    /// Cancel a run. A run that already finished (409) is not an error.
    pub async fn cancel_run(&self, id: &str) -> Result<(), HawkError> {
        match self.json(reqwest::Method::POST, &["v1", "runs", id, "cancel"], None).await {
            Ok(_) => Ok(()),
            Err(HawkError::Status(409, _)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn artifact(&self, run_id: &str, name: &str) -> Result<Artifact, HawkError> {
        let url = Self::url(&self.base_url().await, &["v1", "runs", run_id, "artifacts", name])?;
        let resp = self.send(self.http.get(url)).await?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = resp.bytes().await.map_err(|e| HawkError::Unreachable(e.to_string()))?.to_vec();
        Ok(Artifact { bytes, content_type })
    }

    /// Sign in through a stored session (`POST /v1/sessions/{slug}/acquire`). Returns `{ handle, slug, reused, ... }`.
    pub async fn acquire_session(&self, slug: &str, body: &Value) -> Result<Value, HawkError> {
        self.json(reqwest::Method::POST, &["v1", "sessions", slug, "acquire"], Some(body)).await
    }

    /// Close a live session. One that is already gone (404) is not an error.
    pub async fn release_session(&self, handle: &str) -> Result<(), HawkError> {
        match self.json(reqwest::Method::DELETE, &["v1", "live-sessions", handle], None).await {
            Ok(_) | Err(HawkError::Status(404, _)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .expect("reqwest client")
}

/// A catalog entry flattened for pickers: the manifest's label, system and summary lifted out,
/// whether it writes, plus the `params` schema the node's `params` must satisfy.
pub fn describe(step: &Value) -> Value {
    let manifest = step.get("manifest").cloned().unwrap_or(Value::Null);
    let writes = manifest.get("writes").cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "id": step["id"],
        "name": step["name"],
        "label": manifest.get("label").cloned().unwrap_or_else(|| step["name"].clone()),
        "system": manifest["system"],
        "summary": manifest["summary"],
        "process": step["process"],
        "version": step["version"],
        "publishedAt": step["publishedAt"],
        "writes": !writes.is_null(),
        "irreversible": writes.get("irreversible").and_then(Value::as_bool).unwrap_or(false),
        "params": step.get("params").cloned().unwrap_or_else(|| serde_json::json!({})),
        "manifest": manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn url_encodes_segments_and_keeps_base_path() {
        let u = HawkClient::url("http://h:7878/", &["v1", "runs", "r1", "artifacts", "a b.pdf"]).unwrap();
        assert_eq!(u.as_str(), "http://h:7878/v1/runs/r1/artifacts/a%20b.pdf");
        let u = HawkClient::url("http://gw/hawk", &["v1", "catalog"]).unwrap();
        assert_eq!(u.as_str(), "http://gw/hawk/v1/catalog");
        assert!(HawkClient::url("not a url", &["v1"]).is_err());
    }

    #[test]
    fn describe_lifts_manifest_fields() {
        let d = describe(&json!({
            "id": "raise-invoice", "name": "Raise invoice", "process": "shared", "version": 3,
            "manifest": { "system": "Billing portal", "label": "Raise invoice", "writes": { "action": "invoice", "irreversible": true } },
            "params": { "bls": { "type": "array", "required": true } }
        }));
        assert_eq!(d["label"], "Raise invoice");
        assert_eq!(d["system"], "Billing portal");
        assert_eq!(d["writes"], true);
        assert_eq!(d["irreversible"], true);
        assert_eq!(d["params"]["bls"]["required"], true);

        let d = describe(&json!({ "id": "download-report", "name": "Download report", "manifest": {} }));
        assert_eq!(d["label"], "Download report");
        assert_eq!(d["writes"], false);
        assert_eq!(d["params"], json!({}));
    }
}
