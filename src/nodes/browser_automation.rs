//! BrowserAutomation node: runs any of hawk's published automations in a headless browser
//! and returns its result. Generic: nothing here is specific to a site or an automation.
//!
//! The run happens in hawk (`hawkd`, see [`crate::hawk`]); this node starts it with
//! `POST /v1/runs` and follows it. A run that finishes within `inlineFor` completes the node
//! in the same request. A longer one suspends the workflow (status `waiting`, like a Wait
//! node) and is checked every `durablePollInterval` by the timer worker, so it holds no
//! request open and survives restarts. Pick automations with `GET /browser-automations`.
//!
//! Config keys (in node `data`; `{{ }}` expressions are interpolated before this runs):
//!   - `automation` (required): the hawk automation id (see `GET /browser-automations`).
//!   - `version`: run this version instead of the published one.
//!   - `params`: the automation's params (see its `params` schema in the catalog).
//!   - `inputs`: outputs of upstream steps, available to the automation as `inputs.*`.
//!   - `env`, `secrets`: objects available as `env.*` / `secrets.*`. Secrets are masked by
//!     hawk and never stored by it; keep them out of the definition with `{{ global.x }}`.
//!   - `environment`: a stored hawk environment to load `env` from (`env` overrides it).
//!   - `session`: a live session handle to run in (string), or
//!     `{ "slug", "key"?, "params"?, "secrets"?, "env"?, "environment"? }` to sign in
//!     through hawk's stored session first. Without `key` that session is closed when the
//!     run ends; with one it is kept for reuse by later runs with the same key.
//!   - `mode`: `"live"` (default), `"dry"` (stop before the first write) or `"mock"`.
//!   - `actor`: who to record the run as. Default `workflow-engine:<workflow id>`.
//!   - `timeout`: give up (and cancel the run) after this long. Default `"30m"`.
//!   - `inlineFor`: how long to wait in-request before suspending. Default `"60s"`; `0`
//!     suspends at once.
//!   - `pollInterval`: in-request check interval. Default `"2s"`.
//!   - `durablePollInterval`: check interval while suspended. Default `"15s"`.
//!   - `failOnError`: fail the node when the run does not pass. Default `true`; with
//!     `false` the node completes and `ok` tells the outcome.
//!   - `downloadArtifacts`: add each artifact's bytes as `{ filename, contentType, base64 }`,
//!     the shape `formdata` file rows take. Default `false`.
//!
//! Output: `{ runId, automation, version, mode, status, ok, output, error, artifacts,
//! startedAt, endedAt, session }`. `status` is hawk's: `passed`, `failed`, `rejected`,
//! `stopped-before-write` or `cancelled`. Each artifact is `{ name, kind, bytes, sha256, url }`.

use super::wait::parse_duration;
use super::{ExecutionContext, NodeExecutor, NodeOutcome, ResumeReason, SuspendSpec};
use crate::hawk::{HawkClient, HawkError};
use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const DEFAULT_TIMEOUT: &str = "30m";
const DEFAULT_INLINE_FOR: &str = "60s";
const DEFAULT_POLL_INTERVAL: &str = "2s";
const DEFAULT_DURABLE_POLL_INTERVAL: &str = "15s";
/// Consecutive failed status checks (hawk down, 5xx) tolerated before giving up on a run.
const MAX_POLL_ERRORS: u32 = 5;
/// Largest artifact `downloadArtifacts` inlines; it ends up in the execution context.
const MAX_ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;

pub struct BrowserAutomationExecutor {
    hawk: Arc<HawkClient>,
}

impl BrowserAutomationExecutor {
    pub fn new(hawk: Arc<HawkClient>) -> Self {
        Self { hawk }
    }
}

impl Default for BrowserAutomationExecutor {
    fn default() -> Self {
        Self::new(Arc::new(HawkClient::new(None)))
    }
}

enum Session {
    None,
    Handle(String),
    /// Sign in through hawk's stored session `slug` with `body`; `keep` leaves it open after.
    Acquire { slug: String, body: Value, keep: bool },
}

struct Settings {
    /// The `POST /v1/runs` body, without `session` (added once it is known).
    request: Map<String, Value>,
    session: Session,
    timeout: Duration,
    inline_for: Duration,
    poll_interval: Duration,
    durable_poll_interval: Duration,
    fail_on_error: bool,
    download_artifacts: bool,
}

