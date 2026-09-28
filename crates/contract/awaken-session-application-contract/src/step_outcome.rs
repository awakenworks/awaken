// @formal surface obligation=run.lifecycle_absorption
// @formal reason: Static owner: the sole Session runtime-port projection of one settled Run step, reusing
// @formal reason: canonical RunState, Pending, Failure, and Run identity instead of defining a second
// @formal reason: terminal classification. Dynamic path: constructors admit only Awaiting or Ended,
// @formal reason: terminal event projection is derived from that same state, and Running cannot escape a
// @formal reason: settled step boundary. The linked absorption obligation covers the canonical Run state
// @formal reason: machine; message delivery and protocol deduplication remain executable boundary
// @formal reason: evidence.
// Settled Run-step outcome projected through the Session runtime port.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};

use crate::{DelegatedRun, Pending};

/// The result of running one settled Step (a new Run, or a resume).
///
/// `state` reuses the run's sole lifecycle authority instead of storing a second
/// terminal classification. It is private: callers can construct only
/// `Awaiting` or `Ended` outcomes, so `Running` cannot escape a step boundary.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub new_messages: Vec<Message>,
    /// Exact identity of the committed Run when supplied by a durable runtime.
    /// Protocol projections use it only to deduplicate the same terminal fact
    /// observed locally and through the committed lifecycle feed.
    run_id: Option<RunId>,
    state: RunState,
    pending: Option<Pending>,
    await_reason: Option<awaken_agent_contract::agent::awaiting::AwaitReason>,
    delegated_runs: Vec<DelegatedRun>,
}

impl StepOutcome {
    #[must_use]
    pub fn awaiting(messages: Vec<Message>, pending: Option<Pending>) -> Self {
        Self {
            new_messages: messages,
            run_id: None,
            state: RunState::Awaiting,
            pending,
            await_reason: None,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn ended(messages: Vec<Message>, cause: EndCause) -> Self {
        Self {
            new_messages: messages,
            run_id: None,
            state: RunState::Ended(cause),
            pending: None,
            await_reason: None,
            delegated_runs: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &RunState {
        &self.state
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    #[must_use]
    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    /// Neutral terminal fact derived from the authoritative Run state.
    #[must_use]
    pub fn terminal_event(&self) -> awaken_agent_contract::event::Fact {
        awaken_agent_contract::event::terminal(
            &self.state,
            self.pending
                .as_ref()
                .map(|pending| (pending.tool_use_id.as_str(), pending.client_executed)),
        )
    }

    #[must_use]
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    #[must_use]
    pub fn with_await_reason(
        mut self,
        reason: awaken_agent_contract::agent::awaiting::AwaitReason,
    ) -> Self {
        self.await_reason = Some(reason);
        self
    }

    #[must_use]
    pub fn await_reason(&self) -> Option<&awaken_agent_contract::agent::awaiting::AwaitReason> {
        self.await_reason.as_ref()
    }

    #[must_use]
    pub fn failure(&self) -> Option<&Failure> {
        match &self.state {
            RunState::Ended(EndCause::Error(failure)) => Some(failure),
            RunState::Running | RunState::Awaiting | RunState::Ended(_) => None,
        }
    }

    #[must_use]
    pub fn with_delegated_runs(mut self, delegated_runs: Vec<DelegatedRun>) -> Self {
        self.delegated_runs = delegated_runs;
        self
    }

    #[must_use]
    pub fn delegated_runs(&self) -> &[DelegatedRun] {
        &self.delegated_runs
    }
}
