//! Workflow Call node: invokes another workflow and returns its response as this
//! node's output. The called workflow runs as a child execution (its own row in
//! `workflow_executions`), and this node's output is the child's final response —
//! the same value a webhook trigger would return for that workflow.
//!
//! Config keys (in node `data`):
//!   - `workflowId`  target workflow UUID (preferred), or
//!   - `workflow` / `workflowName`  target workflow name, with optional `version` and `tenant`.
//!   - `rawBody`  templated JSON string sent as the sub-workflow's `Webhook.body`, or
//!   - `body` / `input.body`  structured payload sent as the sub-workflow's `Webhook.body`.

use super::{ExecutionContext, NodeExecutor};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Weak};
use uuid::Uuid;

use crate::executor;
use crate::expression;
use crate::registry::NodeRegistry;
use crate::storage;

/// Maximum nesting depth for WorkflowCall nodes, to guard against infinite recursion
/// through indirect cycles (A calls B calls A ...).
const MAX_WORKFLOW_CALL_DEPTH: u64 = 10;

pub struct WorkflowCallExecutor {
    pool: Arc<sqlx::PgPool>,
    /// Weak reference back to the registry so the sub-workflow can resolve node
    /// executors (including further WorkflowCall nodes). Weak avoids a reference cycle.
    registry: Weak<dyn NodeRegistry>,
}

impl WorkflowCallExecutor {
    pub fn new(pool: Arc<sqlx::PgPool>, registry: Weak<dyn NodeRegistry>) -> Self {
        Self { pool, registry }
    }
}

#[async_trait]
impl NodeExecutor for WorkflowCallExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        _node_id: &str,
        mut input: Value,
        mut config: Value,
    ) -> Result<Value, String> {
        // Capture the raw body template before interpolation so JSON-aware substitution (below) can
        // inject real arrays/objects for quoted whole-value placeholders instead of stringifying them.
        let raw_body_template = config
            .get("rawBody")
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

        // Guard against runaway recursion (direct or indirect cycles).
        let depth = ctx
            .context
            .get("workflowCallDepth")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        if depth > MAX_WORKFLOW_CALL_DEPTH {
            return Err(format!(
                "WorkflowCall exceeded max nesting depth ({})",
                MAX_WORKFLOW_CALL_DEPTH
            ));
        }

        // Resolve the target workflow by id (preferred) or by name (+ optional version/tenant).
        let workflow = if let Some(id_str) = config
            .get("workflowId")
            .or_else(|| config.get("workflow_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            let uuid = Uuid::parse_str(id_str.trim())
                .map_err(|_| format!("WorkflowCall: invalid workflowId '{}'", id_str))?;
            storage::get_workflow_by_id(self.pool.as_ref(), uuid)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("WorkflowCall: workflow not found: {}", uuid))?
        } else if let Some(name) = config
            .get("workflowName")
            .or_else(|| config.get("workflow"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            let version = config
                .get("version")
                .and_then(|v| v.as_i64())
                .and_then(|n| i32::try_from(n).ok());
            let tenant = config.get("tenant").and_then(Value::as_str);
            storage::get_workflow_by_name(self.pool.as_ref(), name.trim(), tenant, version)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("WorkflowCall: workflow not found: {}", name))?
        } else {
            return Err(
                "WorkflowCall config must have workflowId or workflow/workflowName".to_string(),
            );
        };

        // Prevent a workflow from directly invoking itself.
        if workflow.id == ctx.workflow_id {
            return Err(format!(
                "WorkflowCall: workflow {} cannot call itself",
                workflow.id
            ));
        }

        // Build the payload passed to the sub-workflow as its `Webhook.body`.
        let body = if let Some(raw) = config.get("rawBody").and_then(Value::as_str) {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
        } else {
            config
                .get("body")
                .cloned()
                .or_else(|| input.get("body").cloned())
                .or_else(|| {
                    if input.is_null() {
                        None
                    } else {
                        Some(input.clone())
                    }
                })
                .unwrap_or(Value::Null)
        };

        let headers = config
            .get("headers")
            .cloned()
            .or_else(|| input.get("headers").cloned())
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        // Log/return shape mirrors HttpRequest/ServiceCall's `request` object.
        let request = serde_json::json!({
            "workflowId": workflow.id,
            "workflowName": workflow.name,
            "version": workflow.version,
            "body": body.clone()
        });

        let initial_context = serde_json::json!({
            "Webhook": { "body": body, "headers": headers },
            "workflowCallDepth": depth
        });

        let registry = self
            .registry
            .upgrade()
            .ok_or("WorkflowCall: node registry is unavailable")?;

        // Create a child execution row for the sub-workflow run.
        let sub_exec = storage::create_execution(
            self.pool.as_ref(),
            workflow.id,
            Some(workflow.version),
            &initial_context,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

        tracing::info!(
            execution_id = %ctx.execution_id,
            node_type = "workflowCall",
            sub_workflow_id = %workflow.id,
            sub_workflow_name = %workflow.name,
            sub_execution_id = %sub_exec.id,
            depth = depth,
            "calling workflow"
        );

        executor::run_workflow(
            self.pool.as_ref(),
            registry,
            workflow.id,
            sub_exec.id,
            &workflow.definition,
            initial_context,
        )
        .await?;

        // The workflow "response" is the last completed step's output — the same value
        // the webhook trigger returns for a workflow.
        let steps = storage::list_steps_by_execution(self.pool.as_ref(), sub_exec.id)
            .await
            .map_err(|e| e.to_string())?;
        let response = steps
            .into_iter()
            .rev()
            .find(|s| s.status == "completed")
            .and_then(|s| s.output)
            .unwrap_or(Value::Null);

        // Surface the inner body/status so downstream Merge/expression nodes read a
        // consistent `{status, body}` shape.
        let status = response.get("status").and_then(|v| v.as_u64()).unwrap_or(200);
        let response_body = response
            .get("body")
            .cloned()
            .unwrap_or_else(|| response.clone());

        tracing::debug!(
            execution_id = %ctx.execution_id,
            node_type = "workflowCall",
            sub_execution_id = %sub_exec.id,
            status = status,
            response_body = ?response_body,
            "workflow call completed"
        );

        Ok(serde_json::json!({
            "status": status,
            "body": response_body,
            "execution_id": sub_exec.id,
            "request": request
        }))
    }
}