/// What a suspended node keeps between checks. Holds no params or secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunState {
    run_id: String,
    automation: String,
    mode: Option<String>,
    started_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
    durable_poll_ms: i64,
    fail_on_error: bool,
    download_artifacts: bool,
    /// The session handle in use, if any.
    session: Option<String>,
    /// Set when this node opened the session and must close it.
    owned_session: bool,
    #[serde(default)]
    poll_errors: u32,
}

fn non_empty_str(config: &Value, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn object(config: &Value, key: &str) -> Result<Option<Value>, String> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v @ Value::Object(_)) => Ok(Some(v.clone())),
        Some(other) => Err(format!("BrowserAutomation `{key}` must be an object, got {other}")),
    }
}

fn duration(config: &Value, key: &str, default: &str) -> Result<Duration, String> {
    match config.get(key) {
        None | Some(Value::Null) => parse_duration(&json!(default)),
        Some(Value::String(s)) if s.trim().is_empty() => parse_duration(&json!(default)),
        Some(v) => parse_duration(v),
    }
    .map_err(|e| format!("BrowserAutomation `{key}`: {e}"))
}

fn flag(config: &Value, key: &str, default: bool) -> bool {
    match config.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => match s.trim() {
            "true" => true,
            "false" => false,
            _ => default,
        },
        _ => default,
    }
}

impl Settings {
    fn parse(config: &Value, ctx: &ExecutionContext) -> Result<Self, String> {
        let automation = non_empty_str(config, "automation")
            .ok_or("BrowserAutomation needs `automation`: the id of a hawk automation")?;
        let mut request = Map::new();
        request.insert("automation".into(), json!(automation));
        match config.get("version") {
            None | Some(Value::Null) => {}
            Some(v) => {
                let n = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                    .ok_or_else(|| format!("BrowserAutomation `version` must be a number, got {v}"))?;
                request.insert("version".into(), json!(n));
            }
        }
        request.insert("params".into(), object(config, "params")?.unwrap_or_else(|| json!({})));
        if let Some(inputs) = config.get("inputs").filter(|v| !v.is_null()) {
            request.insert("inputs".into(), inputs.clone());
        }
        for key in ["env", "secrets"] {
            if let Some(v) = object(config, key)? {
                request.insert(key.into(), v);
            }
        }
        for key in ["environment", "mode"] {
            if let Some(v) = non_empty_str(config, key) {
                request.insert(key.into(), json!(v));
            }
        }
        let actor = non_empty_str(config, "actor").unwrap_or_else(|| format!("workflow-engine:{}", ctx.workflow_id));
        request.insert("actor".into(), json!(actor));

        let session = match config.get("session") {
            None | Some(Value::Null) => Session::None,
            Some(Value::String(s)) if s.trim().is_empty() => Session::None,
            Some(Value::String(s)) => Session::Handle(s.trim().to_string()),
            Some(v @ Value::Object(_)) => {
                if let Some(handle) = non_empty_str(v, "handle") {
                    Session::Handle(handle)
                } else {
                    let slug = non_empty_str(v, "slug")
                        .ok_or("BrowserAutomation `session` needs `slug` (or `handle`, or a handle string)")?;
                    let key = non_empty_str(v, "key");
                    let mut body = Map::new();
                    for k in ["params", "secrets", "env"] {
                        if let Some(o) = object(v, k).map_err(|e| e.replace("`", "`session."))? {
                            body.insert(k.into(), o);
                        }
                    }
                    if let Some(env) = non_empty_str(v, "environment") {
                        body.insert("environment".into(), json!(env));
                    }
                    if let Some(k) = &key {
                        body.insert("key".into(), json!(k));
                    }
                    body.insert("actor".into(), json!(actor));
                    Session::Acquire { slug, body: Value::Object(body), keep: key.is_some() }
                }
            }
            Some(other) => return Err(format!("BrowserAutomation `session` must be a handle or an object, got {other}")),
        };

        let timeout = duration(config, "timeout", DEFAULT_TIMEOUT)?;
        Ok(Self {
            request,
            session,
            timeout,
            inline_for: duration(config, "inlineFor", DEFAULT_INLINE_FOR)?.min(timeout),
            poll_interval: duration(config, "pollInterval", DEFAULT_POLL_INTERVAL)?.max(Duration::milliseconds(10)),
            durable_poll_interval: duration(config, "durablePollInterval", DEFAULT_DURABLE_POLL_INTERVAL)?
                .max(Duration::milliseconds(100)),
            fail_on_error: flag(config, "failOnError", true),
            download_artifacts: flag(config, "downloadArtifacts", false),
        })
    }
}

