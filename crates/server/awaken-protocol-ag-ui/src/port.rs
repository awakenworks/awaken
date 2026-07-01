//! The runtime seam the AG-UI adapter drives (DDD port).
//!
//! Implemented by the server over the neutral shared host; the adapter never
//! constructs a runtime. The vocabulary is neutral — `Message`, a step outcome, a
//! pending tool, a resume — with no AG-UI wire types, so the same host backs this
//! adapter and others on the same thread.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use serde_json::Value;

/// A tool a run parked on.
#[derive(Debug, Clone)]
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
    /// True when the *client* runs the tool and returns the result; false for a
    /// built-in tool awaiting a permission decision.
    pub client_executed: bool,
}

/// The result of one step (a turn or a resume).
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub new_messages: Vec<Message>,
    pub waiting: bool,
    pub exhausted: bool,
    pub pending: Option<Pending>,
}

/// A resume command targeting the pending tool.
#[derive(Debug, Clone)]
pub enum Resume {
    Confirm { allow: bool, note: Option<String> },
    ClientResult { content: String, is_error: bool },
}

/// A driver failure classified by fault.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Internal(String),
}

/// The neutral runtime seam. Implemented by the server over the shared host.
#[async_trait]
pub trait AgUiRuntime: Send + Sync {
    async fn run_turn(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError>;

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: Resume,
    ) -> Result<StepOutcome, DriverError>;

    async fn pending(&self, thread: &str) -> Option<Pending>;

    async fn history(&self, thread: &str) -> Vec<Message>;

    fn model(&self) -> String;
}
