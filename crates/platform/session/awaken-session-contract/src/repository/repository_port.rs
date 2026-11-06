// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal entrypoint webhook.committed_outbox_row_is_recoverable
// @formal entrypoint webhook.delivery_requires_an_enqueued_fact
// @formal entrypoint webhook.lifecycle_commit_and_outbox_are_atomic
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use crate::repository::{ManagedLifecycleFact, SessionId};
use crate::state::SessionWorkerObligation;
use async_trait::async_trait;
use std::collections::HashSet;

/// Durable Session observations without root mutation or reconciliation maintenance.
///
/// The original Store implements this role directly. `scan` accepts only the
/// four SELECT-only selections; quarantine maintenance stays on
/// [`SessionReconciliation::reconciliation_scan`]. The returned values grant no
/// execution, claim, cleanup or permission authority.
///
/// Capability decision table: R1 the same full Store upcasts to its reader
/// and the four original observations remain callable. R2-R4 a reader cannot
/// create, commit or maintain quarantine. These examples prove type capability
/// only; SQL regression tests own the absence of persistence side effects.
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_contract::{SessionRepository, SessionRepositoryReads};
/// use awaken_session_contract::SessionScanQuery;
/// use awaken_session_types::{SessionId};
/// async fn observe(repo: Arc<dyn SessionRepository>, id: &SessionId) {
///     let reader: Arc<dyn SessionRepositoryReads> = repo;
///     let _ = reader.get(id).await;
///     let _ = reader.owner(id).await;
///     let _ = reader.scan(SessionScanQuery::Workspace { workspace_id: "w" }).await;
///     let _ = reader.deleted_event_admissions("w", id).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_contract::SessionRepositoryReads;
/// use awaken_session_contract::{PersistedSession, IdempotencyRecord};
/// async fn create(reader: &dyn SessionRepositoryReads, root: PersistedSession, identity: IdempotencyRecord) {
///     reader.create("w", root, identity, Vec::new()).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_contract::SessionRepositoryReads;
/// use awaken_session_contract::SessionMutation;
/// async fn commit(reader: &dyn SessionRepositoryReads, mutation: SessionMutation) {
///     reader.commit("w", mutation).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_contract::SessionRepositoryReads;
/// async fn maintain(reader: &dyn SessionRepositoryReads) {
///     reader.reconciliation_scan(None).await;
/// }
/// ```
#[async_trait]
pub trait SessionRepositoryReads: Send + Sync {
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

