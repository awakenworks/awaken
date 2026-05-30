//! Cross-store commit coordination for runtime checkpoints (ADR-0036).
//!
//! `CommitCoordinator` is the contract-layer abstraction that owns the
//! transaction spanning `ThreadRunStore` writes and `EventStore` appends.
//! It is the only place that observes both stores at once; the stores
//! themselves remain backend-agnostic.
//!
//! Runtime tee for durable `AgentEvent` variants is folded into this
//! commit boundary through a staging step: a `CanonicalEventStager`
//! receives drafts from the reshaped `DurableEventSink`, and the
//! `LoopRunner` drains the staged drafts into a `CheckpointCommitPlan`
//! at checkpoint cadence.
//!
//! This boundary is deliberately limited to runtime checkpoints:
//! thread messages, run records, canonical event appends, and outbox rows
//! carried by the checkpoint plan. `ConfigStore` writes are outside this
//! coordinator and therefore are not atomic with checkpoints or audit
//! events. Mailbox dispatch result updates are also a separate, idempotent
//! state machine; callers should treat dispatch completion and final
//! `ThreadRunStore` run projection as eventually reconciled rather than a
//! single coordinator transaction.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use std::sync::Arc;

use super::event_store::{AppendOptions, CanonicalEventDraft, EventStoreError};
use super::message::Message;
use super::outbox::OutboxError;
use super::storage::{RunRecord, RuntimeCheckpointStore, StorageError};

// ── transaction scope id ─────────────────────────────────────────────

/// Opaque equality marker identifying the set of stores that can share
/// a single backend transaction.
///
/// Two coordinator implementations that report the same scope id are
/// guaranteed to write to backends that genuinely share a transaction
/// boundary. The string form is for diagnostics only; equality is by
/// value and is enforced at builder time per ADR-0036 D3.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionScopeId(String);

