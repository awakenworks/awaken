// @formal surface obligation=live_inbox.attempt_scope_is_local_current_capability_gated_and_never_carried_over
// @formal reason: The driving port's typed failures, including `Full` backpressure from a
// @formal reason: bounded attempt inbox (ADR-0102 D5), never route around a live attempt.
// Live-inbox value types and the driving port that edits them.
//
// Physically split out of `session.rs` to stay under the repository file limit
// and `include!`d there, so every name keeps its public defining path at
// `session::…` and no consumer import changes. One bounded responsibility:
// what a queued message is, how a snapshot of the queue reads, how an edit can
// fail, and the single application that owns editing.

/// A queued live-inbox message on the Session's in-flight Run. `id` is the
/// runtime's queue identity — targetable until the engine consumes the entry.
/// Neutral: the adapter projects it onto the wire snapshot at the route.
#[derive(Debug, Clone)]
pub struct LiveInboxEntry {
    pub id: u64,
    pub content: Vec<ContentBlock>,
}

/// The session's live-inbox resource: the editable queue of messages addressed
/// to the in-flight Run. `active: false` means no native Run is executing (the
/// queue shows empty; sends go through the normal event path instead). Neutral —
/// the wire shaping (`Json`) lives in the `ext::live_inbox` route.
#[derive(Debug, Clone)]
pub struct LiveInboxSnapshot {
    pub active: bool,
    pub version: u64,
    pub messages: Vec<LiveInboxEntry>,
}

impl LiveInboxSnapshot {
    pub fn inactive() -> Self {
        Self {
            active: false,
            version: 0,
            messages: Vec::new(),
        }
    }
}

/// Why a live-inbox operation failed. Mirrors the runtime contract's edit
/// errors, plus `Inactive` for "no native Run in flight on this Session",
/// `Full` for backpressure from a consumer that has not drained, and
/// `Unavailable` for "the process cannot read which attempts it owns" -- a
/// fault, never reported as an inactive queue the caller may route around.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveInboxError {
    #[error("no Run is in flight; send the message as a normal event")]
    Inactive,
    #[error("no queued message with that id")]
    UnknownMessage,
    #[error("proposed order does not match the current queue")]
    StaleOrder,
    #[error("the live-inbox owner registry is unreadable")]
    Unavailable,
    #[error("the live inbox is full; retry after the Run drains it")]
    Full,
}

/// Protocol-neutral application failure for the editable in-flight inbox.
/// Wire adapters decide how these four domain outcomes map to their envelopes.
#[derive(Debug, thiserror::Error)]
pub enum LiveInboxApplicationError {
    #[error("session not found")]
    NotFound,
    #[error(transparent)]
    Edit(#[from] LiveInboxError),
    #[error("live inbox authority unavailable: {detail}")]
    AuthorityUnavailable { detail: String },
    #[error("live inbox authority state is invalid: {detail}")]
    /// The authoritative Session state violates a durable-state invariant.
    /// An inbox edit cannot repair it; callers must preserve a permanent failure.
    InvalidSessionState { detail: String },
}

/// Driving port for Awaken's live-inbox protocol. The Session application is
/// the sole owner: protocol adapters supply the trusted Workspace scope and do
/// not perform a second ownership lookup or reach through protocol state.
#[async_trait]
pub trait LiveInboxApplication: Send + Sync {
    async fn snapshot(
        &self,
        workspace_id: &str,
        session_id: &crate::SessionId,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError>;

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &crate::SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError>;

    async fn remove(
        &self,
        workspace_id: &str,
        session_id: &crate::SessionId,
        message_id: u64,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn replace(
        &self,
        workspace_id: &str,
        session_id: &crate::SessionId,
        message_id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn reorder(
        &self,
        workspace_id: &str,
        session_id: &crate::SessionId,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxApplicationError>;
}
