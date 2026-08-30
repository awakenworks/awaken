//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port. MCP and Repository entries may persist an
//! exact secret-free credential access/holder pin, but never credential material;
//! realization consumes that pin through the common exact resolver without
//! selecting another source or revision.


mod disposition;
mod persisted_session;
mod recovery;
mod repository_port;
pub use repository_port::*;
use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};

mod environment_binding;
mod repository_publication;
mod runtime_intervals;

pub use repository_publication::SessionArchiveWithRepositoryPublicationError;

use crate::ManagedLifecycleFact;

mod execution_state;
pub use execution_state::{
    SessionExecutionState, SessionExecutionStateError, SessionExecutionTransitionError,
};

/// Secret-free active MCP projection consumed by protocol adapters. It is
/// derived from the typed attachment aggregate without a JSON serialization hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleMcpServer {
    pub name: String,
    pub target: crate::McpTarget,
    pub prompts_as_skills: bool,
}

/// Monotonic root revision for every mutation of one Session aggregate.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

/// Durable retention and public-visibility state of one Session.
///
/// `Deleting` is committed before physical cleanup starts. `Deleted` is retained
/// for imported historical rows and compact tombstone projections; ordinary new
/// deletions remove the aggregate only after cleanup receipts are committed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionDisposition {
    #[default]
    Active,
    Archived {
        archived_at: String,
    },
    Deleting,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SessionDeleteDispositionClass {
    Active,
    Archived,
    Deleting,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionDeleteRequestPlan {
    transition_to_deleting: bool,
    terminalize_execution: bool,
    request_cleanup: bool,
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionDispositionTransitionError {
    #[error("cannot archive a Session while deletion is in progress or complete")]
    ArchiveAfterDelete,
}

/// Durable owner fence for all process-local Session projections. Runtime and
/// Worker identities are opaque to the Session domain.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationLease {
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

/// The durable, adapter-side configuration of one Managed session, keyed by its
/// id (which is also its thread id). Everything here is what the wire `Session`
/// object needs beyond the runtime's committed transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedSession {
    pub session_id: String,
    /// The one optimistic-concurrency fence for baseline, Resource, MCP,
    /// environment, execution, and disposition mutations. New, not-yet-inserted
    /// values use 0.
    pub revision: SessionRevision,
    /// The only immutable configuration authority. A preparation intent is
    /// consumed exactly once and replaced by its frozen baseline.
    pub baseline: crate::SessionBaselineState,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Exact durable neutral mutable tool policy. Empty is an intentional clear;
    /// public protocol tool unions are projections and never persistence truth.
    pub tools: crate::SessionToolConfiguration,
    /// All accepted Session Event batches in root-revision order. Entries remain
    /// after processing as the sole durable inbound DTO provenance; User/System
    /// completion remains owned by Dispatch/Thread and Outcome state by the
    /// Thread Outcome aggregate.
    pub event_batches: Vec<crate::SessionEventBatch>,
    /// Monotonic root-CAS environment fence for overlapping driving events.
    /// Execution and disposition remain the durable logical state; completion
    /// membership is owned by `active_activity_epochs`, while this scalar never
    /// rewinds and therefore keeps environment operations uniquely ordered.
    pub activity_epoch: u64,
    /// Epochs of driving activities that have been admitted but have not yet
    /// settled. This is Session activity truth, not a child/coordinator
    /// relationship registry. `activity_epoch` remains the monotonic
    /// environment fence; this set only prevents an out-of-order completion
    /// from closing the shared Running interval while an older activity remains.
    ///
    /// Historical Running rows deserialize with an empty set. A direct settle
    /// of their current scalar epoch is treated as the one legacy activity; a
    /// new admission supersedes that unknowable crash-orphan and starts the
    /// explicit set at its successor epoch.
    pub active_activity_epochs: BTreeSet<u64>,
    /// One continuous authoritative Running interval. It is persisted in the
    /// aggregate so process recovery and overlapping driving events cannot
    /// fabricate gaps or emit two customer-usage intervals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_interval: Option<crate::SessionRuntimeIntervalStart>,
    /// Every closed aggregate Running interval in root-revision order. This is
    /// intentionally retained for the Session lifetime: the public Events API
    /// accepts any prior event id as a page cursor, so truncating this prefix
    /// would make a valid cross-replica cursor unknowable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_runtime_intervals: Vec<crate::SessionRuntimeInterval>,
    /// Cumulative wall-clock milliseconds across closed Running intervals.
    /// Overlapping activities share one interval, so this is the authoritative
    /// non-double-counted Session runtime quantity used by list-cost pricing.
    pub runtime_active_millis: u64,
    /// Latest cumulative neutral Runtime usage observed by the Session root.
    /// Runtime remains the counter authority; retaining this root-CAS projection
    /// lets no-budget Sessions close an exact historical usage event too.
    #[serde(default)]
    pub usage_cursor: crate::ManagedBudgetUsageCursor,
    /// Exact Managed list-cost budget and immutable price snapshot. All
    /// Session threads share this root-owned admission and settlement fence.
    pub budget: crate::SessionBudgetState,
    /// Durable, secret-free execution-environment phase. Opaque bindings are
    /// interpreted only by the runtime that produced them; this aggregate owns
    /// their transition, not their substrate meaning.
    pub environment: crate::SessionEnvironmentState,
    /// The only initial and hot MCP desired-state authority.
    pub mcp: crate::SessionMcpAttachmentSet,
    /// Durable resource activation state. Its `active` manifest is the exact,
    /// secret-free Session pin; `pending` and activation records make external
    /// realization/release recoverable without importing authorization concepts.
    pub resources: crate::SessionResourceState,
    /// Continuing Session projection ownership; no process-local slot is an
    /// authority for this lease.
    pub realization: Option<SessionRealizationLease>,
    /// Initial Environment realization retry state. Kept inside the Session
    /// root so a reclaimed Worker claim cannot reset the failure budget.
    pub realization_progress: crate::SessionRealizationProgress,
    /// The only durable execution-state authority. The retained serialized key
    /// keeps historical aggregate JSON readable through the store codec.
    #[serde(rename = "status")]
    pub execution: SessionExecutionState,
    /// Retention/public-visibility is independent from execution progress.
    pub disposition: SessionDisposition,
    /// Durable intent/receipt state for terminal Runtime effects. Resource,
    /// Environment, artifact, and process cleanup project from this one fact.
    pub terminal_cleanup: crate::SessionCleanupOperation,
}

/// One durable Session together with its intrinsic Workspace partition.
///
/// Recovery consumes this envelope atomically instead of looking up an owner in
/// a second step. It contains no principal, role, policy, credential, or
/// authorization decision; `workspace_id` is resource routing state only.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScopedPersistedSession {
    pub workspace_id: String,
    pub session: PersistedSession,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionTombstone {
    pub session_id: String,
    pub deleted_revision: SessionRevision,
    pub deleted_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub payload_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionIdempotencyReceipt {
    pub payload_hash: String,
    pub committed_revision: SessionRevision,
}

/// Atomic outcome of one Session-root create command. Both variants carry the
/// repository's durable aggregate; callers must never continue from their
/// locally compiled candidate after the repository classified a replay.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionCreateResult {
    Applied(PersistedSession),
    Replayed(PersistedSession),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "the public root-mutation contract intentionally carries a complete replacement aggregate"
)]
pub enum SessionMutationPayload {
    Replace(PersistedSession),
    Delete(SessionTombstone),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionMutation {
    pub expected_revision: SessionRevision,
    pub idempotency: IdempotencyRecord,
    pub payload: SessionMutationPayload,
    #[serde(default)]
    pub lifecycle_facts: Vec<ManagedLifecycleFact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMutationResult {
    Applied { new_revision: SessionRevision },
    Replayed { new_revision: SessionRevision },
    Conflict { current_revision: SessionRevision },
    IdempotencyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionMutationValidationError {
    #[error("Session mutation idempotency key is empty")]
    EmptyIdempotencyKey,
    #[error("Session mutation payload hash is empty")]
    EmptyPayloadHash,
    #[error("Session mutation id is empty")]
    EmptySessionId,
    #[error("Session mutation revision is exhausted")]
    RevisionExhausted,
    #[error("replacement carries a revision different from expected_revision")]
    ReplacementRevisionMismatch,
    #[error("tombstone deleted_revision is not the next root revision")]
    TombstoneRevisionMismatch,
    #[error("lifecycle fact targets another Session")]
    LifecycleSessionMismatch,
    #[error("Session Running interval is inconsistent with execution state")]
    RuntimeIntervalStateMismatch,
    #[error("Session active activity epochs are inconsistent with execution state or fence")]
    ActiveActivityStateMismatch,
    #[error("lifecycle runtime interval payload is inconsistent")]
    RuntimeIntervalFactMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryConflict {
    #[error("Session already exists")]
    AlreadyExists,
    #[error("Session was deleted")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session repository is unavailable: {0}")]
    Unavailable(String),
    #[error("Session repository contains corrupt durable state: {0}")]
    Corrupt(String),
    #[error("Session repository conflict: {0}")]
    Conflict(SessionRepositoryConflict),
    #[error("Session repository rejected invalid mutation: {0}")]
    InvalidMutation(String),
}

/// Closed supervisor policy for persistence failures. Retryable outages remain
/// pending; corrupt durable truth is isolated for operator repair; command and
/// concurrency failures are returned without background replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepositoryRecoveryAction {
    Retry,
    Quarantine,
    Reject,
}

/// One corrupt durable Session row isolated by the authoritative store scan.
/// The raw aggregate is deliberately absent so recovery reporting cannot leak
/// persisted configuration or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryQuarantine {
    pub session_id: String,
    pub reason: String,
}

/// Complete result of one recovery scan. Store adapters own row decoding and
/// durable quarantine, so callers receive healthy work and isolation evidence
/// from one authority instead of maintaining a parallel recovery registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecoveryScan {
    pub sessions: Vec<ScopedPersistedSession>,
    pub quarantined: Vec<SessionRecoveryQuarantine>,
}

// In-memory and durable adapters live outward in `awaken-session-store`.
// Workspace ownership is persisted atomically beside each row through `create`;
// authorization scope decorators do not belong in this resource persistence port.

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum PayloadKind {
        Replace,
        Delete,
    }

    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        payload: PayloadKind,
        key_nonempty: bool,
        hash_nonempty: bool,
        session_id_nonempty: bool,
        revision_available: bool,
        payload_revision_exact: bool,
        lifecycle_session_exact: bool,
        expected: Result<SessionRevision, SessionMutationValidationError>,
    }

    pub(super) fn session(id: &str, revision: SessionRevision) -> PersistedSession {
        use awaken_credential_contract::{
            CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
        };

        PersistedSession {
            session_id: id.into(),
            revision,
            baseline: crate::SessionBaselineState::Preparing(crate::SessionCreationIntent {
                control: crate::ControlSessionCreationInputs {
                    mutation_policy: crate::SessionMutationPolicy::Managed,
                    environment: crate::EnvironmentSnapshot {
                        environment_id: "environment".into(),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: crate::EnvironmentFingerprint("config".into()),
                        sandbox: Default::default(),
                        sandbox_provisioning: Default::default(),
                        idle_retention: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: crate::SessionNetworkPolicy::Unrestricted,
                        credential_realization: CredentialRealizationProfile {
                            inference_holder: PlaintextHolder::new(
                                PlaintextBoundary::Workload,
                                "awaken.workload.acp",
                            ),
                            mcp_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                        },
                    },
                    runtime_placement: crate::SessionRuntimePlacement::Local,
                    agent_id: "assistant".into(),
                    agent_revision: None,
                    model: "model".into(),
                    execution_model_ref: "model".into(),
                    model_override: None,
                    system_prompt: crate::SessionSystemPromptSelection::Inherit,
                    runtime: None,
                    mcp_authoring: Default::default(),
                    toolsets: Vec::new(),
                    delegate_ids: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            }),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: Default::default(),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget: Default::default(),
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            realization_progress: Default::default(),
            execution: SessionExecutionState::Idle,
            disposition: SessionDisposition::Active,
            terminal_cleanup: Default::default(),
        }
    }

    #[test]
    fn frozen_constructor_owns_the_complete_initial_aggregate_shape() {
        // Cause/effect graph: C1 complete typed creation input is compiled; C2
        // no durable mutation has occurred. Effects: E1 every caller gets one
        // frozen baseline with Preparing execution, zero revision/activity,
        // complete effect aggregates, and exact presentation data; E2 no
        // protocol can construct a durable partially-authored root.
        //
        // | Rule | typed input | prior mutation | Effect |
        // | C1 | complete/frozen | no | E1 canonical aggregate |
        // | C2 | wire-specific defaults | no | E2 impossible at constructor |
        // Constraints/invariants: construction starts at revision/activity zero
        // with one complete frozen baseline; adapters cannot author partial roots.
        let fixture = session("constructor-source", SessionRevision(0));
        let crate::SessionBaselineState::Preparing(intent) = fixture.baseline else {
            panic!("fixture carries a creation intent");
        };
        let compiled = intent.finalize().expect("complete fixture compiles");
        let metadata = BTreeMap::from([("key".to_string(), "value".to_string())]);
        let prepared = PersistedSession::frozen_with_budget(
            "constructor",
            compiled.baseline,
            Default::default(),
            Default::default(),
            Some("title".into()),
            metadata.clone(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(prepared.session_id, "constructor", "C1/E1");
        assert_eq!(prepared.revision, SessionRevision(0), "C1/E1");
        assert_eq!(
            prepared.execution,
            SessionExecutionState::Preparing,
            "C1/E1"
        );
        assert_eq!(prepared.disposition, SessionDisposition::Active, "C1/E1");
        assert_eq!(prepared.activity_epoch, 0, "C1/E1");
        assert!(prepared.active_activity_epochs.is_empty(), "C1/E1");
        assert!(prepared.frozen_baseline().is_some(), "C1/E1");
        assert_eq!(prepared.title.as_deref(), Some("title"), "C1/E1");
        assert_eq!(prepared.metadata, metadata, "C1/E1");
        assert!(prepared.mcp.attachments.is_empty(), "C1/E1");
        assert!(prepared.resources.active.inputs().is_empty(), "C1/E1");
        assert!(prepared.realization.is_none(), "C1/E1");
        assert!(
            matches!(
                prepared.terminal_cleanup,
                crate::SessionCleanupOperation::NotRequested
            ),
            "C1/E1"
        );
    }

    #[test]
    fn persisted_session_accepts_only_the_complete_canonical_grammar() {
        // Grammar cause/effect partition: C1 complete canonical aggregate;
        // C2 unknown status; C3 missing pre-existing required fact; C4 missing
        // Event-batch truth; C5 missing active-activity truth; C6 old status
        // alias; C7 unknown top-level fact. E1 is exact decode and E2 is a
        // fail-closed decode error. Rules S1=C1=>E1 and S2..S7=C2..C7=>E2.
        // Recovery never synthesizes domain truth from defaults, SQL columns,
        // or alternate spellings.
        // Constraints/invariants: the persisted aggregate has one closed grammar;
        // missing, aliased, or unknown facts never receive compatibility defaults.
        let value = serde_json::to_value(session("session-1", SessionRevision(1))).unwrap();
        assert_eq!(value.get("status"), Some(&serde_json::json!("idle")));
        assert!(value.get("lifecycle").is_none());
        assert!(
            serde_json::from_value::<PersistedSession>(value.clone()).is_ok(),
            "S1"
        );

        let mut unknown = value.clone();
        unknown["status"] = serde_json::json!("legacy-unknown");
        assert!(
            serde_json::from_value::<PersistedSession>(unknown).is_err(),
            "S2"
        );

        let mut missing = value.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("realization_progress");
        assert!(
            serde_json::from_value::<PersistedSession>(missing).is_err(),
            "S3"
        );

        let mut missing_batches = value.clone();
        missing_batches
            .as_object_mut()
            .unwrap()
            .remove("event_batches");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_batches).is_err(),
            "S4"
        );

        let mut missing_activities = value.clone();
        missing_activities
            .as_object_mut()
            .unwrap()
            .remove("active_activity_epochs");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_activities).is_err(),
            "S5"
        );

        let mut aliased = value.clone();
        let status = aliased.as_object_mut().unwrap().remove("status").unwrap();
        aliased["lifecycle"] = status;
        assert!(
            serde_json::from_value::<PersistedSession>(aliased).is_err(),
            "S6"
        );

        let mut extra = value;
        extra["unknown"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<PersistedSession>(extra).is_err(),
            "S7"
        );
    }

    #[test]
    fn repository_failure_policy_is_closed_and_exhaustive() {
        use SessionRepositoryError as Error;
        use SessionRepositoryRecoveryAction as Action;

        assert_eq!(
            Error::Unavailable("db offline".into()).recovery_action(),
            Action::Retry
        );
        assert_eq!(
            Error::Corrupt("negative revision".into()).recovery_action(),
            Action::Quarantine
        );
        assert_eq!(Error::NotFound.recovery_action(), Action::Reject);
        assert_eq!(
            Error::Conflict(SessionRepositoryConflict::AlreadyExists).recovery_action(),
            Action::Reject
        );
        assert_eq!(
            Error::InvalidMutation("bad revision".into()).recovery_action(),
            Action::Reject
        );
    }

    #[test]
    fn execution_transition_decision_table_fails_closed() {
        // Test design — Causes: every execution-state pair and the Idle/Running
        // activity-admission partition are evaluated. Effects: permitted pairs
        // mutate atomically and terminal entry clears activities; forbidden pairs
        // preserve the aggregate. Constraints/invariants: Terminated is absorbing,
        // activity begins only from Idle/Running, and no rejected edge mutates.
        // Decision rule X1: the explicit closed transition relation below is true
        // iff `can_transition_to` and `transition_execution` admit the same edge.
        use SessionExecutionState as State;

        let states = [
            State::Preparing,
            State::Activating,
            State::ActivationFailed,
            State::Running,
            State::Rescheduling,
            State::Idle,
            State::Terminated,
        ];
        for from in states {
            // Activity-admission decision table: Idle opens the first interval;
            // Running joins/replaces a live or crash-orphaned epoch; every
            // realization/reschedule/terminal state rejects before Runtime.
            // FMECA: classifying only Idle as ready strands a crash-orphaned
            // Running activity, while admitting any other state bypasses
            // realization or terminal fencing.
            assert_eq!(
                from.admits_activity(),
                matches!(from, State::Idle | State::Running),
                "activity admission from {from}"
            );
            for to in states {
                let expected = from == to
                    || (!from.is_terminal()
                        && match to {
                            State::Terminated => true,
                            State::ActivationFailed => from != State::Idle,
                            State::Activating => matches!(
                                from,
                                State::Preparing | State::Running | State::Rescheduling
                            ),
                            State::Idle => matches!(
                                from,
                                State::Preparing
                                    | State::Activating
                                    | State::Running
                                    | State::Rescheduling
                            ),
                            State::Running => from == State::Idle,
                            State::Rescheduling => matches!(from, State::Idle | State::Running),
                            State::Preparing => false,
                        });
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
            }
        }

        let mut value = session("session-1", SessionRevision(1));
        assert_eq!(value.transition_execution(State::Idle), Ok(false));
        assert_eq!(value.transition_execution(State::Running), Ok(true));
        assert_eq!(value.execution, State::Running);
        assert_eq!(value.begin_activity_epoch(), Some(1));
        assert_eq!(value.active_activity_epochs, BTreeSet::from([1]));
        assert_eq!(value.transition_execution(State::Terminated), Ok(true));
        assert!(
            value.active_activity_epochs.is_empty(),
            "terminal transition clears every active activity"
        );
        let terminal = value.clone();
        assert_eq!(
            value.transition_execution(State::Idle),
            Err(SessionExecutionTransitionError {
                from: State::Terminated,
                to: State::Idle,
            })
        );
        assert_eq!(value, terminal, "rejected transition must be atomic");
    }

    #[test]
    fn disposition_is_orthogonal_to_execution_and_delete_is_idempotent() {
        let mut archived = session("archived", SessionRevision(1));
        assert_eq!(archived.archive("2026-08-08T00:00:00Z"), Ok(true));
        assert_eq!(archived.execution, SessionExecutionState::Terminated);
        assert_eq!(archived.archived_at(), Some("2026-08-08T00:00:00Z"));
        assert!(archived.request_delete());
        assert!(archived.is_hidden());
        assert!(!archived.request_delete(), "delete replay is idempotent");

        let mut failed = session("failed", SessionRevision(1));
        failed.execution = SessionExecutionState::ActivationFailed;
        assert!(failed.is_terminal());
        assert!(
            failed.is_publicly_readable(),
            "failed async creation remains exactly queryable"
        );
        assert!(failed.request_delete(), "failed Sessions remain deletable");
        assert_eq!(failed.execution, SessionExecutionState::ActivationFailed);
        assert!(matches!(failed.disposition, SessionDisposition::Deleting));
    }

    /// Cause graph: lifecycle fact -> terminal classification -> realization and
    /// Resource-cleanup eligibility. `pending` means the Resource aggregate owns
    /// unfinished work; `active` means it has a resident manifest.
    ///
    /// | Rule | status | pending | active | Terminal | Resource reconcile |
    /// |---|---|---|---|---|---|
    /// | L1 | preparing | false | true | false | false |
    /// | L2 | running | false | true | false | false |
    /// | L3 | rescheduling | false | true | false | false |
    /// | L4 | idle | false | true | false | false |
    /// | L5 | idle | true | any | false | true |
    /// | L6 | terminated | false | true | true | true |
    /// | L7 | terminated | false | false | true | false |
    /// | L8 | deleted | false | false | true | true |
    /// | L9 | activation_failed | false | true | true | true |
    #[test]
    fn terminal_state_classification_follows_the_decision_table() {
        for (rule, status, disposition, pending, active, terminal, resource_reconcile) in [
            (
                "L1",
                "preparing",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L2",
                "running",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L3",
                "rescheduling",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L4",
                "idle",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L5",
                "idle",
                SessionDisposition::Active,
                true,
                false,
                false,
                true,
            ),
            (
                "L6",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                true,
                true,
                true,
            ),
            (
                "L7",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                false,
                true,
                false,
            ),
            (
                "L8",
                "terminated",
                SessionDisposition::Deleting,
                false,
                false,
                true,
                true,
            ),
            (
                "L9",
                "activation_failed",
                SessionDisposition::Active,
                false,
                true,
                true,
                true,
            ),
        ] {
            let mut value = session("session-1", SessionRevision(1));
            value.execution = status.parse().expect("fixture execution state");
            value.disposition = disposition;
            let desired = crate::ResolvedSessionResources::try_new(
                vec![crate::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::from("input-1"),
                    mount_path: "input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    source: crate::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-1"),
                    },
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap();
            if active {
                value
                    .resources
                    .prepare("session-1", desired.clone())
                    .expect("active generation prepare");
                value.resources.start_attempt().expect("active attempt");
                value.resources.commit().expect("active commit");
            }
            if pending {
                value
                    .resources
                    .prepare("session-1", desired)
                    .expect("L5 pending generation");
            }
            assert_eq!(value.is_terminal(), terminal, "{rule}");
            assert_eq!(
                value.needs_resource_reconciliation(),
                resource_reconcile,
                "{rule}"
            );
        }
    }

    /// Cause-effect graph:
    ///
    /// C1 key present -> C2 hash present -> C3 Session id present
    /// -> C4 next revision exists -> C5 payload revision is exact
    /// -> C6 every lifecycle fact targets the same Session -> E1 next revision.
    /// Every failed cause yields its stable E2 validation error and no write.
    ///
    /// Decision table (`-` means evaluation already terminated):
    ///
    /// | Rule | Kind | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | R1 | replace | T | T | T | T | T | T | revision 8 |
    /// | R2 | delete | T | T | T | T | T | T | revision 8 |
    /// | R3 | either | F | - | - | - | - | - | empty key |
    /// | R4 | either | T | F | - | - | - | - | empty hash |
    /// | R5 | either | T | T | F | - | - | - | empty id |
    /// | R6 | either | T | T | T | F | - | - | exhausted |
    /// | R7 | replace | T | T | T | T | F | - | replace mismatch |
    /// | R8 | delete | T | T | T | T | F | - | tombstone mismatch |
    /// | R9 | either | T | T | T | T | T | F | lifecycle mismatch |
    ///
    /// Constraint/invariant: validation is ordered and side-effect free; the
    /// first failed prerequisite returns its stable error and never advances a
    /// Session revision or partially accepts lifecycle facts.
    #[test]
    fn mutation_validation_tests_are_generated_from_the_decision_table() {
        let rules = [
            Rule {
                id: "R1",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R2",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R3",
                payload: PayloadKind::Replace,
                key_nonempty: false,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyIdempotencyKey),
            },
            Rule {
                id: "R4",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: false,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyPayloadHash),
            },
            Rule {
                id: "R5",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: false,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptySessionId),
            },
            Rule {
                id: "R6",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: false,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::RevisionExhausted),
            },
            Rule {
                id: "R7",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::ReplacementRevisionMismatch),
            },
            Rule {
                id: "R8",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::TombstoneRevisionMismatch),
            },
            Rule {
                id: "R9",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: false,
                expected: Err(SessionMutationValidationError::LifecycleSessionMismatch),
            },
        ];

        for rule in rules {
            let expected_revision = if rule.revision_available {
                SessionRevision(7)
            } else {
                SessionRevision(u64::MAX)
            };
            let session_id = if rule.session_id_nonempty {
                "session-1"
            } else {
                ""
            };
            let next = expected_revision.0.checked_add(1).unwrap_or_default();
            let payload = match rule.payload {
                PayloadKind::Replace => SessionMutationPayload::Replace(session(
                    session_id,
                    if rule.payload_revision_exact {
                        expected_revision
                    } else {
                        SessionRevision(expected_revision.0.saturating_sub(1))
                    },
                )),
                PayloadKind::Delete => SessionMutationPayload::Delete(SessionTombstone {
                    session_id: session_id.into(),
                    deleted_revision: if rule.payload_revision_exact {
                        SessionRevision(next)
                    } else {
                        expected_revision
                    },
                    deleted_at: "2026-07-25T00:00:00Z".into(),
                }),
            };
            let mutation = SessionMutation {
                expected_revision,
                idempotency: IdempotencyRecord {
                    key: if rule.key_nonempty { "request-1" } else { "" }.into(),
                    payload_hash: if rule.hash_nonempty {
                        "sha256:payload"
                    } else {
                        ""
                    }
                    .into(),
                },
                payload,
                lifecycle_facts: vec![ManagedLifecycleFact {
                    id: "fact-1".into(),
                    object_id: if rule.lifecycle_session_exact {
                        session_id
                    } else {
                        "another-session"
                    }
                    .into(),
                    workspace_id: Some("workspace".into()),
                    event_type: "session.updated".into(),
                    timestamp: 1,
                    runtime_interval: None,
                }],
            };
            assert_eq!(
                mutation.validate(),
                rule.expected,
                "decision rule {}",
                rule.id
            );
        }
    }

    #[test]
    fn runtime_interval_mutation_decision_table_is_fail_closed() {
        // Cause/effect graph: C1=open interval with Running/non-Running state;
        // C2=closed payload absent/present; C3=event kind and stable id exact;
        // C4=end precedes start. Effects: E1 admits the exact aggregate/fact;
        // E2 rejects malformed or contradictory durable truth. Decision rules:
        // R1 Running+C1 => E1; R2 non-Running+C1 => E2; R3 exact C2+C3+!C4
        // => E1; R4-R6 missing/wrong-id/reversed payload => E2; C5 active
        // epochs are positive, no newer than the monotonic fence, and absent
        // from Idle or terminal state. R7 valid C5 => E1; R8/R9 future/Idle
        // active epochs => E2. This is the root-store boundary, so no adapter
        // can persist a billable parallel truth.
        // Constraints/invariants: open-interval state, active epochs, and the
        // matching closure fact form one atomic aggregate and one billing truth.
        let expected_revision = SessionRevision(7);
        let valid_interval = crate::SessionRuntimeInterval {
            interval_id: "interval-1".into(),
            activity_epoch: 3,
            started_at_unix_ms: 100,
            ended_at_unix_ms: 200,
            opened_revision: SessionRevision(7),
            closed_revision: SessionRevision(8),
            observations: Vec::new(),
            usage: Default::default(),
            max_list_cost_minor: None,
        };
        let mutation = |session: PersistedSession, fact: ManagedLifecycleFact| SessionMutation {
            expected_revision,
            idempotency: IdempotencyRecord {
                key: format!("interval:{}", fact.id),
                payload_hash: "payload".into(),
            },
            payload: SessionMutationPayload::Replace(session),
            lifecycle_facts: vec![fact],
        };
        let fact = |interval: Option<crate::SessionRuntimeInterval>| ManagedLifecycleFact {
            id: "interval-1".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.runtime_interval_closed".into(),
            timestamp: 1,
            runtime_interval: interval,
        };

        let mut running = session("session-1", expected_revision);
        running.execution = SessionExecutionState::Running;
        running.activity_epoch = 3;
        running.active_activity_epochs.insert(3);
        assert!(running.begin_runtime_interval(100), "R1 setup");
        let ordinary_fact = ManagedLifecycleFact {
            id: "ordinary".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.updated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        assert_eq!(
            mutation(running.clone(), ordinary_fact.clone()).validate(),
            Ok(SessionRevision(8)),
            "R1"
        );

        let mut idle_with_interval = running.clone();
        idle_with_interval.execution = SessionExecutionState::Idle;
        assert_eq!(
            mutation(idle_with_interval, fact(Some(valid_interval.clone()))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalStateMismatch),
            "R2"
        );
        let mut closed = running;
        closed.running_interval = None;
        closed.execution = SessionExecutionState::Idle;
        closed.active_activity_epochs.clear();
        assert_eq!(
            mutation(closed.clone(), fact(Some(valid_interval.clone()))).validate(),
            Ok(SessionRevision(8)),
            "R3"
        );
        let mut future_active = closed.clone();
        future_active.execution = SessionExecutionState::Running;
        future_active.active_activity_epochs.insert(4);
        assert_eq!(
            mutation(future_active, ordinary_fact.clone()).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R8"
        );
        let mut idle_active = closed.clone();
        idle_active.active_activity_epochs.insert(3);
        assert_eq!(
            mutation(idle_active, ordinary_fact).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R9"
        );
        assert_eq!(
            mutation(closed.clone(), fact(None)).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R4"
        );
        let mut wrong_id = valid_interval.clone();
        wrong_id.interval_id = "other".into();
        assert_eq!(
            mutation(closed.clone(), fact(Some(wrong_id))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R5"
        );
        let mut reversed = valid_interval;
        reversed.ended_at_unix_ms = 99;
        assert_eq!(
            mutation(closed, fact(Some(reversed))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R6"
        );
    }

    #[test]
    fn effective_runtime_usage_counts_open_interval_once() {
        // Active-time cause/effect table. C1 interval is closed/open; C2 now is
        // before/equal/after start; C3 the open interval is subsequently closed
        // at the same instant. E1 closed total only; E2 clamp clock rollback;
        // E3 add open elapsed once; E4 closing preserves the same effective
        // total (no double count). Rules: T1 closed=>E1; T2 open+before=>E2;
        // T3 open+after=>E3; T4 T3 then close=>E4. Repeating a rule is exact and
        // budget reconciliation separately keeps its cumulative cursor monotonic.
        // Constraints/invariants: elapsed time is nonnegative, monotonic, and an
        // open interval is included at most once before or after closure.
        let mut value = session("active-time", SessionRevision(1));
        value.runtime_active_millis = 2_000;
        assert_eq!(
            value.effective_runtime_active_millis(50_000),
            2_000,
            "T1/E1"
        );
        value.execution = SessionExecutionState::Running;
        value.activity_epoch = 1;
        value.active_activity_epochs.insert(1);
        assert!(value.begin_runtime_interval(10_000), "T2 setup");
        assert_eq!(value.effective_runtime_active_millis(9_000), 2_000, "T2/E2");
        assert_eq!(
            value.effective_runtime_active_millis(13_500),
            5_500,
            "T3/E3"
        );
        value.close_runtime_interval(13_500).expect("T4 close");
        assert_eq!(
            value.effective_runtime_active_millis(99_000),
            5_500,
            "T4/E4"
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        SessionDeleteDispositionClass, SessionExecutionState, session_delete_request_plan,
        session_tombstone_is_admitted,
    };

    #[kani::proof]
    fn terminal_execution_never_reopens() {
        let terminal = if kani::any::<bool>() {
            SessionExecutionState::ActivationFailed
        } else {
            SessionExecutionState::Terminated
        };
        let next = match kani::any::<u8>() % 7 {
            0 => SessionExecutionState::Preparing,
            1 => SessionExecutionState::Activating,
            2 => SessionExecutionState::ActivationFailed,
            3 => SessionExecutionState::Running,
            4 => SessionExecutionState::Rescheduling,
            5 => SessionExecutionState::Idle,
            _ => SessionExecutionState::Terminated,
        };
        if terminal.can_transition_to(next) {
            assert_eq!(terminal, next);
        }
    }

    #[kani::proof]
    fn session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup() {
        let disposition_hidden = kani::any::<bool>();
        let execution_terminal = kani::any::<bool>();
        let cleanup_completed = kani::any::<bool>();
        let event_batches_complete = kani::any::<bool>();
        let session_identity_exact = kani::any::<bool>();
        let next_revision_exact = kani::any::<bool>();
        assert_eq!(
            session_tombstone_is_admitted(
                disposition_hidden,
                execution_terminal,
                cleanup_completed,
                event_batches_complete,
                session_identity_exact,
                next_revision_exact,
            ),
            disposition_hidden
                && execution_terminal
                && cleanup_completed
                && event_batches_complete
                && session_identity_exact
                && next_revision_exact
        );
    }

    #[kani::proof]
    fn session_delete_request_plan_is_exact_hidden_terminal_and_idempotent() {
        let disposition_code = kani::any::<u8>();
        kani::assume(disposition_code < 4);
        let disposition = match disposition_code {
            0 => SessionDeleteDispositionClass::Active,
            1 => SessionDeleteDispositionClass::Archived,
            2 => SessionDeleteDispositionClass::Deleting,
            3 => SessionDeleteDispositionClass::Deleted,
            _ => unreachable!(),
        };
        let execution_terminal = kani::any::<bool>();
        let plan = session_delete_request_plan(disposition, execution_terminal);
        let first_request = disposition_code < 2;
        assert_eq!(plan.transition_to_deleting, first_request);
        assert_eq!(plan.request_cleanup, first_request);
        assert_eq!(
            plan.terminalize_execution,
            first_request && !execution_terminal
        );
        if disposition_code >= 2 {
            assert!(!plan.transition_to_deleting);
            assert!(!plan.request_cleanup);
            assert!(!plan.terminalize_execution);
        }
    }
}



/// Closed reducer plan for the durable Delete-intent transaction. Deleting and
/// Deleted are absorbing replays; every admitted request hides the aggregate,
/// fences nonterminal execution, and requests recoverable cleanup together.
#[must_use]
const fn session_delete_request_plan(
    disposition: SessionDeleteDispositionClass,
    execution_terminal: bool,
) -> SessionDeleteRequestPlan {
    let transition_to_deleting = matches!(
        disposition,
        SessionDeleteDispositionClass::Active | SessionDeleteDispositionClass::Archived
    );
    SessionDeleteRequestPlan {
        transition_to_deleting,
        terminalize_execution: transition_to_deleting && !execution_terminal,
        request_cleanup: transition_to_deleting,
    }
}
