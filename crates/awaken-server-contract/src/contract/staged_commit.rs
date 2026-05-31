//! Server-side staged checkpoint commit (ADR-0036 / ADR-0038).
//!
//! Canonical events and outbox rows committed atomically with a checkpoint are
//! kept off the runtime-facing [`ThreadCommit`] so the runtime never
//! names event/outbox vocabulary. They flow through [`ThreadCommitStagedWrites`]
//! and [`StagedCommitCoordinator::commit_checkpoint_staged`], which store
//! coordinators implement; the runtime-facing
//! [`CommitCoordinator::commit_checkpoint`] is equivalent to a staged commit
//! with no extra writes.

use crate::contract::outbox::OutboxMessageDraft;
use async_trait::async_trait;
use awaken_runtime_contract::contract::commit_coordinator::{
    CommitCoordinator, CommitError, StagedCanonicalEvent, ThreadCommit,
};
use awaken_runtime_contract::contract::event_store::{
    AppendOptions, CanonicalEventDraft, EventScope,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Server-authored canonical event attached to the same thread commit as
/// the state transition that made the fact true.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerCanonicalEvent {
    pub draft: CanonicalEventDraft,
    pub options: AppendOptions,
}

impl ServerCanonicalEvent {
    /// Construct a server-authored canonical event with default append options.
    #[must_use]
    pub fn new(draft: CanonicalEventDraft) -> Self {
        Self {
            draft,
            options: AppendOptions::default(),
        }
    }

    /// Attach append options (idempotency, expected cursors).
    #[must_use]
    pub fn with_options(mut self, options: AppendOptions) -> Self {
        self.options = options;
        self
    }
}

/// Outcome for advisory server canonical publication through an outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEventPublishOutcome {
    Enqueued { dedupe_key: String },
}

/// Failure surface for advisory server canonical publication.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EventPublishError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("outbox enqueue failed: {0}")]
    Enqueue(#[from] crate::contract::outbox::OutboxError),
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Long-lived publisher for advisory server-authored canonical events.
#[async_trait]
pub trait OutboxServerEventPublisher: Send + Sync {
    async fn publish(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<ServerEventPublishOutcome, EventPublishError>;
}

/// Non-replay diagnostic event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

/// Fire-and-forget diagnostic event publisher.
pub trait DiagnosticEventPublisher: Send + Sync {
    fn record(&self, event: DiagnosticEvent);
}

/// Event/outbox writes committed atomically with a checkpoint, supplied by
/// server-side writers: the runtime tee's canonical drafts (drained from the
/// dispatch [`EventBuffer`](awaken_runtime_contract::contract::commit_coordinator::CanonicalEventStager)),
/// server-authored canonical events, and inline outbox rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadCommitStagedWrites {
    pub canonical_drafts: Vec<StagedCanonicalEvent>,
    pub server_events: Vec<ServerCanonicalEvent>,
    pub additional_outbox: Vec<OutboxMessageDraft>,
}

/// Compatibility name retained for existing server/store call sites.
#[deprecated(since = "0.6.0", note = "Use `ThreadCommitStagedWrites`.")]
pub type CheckpointStagedWrites = ThreadCommitStagedWrites;

impl ThreadCommitStagedWrites {
    /// Whether there are no staged writes — a plain checkpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.canonical_drafts.is_empty()
            && self.server_events.is_empty()
            && self.additional_outbox.is_empty()
    }

    /// Attach staged canonical drafts (runtime tee).
    #[must_use]
    pub fn with_canonical_drafts(mut self, drafts: Vec<StagedCanonicalEvent>) -> Self {
        self.canonical_drafts = drafts;
        self
    }

    /// Attach server-authored canonical events.
    #[must_use]
    pub fn with_server_events(mut self, events: Vec<ServerCanonicalEvent>) -> Self {
        self.server_events = events;
        self
    }

    /// Attach inline-writer outbox rows.
    #[must_use]
    pub fn with_additional_outbox(mut self, rows: Vec<OutboxMessageDraft>) -> Self {
        self.additional_outbox = rows;
        self
    }

    /// Validate every staged write against the checkpoint's thread/run scope.
    /// Mirrors the invariants the runtime-facing plan used to enforce inline.
    pub fn validate(&self, thread_id: &str, run_id: &str) -> Result<(), CommitError> {
        for staged in &self.canonical_drafts {
            staged.draft.validate().map_err(CommitError::EventAppend)?;
            validate_event_scope_membership(&staged.draft, thread_id, run_id)?;
            staged
                .append_options
                .validate()
                .map_err(CommitError::EventAppend)?;
        }
        for event in &self.server_events {
            event.draft.validate().map_err(CommitError::EventAppend)?;
            validate_event_scope_membership(&event.draft, thread_id, run_id)?;
            event.options.validate().map_err(CommitError::EventAppend)?;
        }
        for row in &self.additional_outbox {
            row.validate().map_err(CommitError::OutboxInsert)?;
        }
        Ok(())
    }
}