fn finished(run: &Value) -> bool {
    !matches!(run.get("status").and_then(Value::as_str), Some("running") | None)
}

/// hawk being down or failing is worth retrying; anything else (e.g. the run is gone) is not.
fn transient(e: &HawkError) -> bool {
    match e {
        HawkError::Unreachable(_) => true,
        HawkError::Status(s, _) => *s >= 500,
    }
}

impl BrowserAutomationExecutor {
    async fn release(&self, state: &RunState) {
        if let (true, Some(handle)) = (state.owned_session, &state.session) {
            if let Err(e) = self.hawk.release_session(handle).await {
                tracing::warn!(run_id = %state.run_id, session = %handle, error = %e, "could not close hawk session");
            }
        }
    }

    /// Stop following a run: cancel it, close its session, and fail with `why`.
    async fn abandon(&self, state: &RunState, why: String) -> String {
        if let Err(e) = self.hawk.cancel_run(&state.run_id).await {
            tracing::warn!(run_id = %state.run_id, error = %e, "could not cancel hawk run");
        }
        self.release(state).await;
        format!("browser automation \"{}\" {why} (hawk run {})", state.automation, state.run_id)
    }

    fn suspend(&self, state: &RunState, now: DateTime<Utc>) -> Result<NodeOutcome, String> {
        let wake_at = (now + Duration::milliseconds(state.durable_poll_ms)).min(state.deadline);
        Ok(NodeOutcome::Suspend(SuspendSpec {
            wake_at: Some(wake_at),
            state: serde_json::to_value(state).map_err(|e| e.to_string())?,
            ..Default::default()
        }))
    }

    /// Follow `run` in this request until it ends, `inline_until` passes (suspend) or the deadline does (fail).
    async fn follow(
        &self,
        mut state: RunState,
        mut run: Value,
        inline_until: DateTime<Utc>,
        poll_interval: Duration,
    ) -> Result<NodeOutcome, String> {
        loop {
            if finished(&run) {
                return self.finish(&state, run).await;
            }
            let now = Utc::now();
            if now >= state.deadline {
                return Err(self.abandon(&state, "timed out".into()).await);
            }
            if now >= inline_until {
                return self.suspend(&state, now);
            }
            let nap = poll_interval.min(inline_until - now).to_std().unwrap_or_default();
            tokio::time::sleep(nap).await;
            match self.hawk.get_run(&state.run_id).await {
                Ok(r) => {
                    run = r;
                    state.poll_errors = 0;
                }
                Err(e) if transient(&e) && state.poll_errors + 1 < MAX_POLL_ERRORS => {
                    state.poll_errors += 1;
                    tracing::warn!(run_id = %state.run_id, error = %e, "checking hawk run failed; retrying");
                }
                Err(e) => return Err(self.abandon(&state, format!("could not be followed: {e}")).await),
            }
        }
    }

