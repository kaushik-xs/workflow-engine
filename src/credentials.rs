//! Credentials, resolved from the credential store at the moment of use.
//!
//! The engine keeps no secrets. A node reads one as `{{ credentials.<name>.<field> }}`
//! in its config; just before the node runs, the engine resolves each credential it
//! names from the credential store (`CREDENTIAL_STORE_URL`, with this engine's consumer
//! token `CREDENTIAL_STORE_TOKEN`) for the run's tenant, and the store logs the use with
//! the workflow, execution and node. The values never enter the execution context:
//! what the node returns (or its error) has every value it was given replaced by `***`
//! before anything is recorded.
//!
//! `BrowserAutomation` passes credential *names* on to hawk (`credentials` in its
//! config), which resolves them itself — so the values do not pass through here at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Map, Value};
use uuid::Uuid;

/// A credential name, as `credentials.<name>` reads it.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z')) && name.len() <= 64 && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

pub struct Client {
    pool: sqlx::PgPool,
    url: Option<String>,
    token: Option<String>,
    http: reqwest::Client,
    tenants: Mutex<HashMap<Uuid, String>>,
}

static CLIENT: OnceLock<Arc<Client>> = OnceLock::new();

/// Set up once at startup from `CREDENTIAL_STORE_URL` / `CREDENTIAL_STORE_TOKEN`.
pub fn init(pool: sqlx::PgPool) -> Arc<Client> {
    let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let url = var("CREDENTIAL_STORE_URL").map(|u| u.trim_end_matches('/').to_string());
    if url.is_none() {
        tracing::info!("CREDENTIAL_STORE_URL is not set: nodes cannot use credentials");
    }
    let client = Arc::new(Client {
        pool,
        url,
        token: var("CREDENTIAL_STORE_TOKEN"),
        http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().expect("reqwest client"),
        tenants: Mutex::default(),
    });
    let _ = CLIENT.set(client.clone());
    client
}

pub fn client() -> Option<Arc<Client>> {
    CLIENT.get().cloned()
}

impl Client {
    /// The tenant a workflow belongs to (cached).
    pub async fn tenant_of(&self, workflow_id: Uuid) -> Result<String, String> {
        if let Some(t) = self.tenants.lock().unwrap().get(&workflow_id) {
            return Ok(t.clone());
        }
        let tenant: Option<String> = sqlx::query_scalar("SELECT tenant FROM workflows WHERE id = $1")
            .bind(workflow_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let tenant = tenant.ok_or("the run's workflow is gone")?;
        self.tenants.lock().unwrap().insert(workflow_id, tenant.clone());
        Ok(tenant)
    }

    /// One credential's fields, for `context` (logged by the store).
    pub async fn resolve(&self, tenant: &str, name: &str, context: Value) -> Result<Map<String, Value>, String> {
        let (Some(url), Some(token)) = (&self.url, &self.token) else {
            return Err(format!("credential \"{name}\": set CREDENTIAL_STORE_URL and CREDENTIAL_STORE_TOKEN to use credentials"));
        };
        let res = self
            .http
            .post(format!("{url}/credentials/{name}/resolve"))
            .bearer_auth(token)
            .header("X-Tenant-ID", tenant)
            .json(&json!({ "context": context }))
            .send()
            .await
            .map_err(|e| format!("credential \"{name}\": cannot reach the credential store: {e}"))?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(format!("credential \"{name}\": {}", body["error"].as_str().unwrap_or(status.as_str())));
        }
        body["values"].as_object().cloned().ok_or_else(|| format!("credential \"{name}\": the store answered oddly"))
    }
}

/// Credential names a node's config reads as `credentials.<name>`.
pub fn referenced(value: &Value) -> Vec<String> {
    let mut names = vec![];
    collect(value, &mut names);
    names.sort();
    names.dedup();
    names
}

fn collect(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            for (_, _, expr) in crate::expression::find_expressions(s) {
                let mut rest = expr.as_str();
                while let Some(i) = rest.find("credentials.") {
                    let before = rest[..i].chars().last();
                    let tail = &rest[i + "credentials.".len()..];
                    let name: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && valid_name(&name) {
                        names.push(name);
                    }
                    rest = tail;
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, names)),
        Value::Object(map) => map.values().for_each(|v| collect(v, names)),
        _ => {}
    }
}

/// The values to hide: every string (and number) field of the credentials used.
pub fn secrets_of(credentials: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = credentials
        .values()
        .filter_map(Value::as_object)
        .flat_map(|m| m.values())
        .filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|s| s.len() >= 3)
        .collect();
    // Longest first, so a value containing another is hidden whole.
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out.dedup();
    out
}

pub fn mask_text(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "***");
        }
    }
    out
}

/// `value` with every credential value replaced by `***`, in strings and object keys.
pub fn mask(value: &Value, secrets: &[String]) -> Value {
    if secrets.is_empty() {
        return value.clone();
    }
    match value {
        Value::String(s) => Value::String(mask_text(s, secrets)),
        Value::Number(n) if secrets.contains(&n.to_string()) => Value::String("***".into()),
        Value::Array(items) => Value::Array(items.iter().map(|v| mask(v, secrets)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (mask_text(k, secrets), mask(v, secrets))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_credential_references_in_expressions_only() {
        let config = json!({
            "url": "{{ credentials.dcs.link }}/login",
            "headers": { "Authorization": "Bearer {{ credentials.graph_api.token }}" },
            "body": "credentials.plain_text is not an expression",
            "x": "{{ nodes.credentials.body }} {{ my_credentials.x }}"
        });
        assert_eq!(referenced(&config), ["dcs", "graph_api"]);
    }

    #[test]
    fn masks_values_wherever_they_appear() {
        let creds = json!({ "dcs": { "user": "ops", "password": "hunter2!", "pin": 4821 } });
        let secrets = secrets_of(creds.as_object().unwrap());
        let out = mask(&json!({ "request": { "body": "u=ops&p=hunter2!", "pin": 4821, "hunter2!": true }, "ok": 1 }), &secrets);
        assert_eq!(out, json!({ "request": { "body": "u=***&p=***", "pin": "***", "***": true }, "ok": 1 }));
    }
}