/// Identifiers assigned by stores during a successful staged commit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadCommitStagedOutcome {
    /// Canonical event ids in the same order as the input
    /// `canonical_drafts`. Empty when the staged commit carried no events.
    pub canonical_event_ids: Vec<String>,
    /// Server-authored canonical event ids in the same order as the input
    /// `server_events`. Empty when the staged commit attached no server events.
    pub server_event_ids: Vec<String>,
    /// Outbox ids in the same order as `additional_outbox`. Empty when
    /// the staged commit attached no inline-writer outbox rows.
    pub additional_outbox_ids: Vec<String>,
}

fn validate_event_scope_membership(
    draft: &CanonicalEventDraft,
    thread_id: &str,
    run_id: &str,
) -> Result<(), CommitError> {
    for scope in &draft.scopes {
        match scope {
            EventScope::Thread {
                thread_id: scope_thread,
            } if scope_thread != thread_id => {
                return Err(CommitError::Validation(format!(
                    "event thread scope '{scope_thread}' must match checkpoint thread_id '{thread_id}'"
                )));
            }
            EventScope::Run { run_id: scope_run } if scope_run != run_id => {
                return Err(CommitError::Validation(format!(
                    "event run scope '{scope_run}' must match checkpoint run_id '{run_id}'"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A [`CommitCoordinator`] that can additionally commit staged event/outbox
/// writes atomically with the checkpoint. Store coordinators implement this;
/// the runtime-facing [`CommitCoordinator::commit_checkpoint`] is equivalent to
/// a staged commit with [`ThreadCommitStagedWrites::default`].
#[async_trait]
pub trait StagedCommitCoordinator: CommitCoordinator {
    /// Commit a checkpoint together with staged event/outbox writes in one
    /// transaction. See [`CommitCoordinator::commit_checkpoint`] for ordering
    /// and failure semantics.
    async fn commit_checkpoint_staged(
        &self,
        plan: ThreadCommit,
        staged: ThreadCommitStagedWrites,
    ) -> Result<ThreadCommitStagedOutcome, CommitError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::event_store::{CanonicalEventKind, EventVisibility};
    use serde_json::json;

    fn draft(kind: &str, thread_id: &str, run_id: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread(thread_id), EventScope::run(run_id)],
            CanonicalEventKind::new(kind).unwrap(),
            json!({ "kind": kind }),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    #[test]
    fn empty_is_empty() {
        assert!(ThreadCommitStagedWrites::default().is_empty());
    }

    #[test]
    fn validate_accepts_matching_scope() {
        let staged = ThreadCommitStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(draft("RunStarted", "t", "r")),
        ]);
        staged.validate("t", "r").unwrap();
    }

    #[test]
    fn validate_rejects_wrong_thread_scope() {
        let staged = ThreadCommitStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(draft("RunStarted", "other", "r")),
        ]);
        let err = staged.validate("t", "r").unwrap_err();
        assert!(matches!(err, CommitError::Validation(m) if m.contains("thread scope")));
    }

    #[test]
    fn validate_rejects_wrong_run_scope() {
        let staged = ThreadCommitStagedWrites::default().with_server_events(vec![
            ServerCanonicalEvent::new(draft("RunSubmitted", "t", "other")),
        ]);
        let err = staged.validate("t", "r").unwrap_err();
        assert!(matches!(err, CommitError::Validation(m) if m.contains("run scope")));
    }
}
