// @formal surface obligation=live_inbox.attempt_scope_is_local_current_capability_gated_and_never_carried_over
// @formal reason: The driving port's typed failures, including `Full` backpressure from a
// @formal reason: bounded attempt inbox (ADR-0102 D5), never route around a live attempt.
//! The live inbox driving port: the editable queue of messages addressed to a
//! Session's in-flight Run, how a snapshot of it reads, how an edit can fail,
//! and the single application that owns editing. Protocol adapters call it;
//! the Session application implements it. The execution contract's
//! `LiveInboxAccess` role reads and edits the same values on the Worker side.

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;

use crate::SessionId;

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
        session_id: &SessionId,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError>;

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError>;

    async fn remove(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
        message_id: u64,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn replace(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
        message_id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxApplicationError>;

    async fn reorder(
        &self,
        workspace_id: &str,
        session_id: &SessionId,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxApplicationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Item 4: `LiveInboxError` Display messages are stable, distinct wire text.
    #[test]
    fn live_inbox_error_display_messages_are_pinned() {
        // Test design — Causes: each closed LiveInbox failure variant is rendered.
        // Effects: it produces its exact distinct operator-facing text.
        // Constraints/invariants: display text is a pinned wire/debug boundary;
        // variants never alias. Decision rule L1: enumerate all variants=>five
        // exact, pairwise-distinct messages.
        assert_eq!(
            LiveInboxError::Inactive.to_string(),
            "no Run is in flight; send the message as a normal event",
        );
        assert_eq!(
            LiveInboxError::UnknownMessage.to_string(),
            "no queued message with that id",
        );
        assert_eq!(
            LiveInboxError::StaleOrder.to_string(),
            "proposed order does not match the current queue",
        );
        assert_eq!(
            LiveInboxError::Unavailable.to_string(),
            "the live-inbox owner registry is unreadable",
        );
        assert_eq!(
            LiveInboxError::Full.to_string(),
            "the live inbox is full; retry after the Run drains it",
        );
    }

    // Item 5: `LiveInboxSnapshot::inactive()` invariants.
    #[test]
    fn inactive_snapshot_is_empty_versionless_and_inactive() {
        // Test design — Cause: no Run is in flight, so the canonical inactive
        // constructor is selected. Effects: active=false, version=0, and no
        // queued messages. Constraints/invariants: absence carries neither a
        // resumable version nor latent inbox state. Decision rule I1: inactive
        // constructor=>assert the complete three-field boundary tuple.
        let snap = LiveInboxSnapshot::inactive();
        assert!(!snap.active);
        assert_eq!(snap.version, 0);
        assert!(snap.messages.is_empty());
    }
}
