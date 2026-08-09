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

use async_trait::async_trait;
use std::collections::BTreeMap;

use crate::ManagedLifecycleFact;

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

/// The one durable logical state of a Managed Session aggregate.
///
/// Runtime handles, leases, protocol DTOs, and notification delivery are
/// projections or effects of this value; none is a second lifecycle authority.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleState {
    Preparing,
    Activating,
    ActivationFailed,
    Running,
    Rescheduling,
    #[default]
    Idle,
    Terminated,
    Deleted,
}

impl SessionLifecycleState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Activating => "activating",
            Self::ActivationFailed => "activation_failed",
            Self::Running => "running",
            Self::Rescheduling => "rescheduling",
            Self::Idle => "idle",
            Self::Terminated => "terminated",
            Self::Deleted => "deleted",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ActivationFailed | Self::Terminated | Self::Deleted
        )
    }

    /// Whether the canonical Session state machine admits `next`.
    ///
    /// Replays are deliberately idempotent. Terminal states fail closed, and
    /// every non-terminal transition used by fresh execution, resume, and
    /// recovery is defined here rather than in those callers.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        match next {
            Self::Terminated | Self::Deleted => true,
            Self::ActivationFailed => !matches!(self, Self::Idle),
            Self::Activating => {
                matches!(self, Self::Preparing | Self::Running | Self::Rescheduling)
            }
            Self::Idle => matches!(
                self,
                Self::Preparing | Self::Activating | Self::Running | Self::Rescheduling
            ),
            Self::Running => matches!(self, Self::Idle),
            Self::Rescheduling => matches!(self, Self::Idle | Self::Running),
            Self::Preparing => false,
        }
    }
}

impl std::fmt::Display for SessionLifecycleState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown Session lifecycle state `{0}`")]
pub struct SessionLifecycleStateError(pub String);

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid Session lifecycle transition from `{from}` to `{to}`")]
pub struct SessionLifecycleTransitionError {
    pub from: SessionLifecycleState,
    pub to: SessionLifecycleState,
}

impl std::str::FromStr for SessionLifecycleState {
    type Err = SessionLifecycleStateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "activating" => Ok(Self::Activating),
            "activation_failed" => Ok(Self::ActivationFailed),
            "running" => Ok(Self::Running),
            "rescheduling" => Ok(Self::Rescheduling),
            "idle" => Ok(Self::Idle),
            "terminated" => Ok(Self::Terminated),
            "deleted" => Ok(Self::Deleted),
            other => Err(SessionLifecycleStateError(other.to_string())),
        }
    }
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
pub struct PersistedSession {
    pub session_id: String,
    /// The one optimistic-concurrency fence for baseline, Resource, MCP,
    /// environment and lifecycle mutations. New, not-yet-inserted values use 0.
    #[serde(default)]
    pub revision: SessionRevision,
    /// The only immutable configuration authority. A preparation intent is
    /// consumed exactly once and replaced by its frozen baseline.
    pub baseline: crate::SessionBaselineState,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Exact durable neutral mutable tool policy. Empty is an intentional clear;
    /// public protocol tool unions are projections and never persistence truth.
    #[serde(default)]
    pub tools: crate::SessionToolConfiguration,
    /// Monotonic root-CAS fence for overlapping driving events. Lifecycle status
    /// remains the one durable logical state; this scalar only prevents a stale
    /// completion from settling a newer turn.
    #[serde(default)]
    pub activity_epoch: u64,
    /// Durable, secret-free execution-environment phase. Opaque bindings are
    /// interpreted only by the runtime that produced them; this aggregate owns
    /// their transition, not their substrate meaning.
    #[serde(default)]
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
    /// The only durable logical lifecycle authority. The retained serialized key
    /// keeps historical aggregate JSON and database fixtures readable.
    #[serde(rename = "status", alias = "lifecycle")]
    pub lifecycle: SessionLifecycleState,
    pub archived_at: Option<String>,
}

