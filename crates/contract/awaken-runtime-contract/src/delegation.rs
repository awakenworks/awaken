//! Runtime interface for executing a first-class delegated child Run.
//!
//! Local and remote Agents implement the same lifecycle. Their adapters may
//! differ, but the runtime always supplies durable parent/call/child/result
//! identities and receives either an ended result or an awaiting continuation.

use async_trait::async_trait;
pub use awaken_agent_contract::agent::delegation::{ChildRunCancellation, DelegationLimits};
use awaken_agent_contract::agent::delegation::{
    DelegationId, DelegationOrigin, DelegationRegistry,
};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::CancellationToken;
use crate::llm::ThreadUsage;
use crate::resume::ResumeResult;
use crate::runtime_context::RuntimeRunContext;

/// Start a child Run. `arguments` remains the model-visible tool payload because
/// the executor owns that tool's schema; identity and cancellation are typed.
pub struct DelegationRequest {
    pub origin: DelegationOrigin,
    pub child_run_id: RunId,
    /// The parent session whose durable execution capabilities and commit
    /// boundary recover this child. The child keeps its own Run/Thread identity;
    /// this is routing, not domain ownership.
    pub parent_thread_id: ThreadId,
    pub arguments: Value,
    /// The same resolved execution wiring a directly initiated Run receives.
    pub context: RuntimeRunContext,
}

/// Resume a child Run that previously reached an awaiting boundary.
///
/// The result stays typed all the way from the protocol-facing parent Run to the
/// child Run. In particular, a tool-permission decision must never be flattened
/// into free-form text: the child validates it against its own committed
/// `ResumeTicket` exactly like a directly admitted Run.
pub struct DelegationResume {
    pub origin: DelegationOrigin,
    pub child_run_id: RunId,
    pub parent_thread_id: ThreadId,
    pub continuation: Value,
    pub result: ResumeResult,
    pub context: RuntimeRunContext,
}

/// One durable boundary reached by a delegated child Run.
pub enum DelegationStep {
    /// The child Run ended and produced its terminal result.
    Ended { text: String, usage: ThreadUsage },
    /// The child Run awaits input under this opaque durable continuation.
    Awaiting { continuation: Value },
}

/// A child Run's ended value durably waiting for its parent tool call to consume
/// it. This is a delivery envelope, not a second terminal result: it is removed
/// in the same parent commit that installs the result in `ToolBatch`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildRunResult {
    pub child_run_id: RunId,
    pub text: String,
    pub usage: ThreadUsage,
}

