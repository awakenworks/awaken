//! Server-side staged checkpoint commit (ADR-0036 / ADR-0038).
//!
//! Canonical events and outbox rows committed atomically with a checkpoint are
//! kept off the runtime-facing [`Checkpoint`] so the runtime never
//! names event/outbox vocabulary. They flow through [`CheckpointStagedWrites`]
//! and [`StagedCommitCoordinator::commit_checkpoint_staged`], which store
//! coordinators implement; the runtime-facing
//! [`CommitCoordinator::commit_checkpoint`] is equivalent to a staged commit
//! with no extra writes.

use crate::contract::outbox::OutboxMessageDraft;
use async_trait::async_trait;
use awaken_runtime_contract::contract::commit_coordinator::{
    Checkpoint, CheckpointCommitOutcome, CommitCoordinator, CommitError, ServerCanonicalEvent,
    StagedCanonicalEvent,
};
use awaken_runtime_contract::contract::event_store::{CanonicalEventDraft, EventScope};

/// Event/outbox writes committed atomically with a checkpoint, supplied by
/// server-side writers: the runtime tee's canonical drafts (drained from the
/// dispatch [`EventBuffer`](awaken_runtime_contract::contract::commit_coordinator::CanonicalEventStager)),
/// server-authored canonical events, and inline outbox rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckpointStagedWrites {
    pub canonical_drafts: Vec<StagedCanonicalEvent>,
    pub server_events: Vec<ServerCanonicalEvent>,
    pub additional_outbox: Vec<OutboxMessageDraft>,
}

impl CheckpointStagedWrites {
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
/// a staged commit with [`CheckpointStagedWrites::default`].
#[async_trait]
pub trait StagedCommitCoordinator: CommitCoordinator {
    /// Commit a checkpoint together with staged event/outbox writes in one
    /// transaction. See [`CommitCoordinator::commit_checkpoint`] for ordering
    /// and failure semantics.
    async fn commit_checkpoint_staged(
        &self,
        plan: Checkpoint,
        staged: CheckpointStagedWrites,
    ) -> Result<CheckpointCommitOutcome, CommitError>;
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
        assert!(CheckpointStagedWrites::default().is_empty());
    }

    #[test]
    fn validate_accepts_matching_scope() {
        let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(draft("RunStarted", "t", "r")),
        ]);
        staged.validate("t", "r").unwrap();
    }

    #[test]
    fn validate_rejects_wrong_thread_scope() {
        let staged = CheckpointStagedWrites::default().with_canonical_drafts(vec![
            StagedCanonicalEvent::new(draft("RunStarted", "other", "r")),
        ]);
        let err = staged.validate("t", "r").unwrap_err();
        assert!(matches!(err, CommitError::Validation(m) if m.contains("thread scope")));
    }

    #[test]
    fn validate_rejects_wrong_run_scope() {
        let staged =
            CheckpointStagedWrites::default().with_server_events(vec![ServerCanonicalEvent::new(
                draft("RunSubmitted", "t", "other"),
            )]);
        let err = staged.validate("t", "r").unwrap_err();
        assert!(matches!(err, CommitError::Validation(m) if m.contains("run scope")));
    }
}