impl PersistedSession {
    /// Apply the sole durable Session lifecycle transition function.
    ///
    /// Returns `false` for an idempotent replay and leaves the aggregate
    /// untouched when the transition is invalid.
    pub fn transition_lifecycle(
        &mut self,
        next: SessionLifecycleState,
    ) -> Result<bool, SessionLifecycleTransitionError> {
        let from = self.lifecycle;
        if !from.can_transition_to(next) {
            return Err(SessionLifecycleTransitionError { from, to: next });
        }
        if from == next {
            return Ok(false);
        }
        self.lifecycle = next;
        Ok(true)
    }

    /// Whether the root Session lifecycle forbids every new realization effect.
    /// Keep this classification on the aggregate so API rehydration, MCP recovery,
    /// and later reconcilers cannot grow different terminal-status lists.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.lifecycle.is_terminal()
    }

    /// Whether the Resource convergence driver owns work for this Session.
    ///
    /// A running or rescheduling Session with an active manifest is deliberately
    /// excluded: its resident Environment is live execution state, not terminal
    /// cleanup work. Keeping this predicate beside [`Self::is_terminal`] prevents
    /// reconcilers from recreating lifecycle status lists with string comparisons.
    #[must_use]
    pub fn needs_resource_reconciliation(&self) -> bool {
        self.lifecycle == SessionLifecycleState::Deleted
            || self.resources.needs_reconciliation()
            || (self.is_terminal() && self.resources.has_active())
    }

    /// Whether this durable aggregate must be revisited by any Coordinator
    /// convergence driver. Keeping the union here prevents SQLite, Postgres,
    /// and future repositories from growing different recovery scans.
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.needs_resource_reconciliation()
            || self.resources.has_references()
            || self.mcp.needs_reconciliation()
            || !matches!(
                self.environment,
                crate::SessionEnvironmentState::Unmaterialized
            )
            || self.needs_work_dispatch()
    }

    /// Whether the externally executed Session must have a WorkQueue
    /// projection. Application-owned Sessions cross a distinct claim boundary
    /// and are intentionally excluded.
    #[must_use]
    pub fn needs_work_dispatch(&self) -> bool {
        !self.is_terminal()
            && self.frozen_baseline().is_some_and(|baseline| {
                baseline.environment.self_hosted && !self.has_application_contribution()
            })
    }

    /// Whether the frozen Session consumed an external application contribution.
    /// This immutable aggregate fact, rather than a transient realization lease,
    /// distinguishes the claimed-application realization path after restart.
    #[must_use]
    pub fn has_application_contribution(&self) -> bool {
        self.frozen_baseline()
            .is_some_and(|baseline| baseline.application.is_some())
    }

    #[must_use]
    pub fn frozen_baseline(&self) -> Option<&crate::SessionBaseline> {
        match &self.baseline {
            crate::SessionBaselineState::Frozen(baseline) => Some(baseline),
            crate::SessionBaselineState::Preparing(_) => None,
        }
    }

    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        self.frozen_baseline()
            .map(|baseline| baseline.agent_id.as_str())
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.frozen_baseline()
            .map(|baseline| baseline.model.as_str())
    }

    #[must_use]
    pub fn environment_id(&self) -> &str {
        match &self.baseline {
            crate::SessionBaselineState::Preparing(intent) => {
                &intent.control.environment.environment_id
            }
            crate::SessionBaselineState::Frozen(baseline) => &baseline.environment.environment_id,
        }
    }

    /// Managed wire projection derived from durably active generations only.
    #[must_use]
    pub fn visible_mcp_servers(&self) -> Vec<VisibleMcpServer> {
        self.mcp
            .visible()
            .into_iter()
            .map(|attachment| VisibleMcpServer {
                name: attachment.name.clone(),
                target: attachment.target.clone(),
                prompts_as_skills: attachment.prompts_as_skills,
            })
            .collect()
    }
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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "the public root-mutation contract intentionally carries a complete replacement aggregate"
)]
pub enum SessionMutationPayload {
    Replace(PersistedSession),
    Delete(SessionTombstone),
}