/// Run-scoped reliable-delivery inbox between child completion and parent result
/// consumption. A stable delegation id makes recording idempotent across retries.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildRunResultInbox {
    pending: BTreeMap<DelegationId, ChildRunResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultRecord {
    Recorded,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryPhase {
    Absent,
    Ready,
    Consumed,
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryTransition {
    Record,
    Consume,
    EndParent,
}

/// Heap-free reliable-delivery kernel shared by the durable inbox and Kani.
/// `None` means the transition is illegal; a returned `false` effect is an
/// idempotent retry that leaves the phase unchanged.
fn transition_delivery(
    phase: DeliveryPhase,
    transition: DeliveryTransition,
) -> Option<(DeliveryPhase, bool)> {
    use DeliveryPhase::{Absent, Consumed, Discarded, Ready};
    use DeliveryTransition::{Consume, EndParent, Record};
    match (phase, transition) {
        (Absent, Record) => Some((Ready, true)),
        (Ready, Record) => Some((Ready, false)),
        (Ready, Consume) => Some((Consumed, true)),
        (Absent | Ready, EndParent) => Some((Discarded, true)),
        (Discarded, EndParent) => Some((Discarded, false)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegationResultError {
    #[error("delegation result identity conflicts with an existing result")]
    Conflict,
    #[error("delegation result does not name the relationship's stable child Run")]
    ChildRunMismatch,
    #[error("delegation result is not pending")]
    NotFound,
}

impl ChildRunResultInbox {
    #[must_use]
    pub fn get(&self, id: &DelegationId) -> Option<&ChildRunResult> {
        self.pending.get(id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn record(
        &mut self,
        id: DelegationId,
        result: ChildRunResult,
    ) -> Result<ResultRecord, DelegationResultError> {
        if result.child_run_id != id.child_run_id() {
            return Err(DelegationResultError::ChildRunMismatch);
        }
        match self.pending.get(&id) {
            Some(existing) if existing == &result => {
                let transition =
                    transition_delivery(DeliveryPhase::Ready, DeliveryTransition::Record);
                debug_assert_eq!(transition, Some((DeliveryPhase::Ready, false)));
                Ok(ResultRecord::Duplicate)
            }
            Some(_) => Err(DelegationResultError::Conflict),
            None => {
                let transition =
                    transition_delivery(DeliveryPhase::Absent, DeliveryTransition::Record);
                debug_assert_eq!(transition, Some((DeliveryPhase::Ready, true)));
                self.pending.insert(id, result);
                Ok(ResultRecord::Recorded)
            }
        }
    }

    pub fn consume(&mut self, id: &DelegationId) -> Result<ChildRunResult, DelegationResultError> {
        let result = self
            .pending
            .remove(id)
            .ok_or(DelegationResultError::NotFound)?;
        debug_assert_eq!(
            transition_delivery(DeliveryPhase::Ready, DeliveryTransition::Consume),
            Some((DeliveryPhase::Consumed, true))
        );
        Ok(result)
    }

    /// Discard every result that the parent can no longer consume. The parent
    /// terminal commit removes the state cell after this transition.
    pub fn discard_on_parent_end(&mut self) -> usize {
        let count = self.pending.len();
        for _ in self.pending.values() {
            debug_assert_eq!(
                transition_delivery(DeliveryPhase::Ready, DeliveryTransition::EndParent),
                Some((DeliveryPhase::Discarded, true))
            );
        }
        self.pending.clear();
        count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationFailureKind {
    Terminal,
    Retryable,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DelegationExecutionError {
    message: String,
    kind: DelegationFailureKind,
}

impl DelegationExecutionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DelegationFailureKind::Terminal,
        }
    }

    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DelegationFailureKind::Retryable,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> DelegationFailureKind {
        self.kind
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self.kind, DelegationFailureKind::Retryable)
    }
}

/// Runtime-facing interface for delegated child Runs. This is the runtime API,
/// not a generic "port": its methods name Agent-domain operations directly.
#[async_trait]
pub trait RunDelegationService: Send + Sync {
    /// Model-visible delegation tool handled by this executor.
    fn tool_id(&self) -> &str;

    /// Extract the target Agent identity from this tool's model-visible payload.
    /// The runtime needs the identity only for durable lineage/budget checks; the
    /// tool implementation continues to own its schema and placement decision.
    fn target_agent_id(&self, arguments: &Value) -> Result<String, DelegationExecutionError> {
        arguments
            .get("agent_id")
            .and_then(Value::as_str)
            .filter(|agent_id| !agent_id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| DelegationExecutionError::new("delegation target agent is missing"))
    }

    /// Whether this request is guaranteed to run to a terminal child boundary
    /// without asking the parent for input. Such calls from one model batch may
    /// execute concurrently while retaining an ordered publication barrier.
    fn supports_parallel_completion(&self, _arguments: &Value) -> bool {
        false
    }

    /// Start or reconnect to the durable request identified by
    /// `origin.delegation_id` / `child_run_id`. Repeated calls with the same
    /// identity MUST address the same child Run; creating a second child is a
    /// contract violation.
    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError>;

    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError>;

    /// Idempotently deliver a cancellation that was committed with the parent
    /// Run's terminal state. Implementations must treat retries as addressing
    /// the same child Run, never as a new operation.
    async fn cancel(
        &self,
        _cancellation: ChildRunCancellation,
    ) -> Result<(), DelegationExecutionError> {
        Err(DelegationExecutionError::new(
            "Run delegation service does not support durable cancellation",
        ))
    }
}

/// The current Run's relationship registry, committed beside `ToolBatch` through
/// the ordinary thread state log. There is intentionally no delegation store.
pub struct RunDelegations;

impl StateKey for RunDelegations {
    const KEY: &'static str = "runtime.delegations.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<DelegationRegistry>;
}

/// The current parent Run's unconsumed child results. Kept separate from
/// `RunDelegations` (relationship identity) and `ActiveToolBatch` (consumed tool
/// result) so each fact has one owner.
pub struct PendingChildRunResults;

impl StateKey for PendingChildRunResults {
    const KEY: &'static str = "runtime.delegation-results.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = ChildRunResultInbox;
}

/// A registered Agent hosted outside this process. Protocol-specific task ids,
/// polling, and cancellation remain inside its adapter.
#[async_trait]
pub trait RemoteAgent: Send + Sync {
    async fn run(
        &self,
        agent_id: &str,
        request_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegationStep, DelegationExecutionError>;

    async fn card(&self, agent_id: &str) -> Result<Value, DelegationExecutionError>;

    /// Idempotently cancel the remote child addressed by the durable execution
    /// reference returned at its awaiting boundary.
    async fn cancel(
        &self,
        _agent_id: &str,
        _child_run_id: &RunId,
        _execution_reference: Option<&Value>,
    ) -> Result<(), DelegationExecutionError> {
        Err(DelegationExecutionError::new(
            "remote Agent does not support durable cancellation",
        ))
    }
}

#[cfg(test)]
mod result_tests {
    use super::*;

    fn result(id: &DelegationId, text: &str) -> ChildRunResult {
        ChildRunResult {
            child_run_id: id.child_run_id(),
            text: text.into(),
            usage: ThreadUsage::default(),
        }
    }

    #[test]
    fn result_is_recorded_once_and_consumed_once() {
        let id = DelegationId("d".into());
        let mut inbox = ChildRunResultInbox::default();
        assert_eq!(
            inbox.record(id.clone(), result(&id, "done")),
            Ok(ResultRecord::Recorded)
        );
        assert_eq!(
            inbox.record(id.clone(), result(&id, "done")),
            Ok(ResultRecord::Duplicate)
        );
        assert_eq!(inbox.consume(&id).unwrap().text, "done");
        assert_eq!(inbox.consume(&id), Err(DelegationResultError::NotFound));
        assert!(inbox.is_empty());
    }

    #[test]
    fn conflicting_or_wrong_child_result_fails_closed() {
        let id = DelegationId("d".into());
        let mut inbox = ChildRunResultInbox::default();
        inbox.record(id.clone(), result(&id, "one")).unwrap();
        assert_eq!(
            inbox.record(id.clone(), result(&id, "two")),
            Err(DelegationResultError::Conflict)
        );
        assert_eq!(
            ChildRunResultInbox::default().record(
                id,
                ChildRunResult {
                    child_run_id: RunId("wrong".into()),
                    text: String::new(),
                    usage: ThreadUsage::default(),
                }
            ),
            Err(DelegationResultError::ChildRunMismatch)
        );
    }

    #[test]
    fn parent_end_discards_every_unconsumed_result() {
        let first = DelegationId("first".into());
        let second = DelegationId("second".into());
        let mut inbox = ChildRunResultInbox::default();
        inbox.record(first.clone(), result(&first, "one")).unwrap();
        inbox
            .record(second.clone(), result(&second, "two"))
            .unwrap();
        assert_eq!(inbox.discard_on_parent_end(), 2);
        assert!(inbox.is_empty());
    }
}

#[cfg(kani)]
mod delivery_verification {
    use super::*;

    fn symbolic_phase(tag: u8) -> DeliveryPhase {
        match tag % 4 {
            0 => DeliveryPhase::Absent,
            1 => DeliveryPhase::Ready,
            2 => DeliveryPhase::Consumed,
            _ => DeliveryPhase::Discarded,
        }
    }

    fn symbolic_transition(tag: u8) -> DeliveryTransition {
        match tag % 3 {
            0 => DeliveryTransition::Record,
            1 => DeliveryTransition::Consume,
            _ => DeliveryTransition::EndParent,
        }
    }

    #[kani::proof]
    fn child_result_is_consumed_only_from_ready() {
        let phase = symbolic_phase(kani::any());
        let result = transition_delivery(phase, DeliveryTransition::Consume);
        assert_eq!(result.is_some(), phase == DeliveryPhase::Ready);
        if let Some((next, applied)) = result {
            assert_eq!(next, DeliveryPhase::Consumed);
            assert!(applied);
        }
    }

    #[kani::proof]
    fn terminal_delivery_phases_never_reopen() {
        let phase = if kani::any() {
            DeliveryPhase::Consumed
        } else {
            DeliveryPhase::Discarded
        };
        let transition = symbolic_transition(kani::any());
        let result = transition_delivery(phase, transition);
        assert!(!result.is_some_and(|(next, applied)| {
            applied && matches!(next, DeliveryPhase::Absent | DeliveryPhase::Ready)
        }));
    }
}
