// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: private-proof receipt and neutral Host boundary value types over canonical
// @formal reason: Run, Pending, and PermissionDecision contracts. Dynamic path: only a verified committed
// @formal reason: Run snapshot constructs the receipt consumed by the existing settled-step projection;
// @formal reason: bare executor state cannot become application completion. Evidence:
// @formal reason: runtime.agent_loop.runtime_execution and settled-step projection tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Neutral value types produced/consumed by [`ExecutorComponents`]: pending tools,
//! run results, resume commands, host errors, and outcome reports.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// The neutral resume command: answer a built-in tool's permission gate, or
/// deliver a client-executed tool's result.
pub enum HostResume {
    /// Built-in tool awaiting approval (Managed `user.tool_confirmation`; AI SDK
    /// `approval-responded` / `output-denied`).
    Permission(PermissionDecision),
    /// Client-executed tool result (Managed `user.custom_tool_result`; AI SDK
    /// `output-available` / `output-error` on a client tool part).
    ClientResult {
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    },
}

/// A Host resume is the protocol-neutral Run resume it names.
impl From<HostResume> for awaken_session_application_contract::RunResume {
    fn from(resume: HostResume) -> Self {
        match resume {
            HostResume::Permission(decision) => Self::Permission(decision),
            HostResume::ClientResult { content, is_error } => {
                Self::ClientResult { content, is_error }
            }
        }
    }
}

impl From<awaken_dispatch_erasure::RuntimeAuthorityError> for SessionExecutionError {
    fn from(error: awaken_dispatch_erasure::RuntimeAuthorityError) -> Self {
        crate::session_execution_error::from_run_error(
            awaken_session_run_delivery::runtime_authority_error(error),
        )
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
    /// What grading this round cost, observed on the Judge Run's own Thread;
    /// `None` where the round was never graded or the backend recorded none.
    pub usage: Option<awaken_agent_contract::audit::model_request::TokenUsage>,
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
