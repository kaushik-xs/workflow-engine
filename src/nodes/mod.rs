use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

pub use crate::executor::ExecutionContext;
pub use http_request::HttpRequestExecutor;
pub use http_trigger::HttpTriggerExecutor;
pub use if_node::IfExecutor;
pub use merge::MergeExecutor;
pub use service_call::ServiceCallExecutor;
pub use set_variable::SetVariableExecutor;
pub use switch::SwitchExecutor;
pub use wait::WaitExecutor;
pub use workflow_call::WorkflowCallExecutor;

pub(crate) use condition::truthy;

mod condition;
mod http_body;
mod http_request;
mod http_trigger;
mod if_node;
mod merge;
mod service_call;
mod set_variable;
mod switch;
mod wait;
mod workflow_call;

/// What running a node produced: its output, or a request to suspend the run.
#[derive(Debug, Clone)]
pub enum NodeOutcome {
    Complete(Value),
    Suspend(SuspendSpec),
}

/// Suspend the run until a time passes and/or a signal arrives, whichever comes first.
/// The executor saves the run as `waiting`; when it is resumed, the node's
/// [`NodeExecutor::resume`] is called with `state` and the [`ResumeReason`].
#[derive(Debug, Clone, Default)]
pub struct SuspendSpec {
    /// Resume at this time. `None`: no timer.
    pub wake_at: Option<DateTime<Utc>>,
    /// Resume when a signal with this key arrives (`POST /signals`). `None`: no signal.
    pub correlation_key: Option<String>,
    /// JMESPath expression over `{ "signal": { "key", "payload" } }` that must be truthy
    /// for a signal to resume the run. `None`: any signal with the key does.
    pub filter: Option<String>,
    /// The node's own data, handed back to [`NodeExecutor::resume`].
    pub state: Value,
}

/// Why a suspended node is being resumed.
#[derive(Debug, Clone)]
pub enum ResumeReason {
    /// Its `wake_at` passed.
    Timer,
    /// A matching signal arrived, with this payload.
    Signal(Value),
}

/// A node type. Plain nodes implement [`execute`](Self::execute); nodes that can suspend
/// the run implement [`run`](Self::run) and [`resume`](Self::resume) instead.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(
        &self,
        _ctx: &ExecutionContext,
        node_id: &str,
        _input: Value,
        _config: Value,
    ) -> Result<Value, String> {
        Err(format!("node {node_id} does not implement execute"))
    }

    /// Run the node. Defaults to [`execute`](Self::execute), which always completes.
    async fn run(
        &self,
        ctx: &ExecutionContext,
        node_id: &str,
        input: Value,
        config: Value,
    ) -> Result<NodeOutcome, String> {
        self.execute(ctx, node_id, input, config).await.map(NodeOutcome::Complete)
    }

    /// Continue a node that suspended. It may complete or suspend again. The default
    /// completes with what resumed it: `{ "resumedBy": "timer" }` or
    /// `{ "resumedBy": "signal", "signal": <payload> }`.
    async fn resume(
        &self,
        _ctx: &ExecutionContext,
        _node_id: &str,
        _state: Value,
        reason: ResumeReason,
    ) -> Result<NodeOutcome, String> {
        Ok(NodeOutcome::Complete(match reason {
            ResumeReason::Timer => serde_json::json!({ "resumedBy": "timer" }),
            ResumeReason::Signal(payload) => {
                serde_json::json!({ "resumedBy": "signal", "signal": payload })
            }
        }))
    }
}