    async fn finish(&self, state: &RunState, run: Value) -> Result<NodeOutcome, String> {
        self.release(state).await;
        let status = run.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let mode = run.get("mode").and_then(Value::as_str).or(state.mode.as_deref()).unwrap_or("live");
        // A dry run is meant to stop before its first write.
        let ok = status == "passed" || (status == "stopped-before-write" && mode == "dry");

        let base = self.hawk.base_url().await;
        let mut artifacts = Vec::new();
        for artifact in run.get("artifacts").and_then(Value::as_array).into_iter().flatten() {
            let mut a = artifact.clone();
            let Some(name) = artifact.get("name").and_then(Value::as_str) else {
                artifacts.push(a);
                continue;
            };
            if let Ok(url) = HawkClient::url(&base, &["v1", "runs", &state.run_id, "artifacts", name]) {
                a["url"] = json!(url.as_str());
            }
            if state.download_artifacts {
                let size = artifact.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                if size > MAX_ARTIFACT_BYTES {
                    return Err(format!(
                        "browser automation \"{}\": artifact {name} is {size} bytes, over the {MAX_ARTIFACT_BYTES}-byte downloadArtifacts limit; fetch it from its url instead (hawk run {})",
                        state.automation, state.run_id
                    ));
                }
                let file = self.hawk.artifact(&state.run_id, name).await.map_err(|e| {
                    format!("browser automation \"{}\": downloading artifact {name}: {e} (hawk run {})", state.automation, state.run_id)
                })?;
                a["filename"] = json!(name);
                a["contentType"] = json!(file.content_type.unwrap_or_else(|| "application/octet-stream".into()));
                a["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&file.bytes));
            }
            artifacts.push(a);
        }

        let output = json!({
            "runId": state.run_id,
            "automation": state.automation,
            "version": run["version"],
            "mode": mode,
            "status": status,
            "ok": ok,
            "output": run["output"],
            "error": run["error"],
            "artifacts": artifacts,
            "startedAt": run["startedAt"],
            "endedAt": run["endedAt"],
            "session": state.session,
        });
        if !ok && state.fail_on_error {
            let error = &run["error"];
            let message = error.get("message").and_then(Value::as_str).unwrap_or("no error message");
            let at = error.get("path").and_then(Value::as_str).filter(|p| !p.is_empty());
            return Err(format!(
                "browser automation \"{}\" {status}: {message}{} (hawk run {})",
                state.automation,
                at.map(|p| format!(" at {p}")).unwrap_or_default(),
                state.run_id
            ));
        }
        Ok(NodeOutcome::Complete(output))
    }
}

#[async_trait]
impl NodeExecutor for BrowserAutomationExecutor {
    async fn run(
        &self,
        ctx: &ExecutionContext,
        node_id: &str,
        _input: Value,
        config: Value,
    ) -> Result<NodeOutcome, String> {
        let settings = Settings::parse(&config, ctx)?;
        let automation = settings.request["automation"].as_str().unwrap_or_default().to_string();
        let mut request = settings.request;

        let (session, owned_session) = match settings.session {
            Session::None => (None, false),
            Session::Handle(h) => (Some(h), false),
            Session::Acquire { slug, body, keep } => {
                let acquired = self
                    .hawk
                    .acquire_session(&slug, &body)
                    .await
                    .map_err(|e| format!("browser automation \"{automation}\": signing in through session \"{slug}\": {e}"))?;
                let handle = acquired
                    .get("handle")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("browser automation \"{automation}\": session \"{slug}\" returned no handle"))?;
                (Some(handle.to_string()), !keep)
            }
        };
        if let Some(handle) = &session {
            request.insert("session".into(), json!(handle));
        }

        tracing::info!(
            execution_id = %ctx.execution_id,
            node_id = %node_id,
            node_type = "browserAutomation",
            automation = %automation,
            session = ?session,
            "starting hawk run"
        );
        let run = match self.hawk.start_run(&Value::Object(request.clone())).await {
            Ok(run) => run,
            Err(e) => {
                if let (true, Some(handle)) = (owned_session, &session) {
                    let _ = self.hawk.release_session(handle).await;
                }
                return Err(format!("browser automation \"{automation}\" could not start: {e}"));
            }
        };
        let run_id = run
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("browser automation \"{automation}\": hawk returned a run without an id"))?
            .to_string();

