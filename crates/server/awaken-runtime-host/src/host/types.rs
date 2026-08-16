//! Neutral value types produced/consumed by [`SharedHost`]: pending tools,
//! run results, resume commands, host errors, and outcome reports.

use super::*;

/// A tool a run awaits: its id, model-visible name/input, and whether it is
/// client-executed (the caller runs it and returns a result) or a built-in tool
/// awaiting a permission decision.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// Proof that one Host step was read back from the authoritative ThreadCommit
/// prefix. Public protocols may inspect the projected result fields, but cannot
/// construct this value because the commit identity is private to the Host.
/// Consequently a bare executor terminal state cannot cross the application
/// boundary as a completed step.
pub struct CommittedStepReceipt {
    pub run_id: RunId,
    pub new_messages: Vec<Message>,
    pub state: RunState,
    pub pending: Option<PendingTool>,
    /// `true` when this turn folded its context (the compact plugin summarized
    /// older turns). Read from durable thread state at the terminal step, so a
    /// awaiting→resumed turn reports it exactly once.
    pub compacted: bool,
    /// `true` when the runtime transparently retried a transient inference failure
    /// during this turn (auto-recovery), read from the run's reschedule counter.
    pub rescheduled: bool,
    pub model_requests: Vec<awaken_runtime_contract::llm::ModelRequestObservation>,
    pub rescheduled_delegated_run_ids: std::collections::BTreeSet<String>,
    pub delegated_runs: Vec<awaken_session_contract::DelegatedRun>,
    proof: CommittedStepProof,
}

struct CommittedStepProof {
    thread_id: ThreadId,
    commit_sequence: u64,
    store_cursor: u64,
    operation_ordinal: u64,
    first_message_id: Option<MessageId>,
    last_message_id: Option<MessageId>,
}

pub(super) struct VerifiedStepProjection {
    pub run_id: RunId,
    pub new_messages: Vec<Message>,
    pub state: RunState,
    pub pending: Option<PendingTool>,
    pub compacted: bool,
    pub rescheduled: bool,
    pub model_requests: Vec<awaken_runtime_contract::llm::ModelRequestObservation>,
    pub rescheduled_delegated_run_ids: std::collections::BTreeSet<String>,
    pub delegated_runs: Vec<awaken_session_contract::DelegatedRun>,
}

impl CommittedStepReceipt {
    /// Construct only after the Run owner has verified the exact Thread, Run,
    /// input identities, committed lifecycle, and returned message suffix.
    pub(super) fn from_verified(
        projected: VerifiedStepProjection,
        committed: &RunRecoverySnapshot,
    ) -> Self {
        let first_message_id = projected
            .new_messages
            .first()
            .map(|message| message.id.clone());
        let last_message_id = projected
            .new_messages
            .last()
            .map(|message| message.id.clone());
        Self {
            run_id: projected.run_id,
            new_messages: projected.new_messages,
            state: projected.state,
            pending: projected.pending,
            compacted: projected.compacted,
            rescheduled: projected.rescheduled,
            model_requests: projected.model_requests,
            rescheduled_delegated_run_ids: projected.rescheduled_delegated_run_ids,
            delegated_runs: projected.delegated_runs,
            proof: CommittedStepProof {
                thread_id: committed.thread_id.clone(),
                commit_sequence: committed.thread_version,
                store_cursor: committed.store_cursor,
                operation_ordinal: committed.next_commit_ordinal - 1,
                first_message_id,
                last_message_id,
            },
        }
    }

    #[must_use]
    pub fn thread_id(&self) -> &ThreadId {
        &self.proof.thread_id
    }

    #[must_use]
    pub fn commit_sequence(&self) -> u64 {
        self.proof.commit_sequence
    }

    #[must_use]
    pub fn store_cursor(&self) -> u64 {
        self.proof.store_cursor
    }

    #[must_use]
    pub fn operation_ordinal(&self) -> u64 {
        self.proof.operation_ordinal
    }

    #[must_use]
    pub fn first_message_id(&self) -> Option<&MessageId> {
        self.proof.first_message_id.as_ref()
    }

    #[must_use]
    pub fn last_message_id(&self) -> Option<&MessageId> {
        self.proof.last_message_id.as_ref()
    }
}

/// The neutral resume command: answer a built-in tool's permission gate, or
/// deliver a client-executed tool's result.
pub enum HostResume {
    /// Built-in tool awaiting approval (Managed `user.tool_confirmation`; AI SDK
    /// `approval-responded` / `output-denied`).
    ToolPermission { allow: bool, note: Option<String> },
    /// Client-executed tool result (Managed `user.custom_tool_result`; AI SDK
    /// `output-available` / `output-error` on a client tool part).
    ClientResult {
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    },
}

impl HostResume {
    pub(crate) fn wants_client(&self) -> bool {
        matches!(self, HostResume::ClientResult { .. })
    }
}

/// A host failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no await), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub message: String,
    pub kind: HostErrorKind,
    /// Stable neutral classification. Protocol adapters project this value and
    /// never infer fault identity from human-readable text.
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    Internal,
    BadRequest,
    Conflict,
    Unavailable,
}

impl HostError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
            code: "internal".into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::BadRequest,
            code: "invalid_request".into(),
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Conflict,
            code: "conflict".into(),
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Unavailable,
            code: "unavailable".into(),
        }
    }
    pub fn unavailable_classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Unavailable,
            code: code.into(),
        }
    }
    pub fn classified(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
            code: code.into(),
        }
    }
}

impl From<crate::RuntimeAuthorityError> for HostError {
    fn from(error: crate::RuntimeAuthorityError) -> Self {
        match error {
            crate::RuntimeAuthorityError::Unavailable(message) => Self::unavailable(message),
            crate::RuntimeAuthorityError::Corrupt(message) => {
                Self::classified("runtime_authority_corrupt", message)
            }
            crate::RuntimeAuthorityError::Misconfigured(message) => {
                Self::classified("runtime_authority_misconfigured", message)
            }
        }
    }
}

/// One evaluation round of a goal (neutral): the revision messages committed this
/// round, the round index, the classification token, and the grader explanation.
pub struct HostOutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub description: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The neutral outcome report: the ordered evaluation rounds.
pub struct HostOutcomeReport {
    pub iterations: Vec<HostOutcomeIteration>,
}

/// Result of driving the durable Outcome aggregate to its next external
/// boundary. Awaiting is a successful boundary, not a second failure state.
pub enum HostOutcomeDrive {
    Awaiting,
    Completed(HostOutcomeReport),
}
