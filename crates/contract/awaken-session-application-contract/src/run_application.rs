//! Hosted Session Run admission extensions.
use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::runs::{RunError, StepOutcome};

/// Narrow application port for an explicit newest-wins Session Run.
///
/// Operational HTTP may expose this command, but only the Session application
/// may turn it into a reservation/activity/activation sequence. Keeping it
/// separate from ordinary [`awaken_runtime_contract::runs::Runs`] prevents public protocols from
/// silently changing append semantics.
#[async_trait]
pub trait SessionRunReplacementApplication: Send + Sync {
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError>;
}

/// Narrow application port for publishing a Session Run without observing it.
///
/// Background submission changes only the caller's observation policy. It must
/// still reserve the Run, commit the exact Session activity receipt, and publish
/// that reservation through the same Session-owned admission state machine as a
/// foreground protocol request.
#[async_trait]
pub trait SessionRunBackgroundApplication: Send + Sync {
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<RunId, RunError>;

    /// Recover only the exact original immutable admission, including Task
    /// tenure coordinates. A receipt outranks the Task's later held/Ended phase;
    /// absence alone grants no new admission. Unsupported adapters fail closed.
    async fn task_attention_receipt(
        &self,
        _attention: crate::TaskAttention,
    ) -> Result<Option<crate::TaskAttentionAck>, RunError>;

    /// Admit the background Run that delivers one Task observation, through
    /// the same reservation, activity and activation as
    /// [`Self::submit_session_run_background`]. The Run's dispatch row carries
    /// `attention.input` privately. The acknowledgement is returned only after
    /// the Run is durably activated, so its sender may stop retrying; the same
    /// operation id with the same input returns the same Run.
    async fn submit_task_attention(
        &self,
        attention: crate::TaskAttention,
    ) -> Result<crate::TaskAttentionAck, RunError>;
}