        let now = Utc::now();
        let state = RunState {
            run_id,
            automation,
            mode: request.get("mode").and_then(Value::as_str).map(str::to_string),
            started_at: now,
            deadline: now + settings.timeout,
            durable_poll_ms: settings.durable_poll_interval.num_milliseconds(),
            fail_on_error: settings.fail_on_error,
            download_artifacts: settings.download_artifacts,
            session,
            owned_session,
            poll_errors: 0,
        };
        self.follow(state, run, now + settings.inline_for, settings.poll_interval).await
    }

    async fn resume(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        state: Value,
        _reason: ResumeReason,
    ) -> Result<NodeOutcome, String> {
        let mut state: RunState =
            serde_json::from_value(state).map_err(|e| format!("BrowserAutomation: unreadable saved state: {e}"))?;
        let now = Utc::now();
        match self.hawk.get_run(&state.run_id).await {
            Ok(run) if finished(&run) => self.finish(&state, run).await,
            Ok(_) if now >= state.deadline => Err(self.abandon(&state, "timed out".into()).await),
            Ok(_) => {
                state.poll_errors = 0;
                self.suspend(&state, now)
            }
            Err(e) if transient(&e) && state.poll_errors + 1 < MAX_POLL_ERRORS && now < state.deadline => {
                tracing::warn!(run_id = %state.run_id, error = %e, "checking hawk run failed; will retry");
                state.poll_errors += 1;
                self.suspend(&state, now)
            }
            Err(e) => Err(self.abandon(&state, format!("could not be followed: {e}")).await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{delete, get, post},
        Json, Router,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    /// A stand-in for hawkd: runs finish after `polls` status checks with `outcome`.
    #[derive(Default)]
    struct Mock {
        polls: usize,
        outcome: &'static str,
        reject_start: Option<&'static str>,
        started: Vec<Value>,
        checks: usize,
        cancelled: Vec<String>,
        acquired: Vec<(String, Value)>,
        released: Vec<String>,
    }

    type Shared = Arc<Mutex<Mock>>;

    fn run_json(id: &str, status: &str, outcome_error: bool) -> Value {
        json!({
            "id": id, "automation": "download-report", "version": 4, "mode": "live", "status": status,
            "output": if status == "passed" { json!({ "rows": 3 }) } else { Value::Null },
            "error": if outcome_error { json!({ "kind": "step", "path": "steps[2]", "message": "locator #from not found" }) } else { Value::Null },
            "startedAt": "2026-10-10T10:00:00Z", "endedAt": if status == "running" { Value::Null } else { json!("2026-10-10T10:01:00Z") },
            "artifacts": if status == "passed" { json!([{ "name": "monthly report.pdf", "kind": "download", "bytes": 8, "sha256": "x" }]) } else { json!([]) },
        })
    }

    async fn start(State(m): State<Shared>, Json(body): Json<Value>) -> axum::response::Response {
        let mut m = m.lock().unwrap();
        if let Some(why) = m.reject_start {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": why }))).into_response();
        }
        let status = if m.polls == 0 { m.outcome } else { "running" };
        let mut run = run_json("run-1", status, status == "failed");
        run["mode"] = body.get("mode").cloned().unwrap_or_else(|| json!("live"));
        m.started.push(body);
        (StatusCode::ACCEPTED, Json(run)).into_response()
    }

    async fn status(State(m): State<Shared>, Path(id): Path<String>) -> Json<Value> {
        let mut m = m.lock().unwrap();
        m.checks += 1;
        let status = if m.checks >= m.polls { m.outcome } else { "running" };
        Json(run_json(&id, status, status == "failed"))
    }

    async fn cancel(State(m): State<Shared>, Path(id): Path<String>) -> StatusCode {
        m.lock().unwrap().cancelled.push(id);
        StatusCode::ACCEPTED
    }

    async fn artifact(Path((_id, name)): Path<(String, String)>) -> impl IntoResponse {
        assert_eq!(name, "monthly report.pdf");
        ([("content-type", "application/pdf")], b"%PDF-1.4".to_vec())
    }

    async fn acquire(State(m): State<Shared>, Path(slug): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        m.lock().unwrap().acquired.push((slug.clone(), body));
        Json(json!({ "handle": "sess-1", "slug": slug, "reused": false }))
    }

    async fn release(State(m): State<Shared>, Path(handle): Path<String>) -> StatusCode {
        m.lock().unwrap().released.push(handle);
        StatusCode::NO_CONTENT
    }

    async fn serve(mock: Mock) -> (BrowserAutomationExecutor, Shared) {
        let shared: Shared = Arc::new(Mutex::new(mock));
        let app = Router::new()
            .route("/v1/runs", post(start))
            .route("/v1/runs/:id", get(status))
            .route("/v1/runs/:id/cancel", post(cancel))
            .route("/v1/runs/:id/artifacts/:name", get(artifact))
            .route("/v1/sessions/:slug/acquire", post(acquire))
            .route("/v1/live-sessions/:handle", delete(release))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (BrowserAutomationExecutor::new(Arc::new(HawkClient::with_base_url(url))), shared)
    }

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(Uuid::nil(), Uuid::nil(), json!({}))
    }

    fn config(extra: Value) -> Value {
        let mut c = json!({
            "automation": "download-report",
            "params": { "from": "2026-10-01", "startDate": "01/10/2026" },
            "secrets": { "PORTAL_PASSWORD": "hunter2" },
            "pollInterval": "10ms",
        });
        c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        c
    }

    fn complete(outcome: NodeOutcome) -> Value {
        match outcome {
            NodeOutcome::Complete(v) => v,
            NodeOutcome::Suspend(s) => panic!("expected complete, got suspend {:?}", s.state),
        }
    }

    #[tokio::test]
    async fn runs_inline_until_passed() {
        let (node, mock) = serve(Mock { polls: 2, outcome: "passed", ..Default::default() }).await;
        let out = complete(node.run(&ctx(), "n", Value::Null, config(json!({}))).await.unwrap());
        assert_eq!(out["status"], "passed");
        assert_eq!(out["ok"], true);
        assert_eq!(out["runId"], "run-1");
        assert_eq!(out["output"], json!({ "rows": 3 }));
        assert!(out["artifacts"][0]["url"].as_str().unwrap().ends_with("/v1/runs/run-1/artifacts/monthly%20report.pdf"));
        assert!(out["artifacts"][0].get("base64").is_none());

        let m = mock.lock().unwrap();
        assert_eq!(m.checks, 2);
        let sent = &m.started[0];
        assert_eq!(sent["automation"], "download-report");
        assert_eq!(sent["params"]["from"], "2026-10-01");
        assert_eq!(sent["secrets"]["PORTAL_PASSWORD"], "hunter2");
        assert_eq!(sent["actor"], format!("workflow-engine:{}", Uuid::nil()));
        // hawkd refuses unknown fields, so only its RunRequest keys may be sent.
        let allowed = ["automation", "version", "params", "inputs", "secrets", "env", "environment", "session", "mode", "actor"];
        assert!(sent.as_object().unwrap().keys().all(|k| allowed.contains(&k.as_str())), "{sent}");
    }

    #[tokio::test]
    async fn failed_run_fails_the_node_unless_told_not_to() {
        let (node, _) = serve(Mock { polls: 1, outcome: "failed", ..Default::default() }).await;
        let err = node.run(&ctx(), "n", Value::Null, config(json!({}))).await.unwrap_err();
        assert_eq!(err, "browser automation \"download-report\" failed: locator #from not found at steps[2] (hawk run run-1)");

        let (node, _) = serve(Mock { polls: 1, outcome: "failed", ..Default::default() }).await;
        let out = complete(node.run(&ctx(), "n", Value::Null, config(json!({ "failOnError": false }))).await.unwrap());
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["path"], "steps[2]");
    }

    #[tokio::test]
    async fn dry_run_stopping_before_write_is_ok() {
        let (node, mock) = serve(Mock { polls: 0, outcome: "stopped-before-write", ..Default::default() }).await;
        let out = complete(node.run(&ctx(), "n", Value::Null, config(json!({ "mode": "dry" }))).await.unwrap());
        assert_eq!(out["ok"], true);
        assert_eq!(mock.lock().unwrap().started[0]["mode"], "dry");
    }

    #[tokio::test]
    async fn long_run_suspends_and_resumes_until_done() {
        let (node, mock) = serve(Mock { polls: 2, outcome: "passed", ..Default::default() }).await;
        let cfg = config(json!({ "inlineFor": 0, "durablePollInterval": "30s" }));
        let NodeOutcome::Suspend(spec) = node.run(&ctx(), "n", Value::Null, cfg).await.unwrap() else {
            panic!("expected suspend")
        };
        let left = spec.wake_at.unwrap() - Utc::now();
        assert!(left > Duration::seconds(25) && left <= Duration::seconds(30));
        assert!(spec.correlation_key.is_none());
        assert_eq!(spec.state["runId"], "run-1");
        assert!(!spec.state.to_string().contains("hunter2"), "secrets must not be persisted");

        // First check: still running → suspend again.
        let again = node.resume(&ctx(), "n", spec.state.clone(), ResumeReason::Timer).await.unwrap();
        let NodeOutcome::Suspend(spec) = again else { panic!("expected suspend") };
        // Second check: passed.
        let out = complete(node.resume(&ctx(), "n", spec.state, ResumeReason::Timer).await.unwrap());
        assert_eq!(out["status"], "passed");
        assert_eq!(mock.lock().unwrap().checks, 2);
    }

    #[tokio::test]
    async fn timeout_cancels_the_run() {
        let (node, mock) = serve(Mock { polls: 1000, outcome: "passed", ..Default::default() }).await;
        let err = node.run(&ctx(), "n", Value::Null, config(json!({ "timeout": "50ms" }))).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert_eq!(mock.lock().unwrap().cancelled, ["run-1"]);
    }

    #[tokio::test]
    async fn acquires_a_session_and_closes_it_unless_keyed() {
        let (node, mock) = serve(Mock { polls: 1, outcome: "passed", ..Default::default() }).await;
        let cfg = config(json!({ "session": { "slug": "portal", "secrets": { "password": "pw" } } }));
        let out = complete(node.run(&ctx(), "n", Value::Null, cfg).await.unwrap());
        assert_eq!(out["session"], "sess-1");
        {
            let m = mock.lock().unwrap();
            assert_eq!(m.acquired[0].0, "portal");
            assert_eq!(m.acquired[0].1["secrets"]["password"], "pw");
            assert_eq!(m.started[0]["session"], "sess-1");
            assert_eq!(m.released, ["sess-1"]);
        }

        let (node, mock) = serve(Mock { polls: 1, outcome: "passed", ..Default::default() }).await;
        let cfg = config(json!({ "session": { "slug": "portal", "key": "ops@acme" } }));
        complete(node.run(&ctx(), "n", Value::Null, cfg).await.unwrap());
        let m = mock.lock().unwrap();
        assert_eq!(m.acquired[0].1["key"], "ops@acme");
        assert!(m.released.is_empty());

        // A handle is passed straight through and never closed.
        drop(m);
        let (node, mock) = serve(Mock { polls: 1, outcome: "passed", ..Default::default() }).await;
        complete(node.run(&ctx(), "n", Value::Null, config(json!({ "session": "h-9" }))).await.unwrap());
        let m = mock.lock().unwrap();
        assert_eq!(m.started[0]["session"], "h-9");
        assert!(m.acquired.is_empty() && m.released.is_empty());
    }

    #[tokio::test]
    async fn downloads_artifacts_in_formdata_file_shape() {
        let (node, _) = serve(Mock { polls: 1, outcome: "passed", ..Default::default() }).await;
        let out = complete(node.run(&ctx(), "n", Value::Null, config(json!({ "downloadArtifacts": true }))).await.unwrap());
        let a = &out["artifacts"][0];
        assert_eq!(a["filename"], "monthly report.pdf");
        assert_eq!(a["contentType"], "application/pdf");
        assert_eq!(a["base64"], "JVBERi0xLjQ=");
    }

    #[tokio::test]
    async fn start_rejection_reports_hawks_message() {
        let (node, _) = serve(Mock { reject_start: Some("param \"from\" is required"), ..Default::default() }).await;
        let err = node.run(&ctx(), "n", Value::Null, config(json!({}))).await.unwrap_err();
        assert_eq!(err, "browser automation \"download-report\" could not start: hawk answered 422: param \"from\" is required");
    }

    #[tokio::test]
    async fn validates_config() {
        let node = BrowserAutomationExecutor::new(Arc::new(HawkClient::with_base_url("http://127.0.0.1:9")));
        let ctx = ctx();
        let run = |c: Value| node.run(&ctx, "n", Value::Null, c);
        assert!(run(json!({})).await.unwrap_err().contains("needs `automation`"));
        assert!(run(json!({ "automation": "x", "params": [1] })).await.unwrap_err().contains("`params` must be an object"));
        assert!(run(json!({ "automation": "x", "session": {} })).await.unwrap_err().contains("needs `slug`"));
        assert!(run(json!({ "automation": "x", "timeout": "soon" })).await.unwrap_err().contains("`timeout`"));
        assert!(run(json!({ "automation": "x" })).await.unwrap_err().contains("could not start: hawk unreachable"));
    }
}