impl SessionMutationPayload {
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Replace(session) => &session.session_id,
            Self::Delete(tombstone) => &tombstone.session_id,
        }
    }

    #[must_use]
    pub fn stable_hash(&self) -> String {
        crate::stable_fingerprint(self)
    }
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
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryError {
    #[error("Session repository rejected invalid mutation: {0}")]
    InvalidMutation(String),
    #[error("Session already exists")]
    AlreadyExists,
    #[error("Session was deleted")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error("Session repository storage failed: {0}")]
    Storage(String),
}

impl SessionMutation {
    /// Validate all command-local causes before a repository reads or writes.
    /// A successful return is the only root revision the transaction may commit.
    pub fn validate(&self) -> Result<SessionRevision, SessionMutationValidationError> {
        if self.idempotency.key.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyIdempotencyKey);
        }
        if self.idempotency.payload_hash.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyPayloadHash);
        }
        let session_id = self.payload.session_id();
        if session_id.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptySessionId);
        }
        let next = SessionRevision(
            self.expected_revision
                .0
                .checked_add(1)
                .ok_or(SessionMutationValidationError::RevisionExhausted)?,
        );
        match &self.payload {
            SessionMutationPayload::Replace(session)
                if session.revision != self.expected_revision =>
            {
                return Err(SessionMutationValidationError::ReplacementRevisionMismatch);
            }
            SessionMutationPayload::Delete(tombstone) if tombstone.deleted_revision != next => {
                return Err(SessionMutationValidationError::TombstoneRevisionMismatch);
            }
            SessionMutationPayload::Replace(_) | SessionMutationPayload::Delete(_) => {}
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.object_id != session_id)
        {
            return Err(SessionMutationValidationError::LifecycleSessionMismatch);
        }
        Ok(next)
    }
}

/// The port the Managed adapter drives to persist and restore [`PersistedSession`]
/// rows. The default in-memory impl keeps single-process behavior; a durable impl
/// (e.g. SQLite alongside the transcript store) lets a session survive a restart
/// and be reported faithfully by another process.
#[async_trait]
pub trait ManagedSessionRepository: Send + Sync {
    /// Insert one new aggregate together with owner, idempotency and outbox.
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionRevision, SessionRepositoryError>;

    /// Commit the one root-revision CAS transaction.
    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError>;

    /// Commit a lifecycle transition fact idempotently by stable id.
    async fn append_lifecycle(&self, fact: ManagedLifecycleFact);

    async fn pending_lifecycle(&self) -> Vec<ManagedLifecycleFact>;

