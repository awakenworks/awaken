use super::*;


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

/// The port the Managed adapter drives to persist and restore [`PersistedSession`]
/// rows. The default in-memory impl keeps single-process behavior; a durable impl
/// (e.g. SQLite alongside the transcript store) lets a session survive a restart
/// and be reported faithfully by another process.
#[async_trait]
pub trait ManagedSessionRepository: Send + Sync {
    /// Insert one new aggregate together with owner, idempotency and outbox, or
    /// atomically return the exact durable aggregate for an owner-bound replay.
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
    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError>;

    /// Commit the one root-revision CAS transaction.
    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError>;

    /// Commit a lifecycle transition fact idempotently by stable id.
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError>;

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError>;

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError>;

    /// The stored configuration for `session_id`. Absence is the typed
    /// [`SessionRepositoryError::NotFound`] case, never a storage fallback.
    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError>;

    /// Every visible aggregate owned by one Workspace. Collection reads must
    /// come from durable truth so a process restart cannot make existing
    /// Sessions disappear merely because the protocol projection cache is cold.
    async fn list_by_owner(
        &self,
        _owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support Workspace listing".into(),
        ))
    }

    /// Sessions carrying any durable Resource, MCP, environment, or WorkQueue
    /// projection reconciliation work.
    /// Implementations preserve the intrinsic Workspace partition in the same
    /// row scan; application coordinators filter by their owned state machine.
    /// One index avoids parallel per-feature recovery registries and scans.
    async fn reconcilable_sessions(&self) -> Result<SessionRecoveryScan, SessionRepositoryError>;

    /// Count one typed Environment phase across every canonical live Session
    /// row. Unsupported adapters fail closed; a recovery batch is not global.
    async fn count_environment_phase(
        &self,
        _phase: crate::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support global Environment phase counts".into(),
        ))
    }

    /// Healthy Sessions whose immutable authoring baseline references a Vault.
    /// This is the durable index used by credential rollout controllers; an
    /// in-memory cache or one process's active-runtime list is never complete in
    /// HA. Adapters that cannot provide the scan fail closed and leave the
    /// credential outbox event pending.
    async fn sessions_referencing_vault(
        &self,
        _workspace_id: &str,
        _vault_id: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support Vault rollout indexing".into(),
        ))
    }

    /// Healthy Sessions whose current desired MCP attachments reference one
    /// exact credential source. The Workspace comes from the Session root row;
    /// callers supply a source id only from the credential authority's exact
    /// committed rollout provenance. This index is additive to immutable Vault
    /// authoring references: neither can be inferred from or substituted for the
    /// other.
    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError>;

    /// Durable application-command receipt. This is a read of the same
    /// idempotency table written atomically by `create`/`commit_mutation`, not a
    /// second command registry.
    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError>;

    /// The atomically persisted owner scope of `session_id`.
    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError>;
}
