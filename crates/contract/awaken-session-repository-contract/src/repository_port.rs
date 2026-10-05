// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal entrypoint webhook.committed_outbox_row_is_recoverable
// @formal entrypoint webhook.delivery_requires_an_enqueued_fact
// @formal entrypoint webhook.lifecycle_commit_and_outbox_are_atomic
use super::*;
use crate::{ManagedLifecycleFact, SessionId};
use async_trait::async_trait;
use std::collections::HashSet;

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

/// Read one operational capacity projection from the canonical Session roots.
/// Multiple unfinished Sessions bound to the same logical Worker need one
/// retained Pod; any unbound, never-materialized terminal work needs one
/// additional first-placement Pod. This read grants no claim or recovery.
pub async fn terminal_cleanup_worker_owners(
    repository: &dyn SessionRepository,
) -> Result<u64, SessionRepositoryError> {
    let mut cursor: Option<SessionScanCursor> = None;
    let mut owners = HashSet::new();
    let mut unbound = false;
    loop {
        let scan = repository
            .scan(SessionScanQuery::Reconcilable {
                after: cursor.as_ref(),
            })
            .await?;
        if !scan.quarantined.is_empty() {
            return Err(SessionRepositoryError::Corrupt(
                "Session reconciliation scan contains quarantined roots".into(),
            ));
        }
        for scoped in scan.sessions {
            let session = scoped.session;
            if !session
                .needs_terminal_cleanup_worker()
                .map_err(|error| SessionRepositoryError::Corrupt(error.to_string()))?
            {
                continue;
            }
            if let Some(lease) = session.realization.as_ref() {
                if lease.owner.trim().is_empty() {
                    return Err(SessionRepositoryError::Corrupt(
                        "terminal Session has an empty Worker owner".into(),
                    ));
                }
                owners.insert(lease.owner.clone());
            } else if matches!(session.environment, SessionEnvironmentState::Unmaterialized) {
                unbound = true;
            } else {
                return Err(SessionRepositoryError::Corrupt(
                    "terminal Session has physical state without a Worker owner".into(),
                ));
            }
        }
        let Some(next) = scan.next_cursor else {
            let count = u64::try_from(owners.len()).map_err(|_| {
                SessionRepositoryError::AuthorityUnavailable(
                    "terminal Worker owner count overflow".into(),
                )
            })?;
            return count.checked_add(u64::from(unbound)).ok_or_else(|| {
                SessionRepositoryError::AuthorityUnavailable(
                    "terminal Worker owner count overflow".into(),
                )
            });
        };
        if cursor
            .as_ref()
            .is_some_and(|previous| next.session_id() <= previous.session_id())
        {
            return Err(SessionRepositoryError::Corrupt(
                "Session reconciliation cursor did not advance".into(),
            ));
        }
        cursor = Some(next);
    }
}

// Durable adapters (SQLite and Postgres) live outward in `awaken-session-store`;
// test doubles implement this port explicitly. Workspace ownership is persisted
// atomically beside each row through `create`; authorization scope decorators do
// not belong in this resource persistence port.