impl TransactionScopeId {
    /// Construct a scope id from a non-empty descriptor.
    pub fn new(value: impl Into<String>) -> Result<Self, CommitError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(CommitError::Validation(
                "transaction scope id must be non-empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    /// Return the opaque descriptor for diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ── canonical event stager ───────────────────────────────────────────

/// Stage canonical event drafts produced during phase execution.
///
/// This is a crate-boundary port, not a general abstraction. A single
/// runtime-owned buffer implementation is expected; the trait exists so
/// contract-layer sink code can stage drafts without naming the concrete
/// runtime type. Staging is infallible; the durable failure surface is
/// `CommitCoordinator::commit_checkpoint`.
pub trait CanonicalEventStager: Send + Sync {
    /// Push a draft into the staging buffer.
    fn stage(&self, draft: CanonicalEventDraft);
}

/// Staged canonical event together with its append options.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedCanonicalEvent {
    pub draft: CanonicalEventDraft,
    pub append_options: AppendOptions,
}

impl StagedCanonicalEvent {
    /// Construct a staged entry with default append options.
    #[must_use]
    pub fn new(draft: CanonicalEventDraft) -> Self {
        Self {
            draft,
            append_options: AppendOptions::default(),
        }
    }

    /// Attach append options (idempotency, expected cursors).
    #[must_use]
    pub fn with_options(mut self, options: AppendOptions) -> Self {
        self.append_options = options;
        self
    }
}

/// Server-authored canonical event attached to the same checkpoint plan as
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
    Enqueue(#[from] OutboxError),
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

// ── checkpoint commit plan ───────────────────────────────────────────

/// How a plan's `messages` are written to the thread's committed log
/// (ADR-0042 A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageWriteMode {
    /// Replace the thread's whole committed message list (last-writer-wins).
    /// Retained for tombstone/rollback paths and pre-migration callers; not
    /// multi-instance safe because it depends on a read-modify-write.
    #[default]
    Overwrite,
    /// Append `messages` to the committed log, guarded by
    /// `expected_message_version` (the committed message count the caller
    /// observed). Append-only plus a version guard is the multi-instance-safe
    /// write model (ADR-0042 D5).
    Append,
}

/// One atomic checkpoint commit.
///
/// `ThreadRunStore` checkpoint inputs (thread id, messages, run record) are
/// committed together with `canonical_drafts` (each appended via the shared
/// `EventStore` write) and any additional inline-writer outbox rows the
/// caller wants atomic with the checkpoint.
///
/// `message_mode` selects whether `messages` replaces the whole committed
/// list ([`MessageWriteMode::Overwrite`]) or is a delta appended to it
/// ([`MessageWriteMode::Append`]); in append mode `expected_message_version`
/// is the version guard. Coordinators route the two modes to the matching
/// `ThreadRunStore` write.
#[derive(Debug, Clone)]
pub struct CheckpointCommitPlan {
    pub thread_id: String,
    /// Overwrite: the whole committed list. Append: the delta to append.
    pub messages: Vec<Message>,
    pub message_mode: MessageWriteMode,
    /// Append-mode version guard: the committed message count the caller
    /// observed. Required for append; ignored for overwrite.
    pub expected_message_version: Option<u64>,
    pub run: RunRecord,
}

impl CheckpointCommitPlan {
    /// Build a whole-list overwrite checkpoint plan with no staged events.
    pub fn checkpoint_only(
        thread_id: impl Into<String>,
        messages: Vec<Message>,
        run: RunRecord,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            messages,
            message_mode: MessageWriteMode::Overwrite,
            expected_message_version: None,
            run,
        }
    }

    /// Build an append-delta checkpoint plan: `messages` are appended to the
    /// thread's committed log, guarded by `expected_message_version` (the
    /// committed message count the caller observed). No staged events.
    pub fn append(
        thread_id: impl Into<String>,
        messages: Vec<Message>,
        expected_message_version: Option<u64>,
        run: RunRecord,
    ) -> Self {
        Self {
            thread_id: thread_id.into(),
            messages,
            message_mode: MessageWriteMode::Append,
            expected_message_version,
            run,
        }
    }

    /// Whether this plan appends a delta (vs. overwriting the whole list).
    #[must_use]
    pub fn is_append(&self) -> bool {
        matches!(self.message_mode, MessageWriteMode::Append)
    }

    /// Pre-commit validation that mirrors the runtime invariants.
    pub fn validate(&self) -> Result<(), CommitError> {
        if self.thread_id.trim().is_empty() {
            return Err(CommitError::Validation(
                "thread_id must be non-empty".to_string(),
            ));
        }
        if self.run.thread_id != self.thread_id {
            return Err(CommitError::Validation(format!(
                "run.thread_id '{}' must match checkpoint thread_id '{}'",
                self.run.thread_id, self.thread_id
            )));
        }
        if self.run.run_id.trim().is_empty() {
            return Err(CommitError::Validation(
                "run.run_id must be non-empty".to_string(),
            ));
        }
        if self.run.agent_id.trim().is_empty() {
            return Err(CommitError::Validation(
                "run.agent_id must be non-empty".to_string(),
            ));
        }
        if self.is_append() && self.expected_message_version.is_none() {
            return Err(CommitError::Validation(
                "append checkpoints require expected_message_version".to_string(),
            ));
        }
        Ok(())
    }
}

// ── commit outcome ───────────────────────────────────────────────────

/// Identifiers assigned by stores during a successful commit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckpointCommitOutcome {
    /// Canonical event ids in the same order as the input
    /// `canonical_drafts`. Empty when the plan staged no events.
    pub canonical_event_ids: Vec<String>,
    /// Server-authored canonical event ids in the same order as the input
    /// `server_events`. Empty when the plan attached no server events.
    pub server_event_ids: Vec<String>,
    /// Outbox ids in the same order as `additional_outbox`. Empty when
    /// the plan attached no inline-writer outbox rows.
    pub additional_outbox_ids: Vec<String>,
}

// ── error ───────────────────────────────────────────────────────────