    async fn try_pending_lifecycle(
        &self,
    ) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        Ok(self.pending_lifecycle().await)
    }

    async fn complete_lifecycle(&self, fact_id: &str);

    /// The stored configuration for `session_id`, if any.
    async fn get(&self, session_id: &str) -> Option<PersistedSession>;

    /// Fallible read for callers that must distinguish absence from unavailable
    /// or corrupt durable state. Durable adapters override this; the default
    /// preserves compatibility for simple in-memory/test implementations.
    async fn try_get(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        Ok(self.get(session_id).await)
    }

    /// Sessions carrying any durable Resource, MCP, environment, or WorkQueue
    /// projection reconciliation work.
    /// Implementations preserve the intrinsic Workspace partition in the same
    /// row scan; application coordinators filter by their owned state machine.
    /// One index avoids parallel per-feature recovery registries and scans.
    async fn reconcilable_sessions(&self) -> Vec<ScopedPersistedSession>;

    async fn try_reconcilable_sessions(
        &self,
    ) -> Result<Vec<ScopedPersistedSession>, SessionRepositoryError> {
        Ok(self.reconcilable_sessions().await)
    }

    /// Durable application-command receipt. This is a read of the same
    /// idempotency table written atomically by `create`/`commit_mutation`, not a
    /// second command registry.
    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Option<SessionIdempotencyReceipt>;

    /// The atomically persisted owner scope of `session_id`, if the row exists.
    async fn owner(&self, session_id: &str) -> Option<String>;
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

    fn session(id: &str, revision: SessionRevision) -> PersistedSession {
        use awaken_credential_contract::{
            CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
        };

        PersistedSession {
            session_id: id.into(),
            revision,
            baseline: crate::SessionBaselineState::Preparing(crate::SessionCreationIntent {
                control: crate::ControlSessionCreationInputs {
                    environment: crate::EnvironmentSnapshot {
                        environment_id: "environment".into(),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: crate::EnvironmentFingerprint("config".into()),
                        sandbox: serde_json::json!({}),
                        sandbox_provisioning: Default::default(),
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
                    model: "model".into(),
                    execution_model_ref: "model".into(),
                    runtime: None,
                    mcp_authoring: Default::default(),
                    toolsets: Vec::new(),
                    delegate_ids: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
                application: crate::ApplicationContributionState::Absent,
            }),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            lifecycle: SessionLifecycleState::Idle,
            archived_at: None,
        }
    }

    #[test]
    fn lifecycle_state_preserves_the_historical_key_and_rejects_unknown_truth() {
        let value = serde_json::to_value(session("session-1", SessionRevision(1))).unwrap();
        assert_eq!(value.get("status"), Some(&serde_json::json!("idle")));
        assert!(value.get("lifecycle").is_none());

        let mut unknown = value.clone();
        unknown["status"] = serde_json::json!("legacy-unknown");
        assert!(serde_json::from_value::<PersistedSession>(unknown).is_err());

        let mut aliased = value;
        let status = aliased.as_object_mut().unwrap().remove("status").unwrap();
        aliased["lifecycle"] = status;
        assert_eq!(
            serde_json::from_value::<PersistedSession>(aliased)
                .unwrap()
                .lifecycle,
            SessionLifecycleState::Idle
        );
    }

    #[test]
    fn lifecycle_transition_decision_table_fails_closed() {
        use SessionLifecycleState as State;

        let states = [
            State::Preparing,
            State::Activating,
            State::ActivationFailed,
            State::Running,
            State::Rescheduling,
            State::Idle,
            State::Terminated,
            State::Deleted,
        ];
        for from in states {
            for to in states {
                let expected = from == to
                    || (!from.is_terminal()
                        && match to {
                            State::Terminated | State::Deleted => true,
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
        assert_eq!(value.transition_lifecycle(State::Idle), Ok(false));
        assert_eq!(value.transition_lifecycle(State::Running), Ok(true));
        assert_eq!(value.lifecycle, State::Running);
        assert_eq!(value.transition_lifecycle(State::Terminated), Ok(true));
        let terminal = value.clone();
        assert_eq!(
            value.transition_lifecycle(State::Idle),
            Err(SessionLifecycleTransitionError {
                from: State::Terminated,
                to: State::Idle,
            })
        );
        assert_eq!(value, terminal, "rejected transition must be atomic");
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
    fn terminal_lifecycle_classification_follows_the_decision_table() {
        for (rule, status, pending, active, terminal, resource_reconcile) in [
            ("L1", "preparing", false, true, false, false),
            ("L2", "running", false, true, false, false),
            ("L3", "rescheduling", false, true, false, false),
            ("L4", "idle", false, true, false, false),
            ("L5", "idle", true, false, false, true),
            ("L6", "terminated", false, true, true, true),
            ("L7", "terminated", false, false, true, false),
            ("L8", "deleted", false, false, true, true),
            ("L9", "activation_failed", false, true, true, true),
        ] {
            let mut value = session("session-1", SessionRevision(1));
            value.lifecycle = status.parse().expect("fixture lifecycle state");
            let desired = crate::ResolvedSessionResources {
                inputs: vec![crate::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::from("input-1"),
                    mount_path: "input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    source: crate::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-1"),
                    },
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            };
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
}
