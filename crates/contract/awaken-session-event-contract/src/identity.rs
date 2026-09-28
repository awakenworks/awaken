// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: stable Session Event batch, operation and Run identities plus the closed
// @formal reason: Agent-run boundary moved from the Session facade into the extracted Event vocabulary
// @formal reason: owner. Dynamic path: a committed Session revision deterministically selects one batch
// @formal reason: and each ordinal one operation, preserving replay identity without a clock or second
// @formal reason: registry. Adjacent tests qualify the codec and identity partitions; this is not a proof
// @formal reason: of persistence or protocol delivery.
use awaken_agent_contract::RunLifecycleEventKind;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_session_types::{SessionId, SessionRevision};

use crate::SessionEventBatchError;

const SESSION_EVENT_BATCH_OPERATION_PREFIX: &str = "session-event-batch-v1:";

const SESSION_ROOT_REVISION_BATCH_PREFIX: &str = "session-root-revision-v1:";
pub const MAX_SESSION_INITIAL_EVENTS: usize = 50;
/// Stable retry classification when another Outcome owns the Thread aggregate.
pub const OUTCOME_BUSY_CODE: &str = "outcome_busy";

/// Exact committed lifecycle class consumed by Agent-message settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAgentRunBoundary {
    Awaiting,
    Completed,
    Failed,
    Cancelled,
}

impl SessionAgentRunBoundary {
    /// The one Run-boundary classification of a committed lifecycle kind.
    ///
    /// Settlement, the Managed status/idle projections, interval aggregation
    /// and the canonical event order all key on this same set; they call this
    /// instead of re-spelling `Awaiting | Completed | Failed | Cancelled`, so a
    /// new lifecycle kind cannot join one consumer's boundary set and not the
    /// others'.
    #[must_use]
    pub const fn of_lifecycle_kind(kind: RunLifecycleEventKind) -> Option<Self> {
        match kind {
            RunLifecycleEventKind::Awaiting => Some(Self::Awaiting),
            RunLifecycleEventKind::Completed => Some(Self::Completed),
            RunLifecycleEventKind::Failed => Some(Self::Failed),
            RunLifecycleEventKind::Cancelled => Some(Self::Cancelled),
            RunLifecycleEventKind::Running
            | RunLifecycleEventKind::Resumed
            | RunLifecycleEventKind::Rescheduled => None,
        }
    }

    /// Whether a committed lifecycle kind closes one Run activity interval.
    #[must_use]
    pub const fn is_boundary(kind: RunLifecycleEventKind) -> bool {
        Self::of_lifecycle_kind(kind).is_some()
    }
}

/// Derive the only ordinary Event-batch identity from its prospective committed
/// Session root revision. Fixed-width decimal encoding preserves FIFO ordering
/// without a clock, random id, or process-local counter.
pub fn session_event_batch_id(
    session_id: &SessionId,
    committed_revision: SessionRevision,
) -> Result<String, SessionEventBatchError> {
    if session_id.as_str().trim().is_empty() {
        return Err(SessionEventBatchError::EmptySessionId);
    }
    if committed_revision.0 == 0 {
        return Err(SessionEventBatchError::InvalidBatchRevision);
    }
    Ok(format!(
        "{SESSION_ROOT_REVISION_BATCH_PREFIX}{:020}",
        committed_revision.0
    ))
}

/// Borrowed coordinate decoded from one canonical Session Event operation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEventBatchOperation<'a> {
    pub batch_id: &'a str,
    pub ordinal: usize,
}

/// Encode one Event's stable operation coordinate. Byte-length framing keeps an
/// opaque batch id containing `:` exactly reversible without another registry.
pub fn session_event_batch_operation(
    batch_id: &str,
    ordinal: usize,
) -> Result<String, SessionEventBatchError> {
    if batch_id.is_empty() {
        return Err(SessionEventBatchError::EmptyBatchId);
    }
    Ok(format!(
        "{SESSION_EVENT_BATCH_OPERATION_PREFIX}{}:{batch_id}:{ordinal}",
        batch_id.len()
    ))
}

/// Decode only the canonical representation produced by
/// [`session_event_batch_operation`].
#[must_use]
pub fn decode_session_event_batch_operation(
    operation_id: &str,
) -> Option<SessionEventBatchOperation<'_>> {
    let encoded = operation_id.strip_prefix(SESSION_EVENT_BATCH_OPERATION_PREFIX)?;
    let (batch_len, payload) = encoded.split_once(':')?;
    let batch_len = batch_len.parse::<usize>().ok()?;
    let batch_id = payload.get(..batch_len)?;
    let ordinal = payload.get(batch_len..)?.strip_prefix(':')?;
    let ordinal = ordinal.parse::<usize>().ok()?;
    if batch_id.is_empty() {
        return None;
    }
    let coordinate = SessionEventBatchOperation { batch_id, ordinal };
    (session_event_batch_operation(batch_id, ordinal)
        .ok()
        .as_deref()
        == Some(operation_id))
    .then_some(coordinate)
}

/// Stable User Run identity shared by create-time and ordinary Session Event
/// batches. The operation id is already the unique batch/ordinal coordinate.
#[must_use]
pub fn session_event_user_run_id(session_id: &SessionId, operation_id: &str) -> RunId {
    RunId(format!(
        "session-event-run-{}",
        awaken_agent_contract::stable_fingerprint(&(
            "session-event-user-run-v1",
            session_id,
            operation_id,
        ))
    ))
}

/// Stable Outcome identity shared by create-time and ordinary Session Event
/// batches; ThreadOutcomeState remains the result owner.
#[must_use]
pub fn session_event_outcome_id(session_id: &SessionId, operation_id: &str) -> String {
    format!(
        "outc_{}",
        awaken_agent_contract::stable_fingerprint(&(
            "session-event-outcome-v1",
            session_id,
            operation_id,
        ))
    )
}

/// Stable identity for the legacy convenience composition that has no explicit
/// Session Event operation coordinate. This keeps all callers deterministic;
/// durable Event admission uses [`session_event_outcome_id`] instead.
#[must_use]
pub fn session_outcome_convenience_id(
    thread: &str,
    description: &str,
    rubric: &str,
    max_iterations: u32,
) -> String {
    awaken_agent_contract::stable_fingerprint(&(
        "session-outcome-convenience-v1",
        thread,
        description,
        rubric,
        max_iterations,
    ))
}