/// Failure surface for `CommitCoordinator::commit_checkpoint`.
///
/// Any variant aborts the transaction. The runtime treats this as
/// terminal for the current run per ADR-0036 D6.
#[derive(Debug, Error)]
pub enum CommitError {
    /// Plan failed pre-commit validation.
    #[error("validation error: {0}")]
    Validation(String),
    /// `ThreadRunStore` checkpoint write failed.
    #[error("thread run store write failed: {0}")]
    StoreWrite(#[from] StorageError),
    /// A version-guarded committed message append found a stale expected
    /// version — the committed log advanced under the writer. The caller
    /// reloads, re-merges its delta, recomputes the range, and retries
    /// (ADR-0042 A).
    #[error(
        "message version conflict on thread '{thread_id}': expected {expected}, actual {actual}"
    )]
    MessageVersionConflict {
        thread_id: String,
        expected: u64,
        actual: u64,
    },
    /// `EventStore::append` failed for a staged draft.
    #[error("canonical event append failed: {0}")]
    EventAppend(#[from] EventStoreError),
    /// Inline-writer outbox insert failed.
    #[error("outbox insert failed: {0}")]
    OutboxInsert(#[from] OutboxError),
    /// Backend-level commit error (transaction commit failure, network).
    #[error("commit failed: {0}")]
    Commit(String),
    /// Builder-time scope mismatch detected at runtime.
    #[error("transaction scope mismatch: {0}")]
    ScopeMismatch(String),
}

impl CommitError {
    /// Reclassify a wrapped store-level [`StorageError::VersionConflict`] from
    /// an append commit into the message-level [`CommitError::MessageVersionConflict`]
    /// carrying `thread_id`, so the append retry path can distinguish a stale
    /// version (reload-merge-retry) from other store-write failures (abort).
    /// Other errors pass through unchanged (ADR-0042 A).
    #[must_use]
    pub fn reclassify_append_conflict(self, thread_id: &str) -> Self {
        match self {
            CommitError::StoreWrite(StorageError::VersionConflict { expected, actual }) => {
                CommitError::MessageVersionConflict {
                    thread_id: thread_id.to_string(),
                    expected,
                    actual,
                }
            }
            other => other,
        }
    }
}

// ── coordinator trait ────────────────────────────────────────────────

/// Cross-store atomic commit boundary (ADR-0036 D2).
///
/// Implementations open a backend transaction, drive the
/// `ThreadRunStore` checkpoint write, append each staged canonical
/// draft (which transitively inserts the canonical outbox row in the
/// same transaction per ADR-0034 D9), insert any inline-writer outbox
/// rows from `additional_outbox`, and commit. Any failure rolls the
/// transaction back and surfaces `CommitError`.
///
/// Coordinator construction is the place where scope compatibility is
/// validated: a coordinator that pairs stores from mismatched backends
/// must return an error at construction (or expose enough surface for
/// the `RuntimeBuilder` to reject it at `build()` time). The runtime
/// does not retry across coordinators.
///
/// Out of scope: configuration writes and mailbox dispatch lifecycle
/// mutations. Those stores have their own concurrency contracts. When a
/// workflow needs checkpoint durability, it must express the write through a
/// [`CheckpointCommitPlan`]; otherwise it is intentionally outside this
/// transaction boundary.
#[async_trait]
pub trait CommitCoordinator: Send + Sync {
    /// Return the transaction scope identifier shared by the underlying
    /// `ThreadRunStore` and `EventStore`. Used by the builder to verify
    /// scope compatibility per ADR-0036 D3.
    fn scope(&self) -> TransactionScopeId;

    /// Return the runtime read port backed by the same store the coordinator
    /// commits to. The runtime uses this for resume reads (e.g. `load_run`);
    /// writes flow through [`Self::commit_checkpoint`]. The full store CRUD +
    /// query surface is a server/store concern and is intentionally not
    /// exposed to the runtime through this port.
    fn reader(&self) -> Arc<dyn RuntimeCheckpointStore>;

    /// Commit one checkpoint plan atomically. See trait docs for
    /// ordering and failure semantics.
    async fn commit_checkpoint(
        &self,
        plan: CheckpointCommitPlan,
    ) -> Result<CheckpointCommitOutcome, CommitError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::event_store::{
        CanonicalEventDraft, CanonicalEventKind, EventScope, EventVisibility,
    };
    use serde_json::json;

    fn sample_draft(kind: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t-1"), EventScope::run("run-1")],
            CanonicalEventKind::new(kind).unwrap(),
            json!({"kind": kind}),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    fn sample_run_record() -> crate::contract::storage::RunRecord {
        crate::contract::storage::RunRecord {
            run_id: "run-1".to_string(),
            thread_id: "t-1".to_string(),
            agent_id: "agent-1".to_string(),
            resolution_id: None,
            activation: None,
            ..Default::default()
        }
    }

    #[test]
    fn transaction_scope_id_rejects_blank() {
        assert!(TransactionScopeId::new("").is_err());
        assert!(TransactionScopeId::new("   ").is_err());
        assert!(TransactionScopeId::new("pg::main").is_ok());
    }

    #[test]
    fn staged_canonical_event_with_options_round_trip() {
        let draft = sample_draft("RunStarted");
        let opts = AppendOptions {
            writer_id: Some("runtime".to_string()),
            idempotency_key: Some("k-1".to_string()),
            ..Default::default()
        };
        let staged = StagedCanonicalEvent::new(draft.clone()).with_options(opts.clone());
        assert_eq!(staged.draft, draft);
        assert_eq!(staged.append_options, opts);
    }

    #[test]
    fn plan_checkpoint_only_validates() {
        let plan = CheckpointCommitPlan::checkpoint_only("t-1", Vec::new(), sample_run_record());
        plan.validate().unwrap();
    }

    #[test]
    fn plan_rejects_blank_thread_id() {
        let mut run = sample_run_record();
        run.thread_id = String::new();
        let plan = CheckpointCommitPlan::checkpoint_only("", Vec::new(), run);
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, CommitError::Validation(_)));
    }

    #[test]
    fn plan_rejects_thread_run_mismatch() {
        let mut run = sample_run_record();
        run.thread_id = "other-thread".to_string();
        let plan = CheckpointCommitPlan::checkpoint_only("t-1", Vec::new(), run);
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, CommitError::Validation(message) if message.contains("run.thread_id"))
        );
    }

    #[test]
    fn plan_rejects_blank_run_id() {
        let mut run = sample_run_record();
        run.run_id = "   ".to_string();
        let plan = CheckpointCommitPlan::checkpoint_only("t-1", Vec::new(), run);
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, CommitError::Validation(message) if message.contains("run.run_id")));
    }

    #[test]
    fn plan_rejects_blank_agent_id() {
        let mut run = sample_run_record();
        run.agent_id.clear();
        let plan = CheckpointCommitPlan::checkpoint_only("t-1", Vec::new(), run);
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, CommitError::Validation(message) if message.contains("run.agent_id"))
        );
    }

    // ── ADR-0042 A: append-delta message-write mode ──────────────────

    #[test]
    fn checkpoint_only_is_overwrite() {
        let plan = CheckpointCommitPlan::checkpoint_only(
            "t-1",
            vec![Message::user("a")],
            sample_run_record(),
        );
        assert!(!plan.is_append());
        assert_eq!(plan.message_mode, MessageWriteMode::Overwrite);
        assert_eq!(plan.expected_message_version, None);
        assert_eq!(plan.messages.len(), 1);
    }

    #[test]
    fn append_plan_carries_delta_and_expected_version() {
        let plan = CheckpointCommitPlan::append(
            "t-1",
            vec![Message::user("hi")],
            Some(3),
            sample_run_record(),
        );
        assert!(plan.is_append());
        assert_eq!(plan.message_mode, MessageWriteMode::Append);
        assert_eq!(plan.expected_message_version, Some(3));
        assert_eq!(plan.messages.len(), 1);
        plan.validate().unwrap();
    }

    #[test]
    fn append_plan_rejects_missing_expected_version() {
        let plan = CheckpointCommitPlan::append("t-1", Vec::new(), None, sample_run_record());
        assert!(plan.is_append());
        assert_eq!(plan.expected_message_version, None);
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, CommitError::Validation(message) if message.contains("expected_message_version"))
        );
    }

    #[test]
    fn append_plan_still_validates_run_thread_match() {
        let mut run = sample_run_record();
        run.thread_id = "other-thread".to_string();
        let plan = CheckpointCommitPlan::append("t-1", Vec::new(), Some(0), run);
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, CommitError::Validation(message) if message.contains("run.thread_id"))
        );
    }

    #[test]
    fn message_version_conflict_displays_thread_expected_actual() {
        let err = CommitError::MessageVersionConflict {
            thread_id: "t-1".to_string(),
            expected: 2,
            actual: 5,
        };
        let msg = err.to_string();
        assert!(msg.contains("t-1"), "missing thread_id: {msg}");
        assert!(msg.contains('2'), "missing expected: {msg}");
        assert!(msg.contains('5'), "missing actual: {msg}");
    }
}
