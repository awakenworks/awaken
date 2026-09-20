use async_trait::async_trait;

use super::{PersistedSession, SessionExecutionState, SessionScan, SessionScanQuery};
use crate::{ManagedLifecycleFact, SessionId};

use crate::SessionRevision;

/// Durable owner fence for all process-local Session projections. Runtime and
/// Worker identities are opaque to the Session domain.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationLease {
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

/// The aggregate's realization lease carries exactly the coordinates of the
/// neutral fence algebra, so it states its succession through the one law
/// instead of restating the field comparisons.
impl awaken_provisioning_contract::FenceToken for SessionRealizationLease {
    fn owner(&self) -> &str {
        &self.owner
    }

    fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

impl SessionRealizationLease {
    /// Project the aggregate-owned realization generation into the one neutral
    /// provider fence. Domain callers still decide whether this lease is the
    /// current/live authority; this method only prevents adapters from growing
    /// duplicate field mappings or operation-identity algorithms.
    pub fn sandbox_effect_fence(
        &self,
        operation_id: impl Into<String>,
    ) -> Result<
        awaken_provisioning_contract::SandboxEffectFence,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_provisioning_contract::SandboxEffectFence::for_operation(operation_id, self)
    }
}

/// Closed admission kernel for physically deleting the authoritative Session
/// row. Visibility, execution fencing, accepted Event provenance, and verified
/// cleanup are independent axes; omitting any one of them fails closed.
#[must_use]
pub const fn session_tombstone_is_admitted(
    disposition_hidden: bool,
    execution_terminal: bool,
    cleanup_completed: bool,
    event_batches_complete: bool,
    session_identity_exact: bool,
    next_revision_exact: bool,
) -> bool {
    disposition_hidden
        && execution_terminal
        && cleanup_completed
        && event_batches_complete
        && session_identity_exact
        && next_revision_exact
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
    pub session_id: SessionId,
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

impl SessionMutationPayload {
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
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
    #[error("Session replacement changed its immutable creation observation")]
    CreationObservationChanged,
    #[error("Session replacement erased or regressed its update observation")]
    UpdateObservationRegressed,
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
        if session_id.as_str().trim().is_empty() {
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
        if let SessionMutationPayload::Replace(session) = &self.payload
            && session.running_interval.is_some()
            && session.execution != SessionExecutionState::Running
        {
            return Err(SessionMutationValidationError::RuntimeIntervalStateMismatch);
        }
        if let SessionMutationPayload::Replace(session) = &self.payload
            && (session
                .active_activity_epochs
                .iter()
                .any(|epoch| *epoch == 0 || *epoch > session.activity_epoch)
                || (!session.active_activity_epochs.is_empty()
                    && (session.execution == SessionExecutionState::Idle || session.is_terminal())))
        {
            return Err(SessionMutationValidationError::ActiveActivityStateMismatch);
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.object_id != session_id)
        {
            return Err(SessionMutationValidationError::LifecycleSessionMismatch);
        }
        if self.lifecycle_facts.iter().any(|fact| {
            match (&fact.runtime_interval, fact.event_type.as_str()) {
                (None, "session.runtime_interval_closed") => true,
                (Some(interval), event_type) => {
                    event_type != "session.runtime_interval_closed"
                        || fact.id != interval.interval_id
                        || interval.ended_at_unix_ms < interval.started_at_unix_ms
                }
                (None, _) => false,
            }
        }) {
            return Err(SessionMutationValidationError::RuntimeIntervalFactMismatch);
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.runtime_interval.is_some())
            && matches!(
                &self.payload,
                SessionMutationPayload::Replace(session)
                    if session.execution == SessionExecutionState::Running
                        || session.running_interval.is_some()
            )
        {
            return Err(SessionMutationValidationError::RuntimeIntervalFactMismatch);
        }
        Ok(next)
    }
}

/// Validate the existing root fields, without inventing a second timestamp
/// record or sampling a clock in the repository. Legacy unknown creation is as
/// immutable as a known creation time; a known update may never be erased.
fn validate_root_observation_replacement(
    previous_created: Option<u64>,
    replacement_created: Option<u64>,
    previous_updated: Option<u64>,
    replacement_updated: Option<u64>,
) -> Result<(), SessionMutationValidationError> {
    if previous_created != replacement_created {
        return Err(SessionMutationValidationError::CreationObservationChanged);
    }
    if let Some(previous) = previous_updated {
        let Some(replacement) = replacement_updated else {
            return Err(SessionMutationValidationError::UpdateObservationRegressed);
        };
        if replacement < previous {
            return Err(SessionMutationValidationError::UpdateObservationRegressed);
        }
    }
    Ok(())
}

impl PersistedSession {
    /// Check a fresh replacement against this current durable root, inside the
    /// existing owner/revision transaction. Call this after receipt replay has
    /// been classified: replay must return durable truth without admitting the
    /// caller's newly observed candidate. This method owns only observation
    /// preservation; root identity/revision and tombstones retain their fences.
    pub fn validate_observation_replacement(
        &self,
        replacement: &Self,
    ) -> Result<(), SessionMutationValidationError> {
        validate_root_observation_replacement(
            self.created_at_unix_ms,
            replacement.created_at_unix_ms,
            self.updated_at_unix_ms,
            replacement.updated_at_unix_ms,
        )
    }
}

#[cfg(any(test, kani))]
mod root_observation_proofs {
    use super::*;

    fn check_case(
        created: Option<u64>,
        next_created: Option<u64>,
        updated: Option<u64>,
        next_updated: Option<u64>,
    ) {
        let result =
            validate_root_observation_replacement(created, next_created, updated, next_updated);
        // Independent oracle uses Option's total order (None < every Some),
        // rather than repeating the production nested known-time branches.
        let expected = created == next_created && next_updated >= updated;
        assert_eq!(result.is_ok(), expected);
        if created != next_created {
            assert_eq!(
                result,
                Err(SessionMutationValidationError::CreationObservationChanged)
            );
        } else if next_updated < updated {
            assert_eq!(
                result,
                Err(SessionMutationValidationError::UpdateObservationRegressed)
            );
        }
    }

    #[cfg(kani)]
    #[kani::proof]
    fn session_root_replacement_preserves_creation_and_monotonic_update_observations() {
        // Causes: C1 old/new creation identity (including None); C2 old update
        // known/unknown; C3 candidate update absent/lower/equal/higher. Effects:
        // R1=!C1=>CreationObservationChanged; R2=C1+C2+(absent/lower)=>
        // UpdateObservationRegressed; R3=otherwise=>admit. R1 has precedence if
        // both fail. Every Option<u64> input is symbolic, with no numeric bound,
        // comparison stub or trusted-caller assumption. SQL locking, codec and
        // placement of this gate remain separately tested adapter boundaries.
        check_case(kani::any(), kani::any(), kani::any(), kani::any());
    }

    #[cfg(test)]
    #[test]
    fn root_observation_preservation_decision_table() {
        // R1-R3 Cartesian native cross-check: unknown, zero, adjacent values
        // and MAX cross every equality/order class and both failure causes.
        let values = [None, Some(0), Some(1), Some(u64::MAX - 1), Some(u64::MAX)];
        for created in values {
            for next_created in values {
                for updated in values {
                    for next_updated in values {
                        check_case(created, next_created, updated, next_updated);
                    }
                }
            }
        }
    }
}

/// Outbound persistence port of the Session aggregate.
///
/// Every method is required and names a real consumer; a backend that cannot
/// answer one of them is not a Session repository. The lifecycle outbox written
/// atomically by [`Self::create`] and [`Self::commit`] is drained through the
/// separate [`LifecycleOutbox`](super::LifecycleOutbox) port, which the same
/// durable store implements.
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Insert one new aggregate together with owner, idempotency and outbox, or
    /// atomically return the exact durable aggregate for an owner-bound replay.
    /// Occupied, tombstoned and mismatched identities are the typed
    /// [`SessionRepositoryConflict`] results.
    ///
    /// Caller: `SessionApplication` Session creation.
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError>;

    /// Atomically classify a deterministic create identity without inserting.
    /// `Ok(None)` means both receipt and identity are absent. An exact receipt
    /// returns the current durable aggregate; occupied, tombstoned, mismatched,
    /// or corrupt identities retain the same typed result as [`Self::create`].
    ///
    /// Caller: `SessionApplication::replay_session_create` and
    /// `replay_accepted_session_create` (Managed create retry before compiling a
    /// new candidate).
    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError>;

    /// Commit the one root-revision CAS transaction: expected revision,
    /// replacement or tombstone, lifecycle facts and idempotency receipt are
    /// written together. A fresh Replace preserves the current root's creation
    /// observation and cannot erase/regress a known update observation; exact
    /// receipt replay (`Replayed`/`IdempotencyMismatch`) is classified before
    /// this gate.
    ///
    /// Caller: `SessionApplication` mutation and Resource reconciliation paths.
    async fn commit(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError>;

    /// The stored configuration for `session_id`. Absence is the typed
    /// [`SessionRepositoryError::NotFound`] case, never a storage fallback.
    ///
    /// Caller: every `SessionApplication` command that reloads aggregate truth.
    async fn get(&self, session_id: &SessionId)
    -> Result<PersistedSession, SessionRepositoryError>;

    /// The atomically persisted owner scope of `session_id`. It is a separate
    /// read because ownership routing must stay answerable for a row whose
    /// aggregate no longer decodes.
    ///
    /// Caller: `SessionApplication` owner-scoped commands, file-content and
    /// Coordinator application-access authorization.
    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionRepositoryError>;

    /// Read durable Sessions selected by one closed query. The per-query
    /// failure policy is part of the contract (see [`SessionScanQuery`]):
    /// the reconciliation index isolates corrupt rows into
    /// [`SessionScan::quarantined`], every other query fails closed with
    /// [`SessionRepositoryError::Corrupt`].
    ///
    /// Caller: `SessionApplication` recovery supervisor and realization control
    /// (`Reconcilable`), Session Resource listing (`Workspace`), Managed
    /// credential rollout (`CredentialSourceReferences`).
    async fn scan(
        &self,
        query: SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError>;

    /// Durable application-command receipt. This is a read of the same
    /// idempotency table written atomically by `create`/`commit`, not a second
    /// command registry.
    ///
    /// Caller: `SessionApplication` activity, update, terminal and Resource
    /// reconciliation commands that replay a completed command.
    async fn idempotency_receipt(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError>;

    /// Read deletion evidence only inside its persisted owner partition. None
    /// means no matching tombstone, not proof of non-admission.
    ///
    /// Caller: `SessionApplication` Event batch admission after Session deletion.
    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<Option<crate::DeletedSessionEventAdmissions>, SessionRepositoryError>;

    /// Count one typed Environment phase across every canonical live Session
    /// row. It is global, unpaged and decodes every row, so it cannot be a
    /// [`Self::scan`] page.
    ///
    /// Caller: `SessionEventBatchCutoverValidationSource`.
    async fn count_environment_phase(
        &self,
        phase: crate::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError>;
}

// Durable adapters (SQLite and Postgres) live outward in `awaken-session-store`;
// test doubles implement this port explicitly. Workspace ownership is persisted
// atomically beside each row through `create`; authorization scope decorators do
// not belong in this resource persistence port.
