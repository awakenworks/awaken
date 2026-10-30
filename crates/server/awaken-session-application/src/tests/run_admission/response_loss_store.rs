//! A Session store decorator that loses a response AFTER the real store
//! committed, or fails a write BEFORE it. `FaultingSessionRepository` models
//! a lost CAS response as a conflict; the T33/T04/T17 rows also need the
//! ambiguous `AuthorityUnavailable` a lost network response produces.
use super::*;
use awaken_session_state_contract::{
    IdempotencyRecord, ManagedLifecycleFact, SessionCreateResult, SessionIdempotencyReceipt,
    SessionMutation, SessionMutationResult, SessionRepositoryError,
};

#[derive(Default)]
struct Faults {
    /// Commit for real, then answer Unavailable (unknown outcome).
    commit_then_unavailable: Option<String>,
    /// Answer Unavailable without reaching the store (determinately absent).
    commit_unavailable: Option<String>,
    /// Create for real, then answer Unavailable.
    create_then_unavailable: bool,
    /// Answer Unavailable to a create without reaching the store.
    create_unavailable: bool,
    /// Answer Unavailable to the next exact create replay query.
    replay_create_unavailable: bool,
}

pub(crate) struct ResponseLossStore {
    inner: Arc<dyn awaken_session_repository_contract::SessionStore>,
    faults: Mutex<Faults>,
    pub(crate) commits: AtomicUsize,
    pub(crate) creates: AtomicUsize,
}

fn lost(detail: &str) -> SessionRepositoryError {
    SessionRepositoryError::AuthorityUnavailable(format!("injected {detail}"))
}

impl ResponseLossStore {
    pub(crate) fn new(inner: Arc<dyn awaken_session_repository_contract::SessionStore>) -> Self {
        Self {
            inner,
            faults: Mutex::default(),
            commits: AtomicUsize::new(0),
            creates: AtomicUsize::new(0),
        }
    }

    pub(crate) fn commit_then_unavailable_once(&self, operation: &str) {
        self.faults.lock().unwrap().commit_then_unavailable = Some(operation.into());
    }

    pub(crate) fn commit_unavailable_once(&self, operation: &str) {
        self.faults.lock().unwrap().commit_unavailable = Some(operation.into());
    }

    pub(crate) fn create_then_unavailable_once(&self) {
        self.faults.lock().unwrap().create_then_unavailable = true;
    }

    pub(crate) fn create_unavailable_once(&self) {
        self.faults.lock().unwrap().create_unavailable = true;
    }

    pub(crate) fn replay_create_unavailable_once(&self) {
        self.faults.lock().unwrap().replay_create_unavailable = true;
    }
}

#[async_trait::async_trait]
impl awaken_session_repository_contract::SessionRepositoryReads for ResponseLossStore {
    async fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionRepositoryError> {
        self.inner.owner(session_id).await
    }

    async fn scan(
        &self,
        query: awaken_session_state_contract::SessionScanQuery<'_>,
    ) -> Result<awaken_session_state_contract::SessionScan, SessionRepositoryError> {
        self.inner.scan(query).await
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<
        Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
        SessionRepositoryError,
    > {
        self.inner
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }
}

#[async_trait::async_trait]
impl SessionRepository for ResponseLossStore {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        let (before, after) = {
            let mut faults = self.faults.lock().unwrap();
            (
                std::mem::take(&mut faults.create_unavailable),
                std::mem::take(&mut faults.create_then_unavailable),
            )
        };
        if before {
            return Err(lost("create outage before the root insert"));
        }
        self.creates.fetch_add(1, Ordering::SeqCst);
        let result = self
            .inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await?;
        if after {
            return Err(lost("lost create response"));
        }
        Ok(result)
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        if std::mem::take(&mut self.faults.lock().unwrap().replay_create_unavailable) {
            return Err(lost("create replay outage"));
        }
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let (before, after) = {
            let mut faults = self.faults.lock().unwrap();
            let matches = |configured: &Option<String>| {
                configured
                    .as_ref()
                    .is_some_and(|operation| mutation.idempotency.key.contains(operation))
            };
            let before = matches(&faults.commit_unavailable);
            if before {
                faults.commit_unavailable = None;
            }
            let after = matches(&faults.commit_then_unavailable);
            if after {
                faults.commit_then_unavailable = None;
            }
            (before, after)
        };
        if before {
            return Err(lost("root-CAS outage before commit"));
        }
        self.commits.fetch_add(1, Ordering::SeqCst);
        let result = self.inner.commit(owner_scope, mutation).await?;
        if after {
            return Err(lost("lost root-CAS response"));
        }
        Ok(result)
    }

    async fn idempotency_receipt(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn count_environment_phase(
        &self,
        phase: awaken_session_state_contract::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        self.inner.count_environment_phase(phase).await
    }
}

use awaken_session_repository_contract::SessionReconciliation;

#[async_trait::async_trait]
impl SessionReconciliation for ResponseLossStore {
    async fn reconciliation_scan(
        &self,
        after: Option<&awaken_session_state_contract::SessionScanCursor>,
    ) -> Result<awaken_session_state_contract::SessionScan, SessionRepositoryError> {
        self.inner.reconciliation_scan(after).await
    }
}
#[async_trait::async_trait]
impl awaken_session_repository_contract::LifecycleOutbox for ResponseLossStore {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }
}