    /// Observe live-root or retained tombstone privacy in one storage snapshot.
    /// Only a known ordinary identity in this Workspace permits public scoped
    /// File access; missing, foreign and private identities return false.
    /// Every store must provide its selected native snapshot implementation.
    async fn workspace_content_is_public(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<bool, SessionRepositoryError>;

    /// Read a complete, unpaged SELECT-only selection. Corrupt rows fail
    /// closed with [`SessionRepositoryError::Corrupt`]; this role neither
    /// creates nor clears reconciliation quarantine evidence.
    ///
    /// Caller: Session listing, Worker ownership observation, Vault references
    /// and Managed credential rollout.
    async fn scan(
        &self,
        query: SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError>;

    /// Read deletion evidence only inside its persisted owner partition. None
    /// means no matching tombstone, not proof of non-admission.
    ///
    /// Caller: `SessionApplication` Event batch admission after Session deletion.
    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<Option<crate::repository::DeletedSessionEventAdmissions>, SessionRepositoryError>;
}

/// Outbound persistence port of the Session aggregate.
///
/// Every method is required and names a real consumer; a backend that cannot
/// answer one of them is not a Session repository. The lifecycle outbox written
/// atomically by [`Self::create`] and [`Self::commit`] is drained through the
/// separate [`LifecycleOutbox`](super::LifecycleOutbox) port, which the same
/// durable store implements.
///
/// ```
/// use awaken_session_contract::SessionRepository;
/// use awaken_session_contract::{PersistedSession, IdempotencyRecord, SessionMutation};
/// async fn commands(repo: &dyn SessionRepository, root: PersistedSession, identity: IdempotencyRecord, mutation: SessionMutation) {
///     let _ = repo.create("w", root, identity, Vec::new()).await;
///     let _ = repo.commit("w", mutation).await;
///     let _ = repo.reconciliation_scan(None).await;
/// }
/// ```
#[async_trait]
pub trait SessionRepository: SessionRepositoryReads + SessionReconciliation {
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

    /// One keyset page of Run-free preparation roots: explicit initial demand
    /// or an Idle MCP update retained in the same authoritative Session.
    /// Adapters may use their root-derived index; the application always
    /// reloads and validates the current root before its existing CAS claim.
    /// The default keeps test doubles correct without another task registry.
    async fn preparation_candidates(
        &self,
        after: Option<&SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let mut page = self.reconciliation_scan(after).await?;
        page.sessions
            .retain(|scoped| scoped.session.is_run_free_preparation_candidate());
        Ok(page)
    }

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

    /// Count one typed Environment phase across every canonical live Session
    /// row. It is global, unpaged and decodes every row, so it cannot be a
    /// [`Self::scan`] page.
    ///
    /// Caller: `SessionEventBatchCutoverValidationSource`.
    async fn count_environment_phase(
        &self,
        phase: crate::repository::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError>;
}

/// Read one operational capacity projection from the canonical Session roots.
/// Multiple unfinished Sessions bound to the same logical Worker need one
/// retained Pod, including live Sessions with no claimable Dispatch. Any
/// unbound, never-materialized terminal work needs one first-placement Pod.
/// This read grants no claim or recovery.
async fn session_worker_demand_from_scan<R: SessionReconciliation + ?Sized>(
    repository: &R,
) -> Result<u64, SessionRepositoryError> {
    let mut cursor: Option<SessionScanCursor> = None;
    let mut owners = HashSet::new();
    let mut unbound = false;
    loop {
        let scan = repository.reconciliation_scan(cursor.as_ref()).await?;
        if !scan.quarantined.is_empty() {
            return Err(SessionRepositoryError::Corrupt(
                "Session reconciliation scan contains quarantined roots".into(),
            ));
        }
        for scoped in scan.sessions {
            let session = scoped.session;
            match session
                .worker_obligation()
                .map_err(|error| SessionRepositoryError::Corrupt(error.to_string()))?
            {
                SessionWorkerObligation::RetainedOwner(owner) => {
                    owners.insert(owner.to_owned());
                }
                SessionWorkerObligation::UnboundInitialPreparation
                | SessionWorkerObligation::UnboundTerminalCleanup => unbound = true,
                SessionWorkerObligation::None => {}
            }
        }
        let Some(next) = scan.next_cursor else {
            let count = u64::try_from(owners.len()).map_err(|_| {
                SessionRepositoryError::AuthorityUnavailable(
                    "Session Worker demand count overflow".into(),
                )
            })?;
            return count.checked_add(u64::from(unbound)).ok_or_else(|| {
                SessionRepositoryError::AuthorityUnavailable(
                    "Session Worker demand count overflow".into(),
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

/// Maintenance observations needed by checkpoint retention and demand sampling.
/// This role may maintain quarantine evidence but cannot mutate a Session root.
///
/// ```compile_fail,E0599
/// use awaken_session_contract::SessionReconciliation;
/// use awaken_session_contract::SessionMutation;
/// fn cannot_commit(source: &dyn SessionReconciliation, mutation: SessionMutation) {
///     let _ = source.commit("w", mutation);
/// }
/// ```
#[async_trait]
pub trait SessionReconciliation: Send + Sync {
    /// Revalidate one page of the canonical reconciliation index, maintaining
    /// the original row-local quarantine evidence in the same Store.
    /// Successful EOF, cursor advancement and downstream effect admission stay
    /// with their existing callers; this method grants no realization lease.
    async fn reconciliation_scan(
        &self,
        after: Option<&SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError>;

    /// Capacity observation derived from the same committed Session roots.
    /// Native stores may answer this from their transactionally maintained
    /// root index; test doubles retain the exact paged reference algorithm.
    /// This read never grants a Worker claim or a realization lease.
    async fn worker_demand(&self) -> Result<u64, SessionRepositoryError> {
        session_worker_demand_from_scan(self).await
    }
}
