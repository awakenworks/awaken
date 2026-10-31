//! T34-C01: one SessionStore behaviour table across in-memory SQLite
//! (session_repo_conformance.rs), real-file SQLite and PostgreSQL.
//!
//! | Row | Backend | Suite |
//! |---|---|---|
//! | P1 | in-memory SQLite / PostgreSQL | `run_suite` (session_repo_conformance.rs) |
//! | P2 | file SQLite, fresh file per rule | `run_suite` |
//! | P3 | file SQLite, drop + reopen the same file | `reopen_preserves_authority` |
//! | P4 | PostgreSQL, drop pool + new independent pool on the same schema | `reopen_preserves_authority` |
//!
//! Negative controls, executed through the SAME suites (each must fail):
//! NC1 a divergent wrapper that drops the root CAS (rewrites the expected
//! revision to the current one); NC2 a wrapper that rebuilds the receipt from
//! the current root instead of reading the stored one; NC3 an in-memory
//! "file" factory for the reopen step.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_session_contract::{
    DeletedSessionEventAdmissions, IdempotencyRecord, ManagedLifecycleFact, PersistedSession,
    SessionCreateResult, SessionEnvironmentPhase, SessionIdempotencyReceipt, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryError, SessionScan,
    SessionScanCursor, SessionScanQuery,
};
use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
use awaken_session_types::{SessionId, SessionRevision};
use awaken_store_conformance::session_store::{reopen_preserves_authority, run_suite};
use sqlx::Executor as _;
use {
    awaken_session_application::SessionStore, awaken_session_contract::LifecycleOutbox,
    awaken_session_contract::SessionRepository, awaken_session_contract::SessionRepositoryReads,
};

/// Run the (large, possibly non-Send) suite on its own big-stack thread and
/// current-thread runtime, reporting whether it panicked.
fn on_suite_thread<F: std::future::Future<Output = ()> + 'static>(
    suite: impl FnOnce() -> F + Send + 'static,
) -> std::thread::Result<()> {
    std::thread::Builder::new()
        .name("t34-session-suite".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("suite runtime")
                .block_on(suite())
        })
        .expect("spawn suite thread")
        .join()
}

fn passes<F: std::future::Future<Output = ()> + 'static>(
    suite: impl FnOnce() -> F + Send + 'static,
) {
    if let Err(panic) = on_suite_thread(suite) {
        std::panic::resume_unwind(panic);
    }
}

fn fails<F: std::future::Future<Output = ()> + 'static>(
    suite: impl FnOnce() -> F + Send + 'static,
) -> bool {
    on_suite_thread(suite).is_err()
}

#[test]
fn file_sqlite_backend_conforms_with_a_fresh_file_per_rule() {
    passes(|| async {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let next = std::sync::atomic::AtomicUsize::new(0);
        run_suite(|| {
            let path = dir.path().join(format!(
                "rule-{}.db",
                next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            async move {
                SqliteManagedSessionRepository::open(path.to_str().unwrap())
                    .await
                    .expect("file SQLite Session store")
            }
        })
        .await;
        assert!(
            next.load(std::sync::atomic::Ordering::SeqCst) >= 17,
            "P2 every rule received its own file"
        );
    });
}

#[test]
fn file_sqlite_reopen_preserves_authority_and_in_memory_reopen_is_detected() {
    passes(|| async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reopen.db").to_str().unwrap().to_string();
        let first = SqliteManagedSessionRepository::open(&path).await.unwrap();
        reopen_preserves_authority(first, || {
            let path = path.clone();
            async move { SqliteManagedSessionRepository::open(&path).await.unwrap() }
        })
        .await;
    });
    // NC3: an in-memory "file" factory cannot pass the reopen row.
    assert!(
        fails(|| async {
            let first = SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap();
            reopen_preserves_authority(first, || async {
                SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap()
            })
            .await;
        }),
        "NC3 in-memory reopen must fail the reopen row"
    );
}

#[derive(Clone, Copy, Debug)]
enum Divergence {
    DropsCas,
    RebuildsReceipt,
}

/// A deliberately divergent backend over the in-memory SQLite reference.
struct Divergent {
    inner: SqliteManagedSessionRepository,
    mode: Divergence,
}

#[async_trait]
impl SessionRepositoryReads for Divergent {
    async fn get(&self, id: &SessionId) -> Result<PersistedSession, SessionRepositoryError> {
        self.inner.get(id).await
    }
    async fn owner(&self, id: &SessionId) -> Result<String, SessionRepositoryError> {
        self.inner.owner(id).await
    }
    async fn scan(
        &self,
        query: SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.inner.scan(query).await
    }
    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        id: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError> {
        self.inner.deleted_event_admissions(owner_scope, id).await
    }
}

#[async_trait]
impl SessionRepository for Divergent {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        self.inner
            .create(owner_scope, session, idempotency, facts)
            .await
    }
    async fn replay_create(
        &self,
        owner_scope: &str,
        id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        self.inner.replay_create(owner_scope, id, idempotency).await
    }
    async fn commit(
        &self,
        owner_scope: &str,
        mut mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let current = match self.mode {
            Divergence::DropsCas => self.inner.get(mutation.payload.session_id()).await.ok(),
            Divergence::RebuildsReceipt => None,
        };
        if let Some(current) = current {
            mutation.expected_revision = current.revision;
            match &mut mutation.payload {
                SessionMutationPayload::Replace(session) => session.revision = current.revision,
                SessionMutationPayload::Delete(tombstone) => {
                    tombstone.deleted_revision = SessionRevision(current.revision.0 + 1)
                }
            }
        }
        self.inner.commit(owner_scope, mutation).await
    }
    async fn preparation_candidates(
        &self,
        after: Option<&SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.inner.preparation_candidates(after).await
    }
    async fn idempotency_receipt(
        &self,
        id: &SessionId,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        let stored = self.inner.idempotency_receipt(id, key).await?;
        match (self.mode, stored) {
            (Divergence::RebuildsReceipt, Some(_)) => {
                let current = self.inner.get(id).await?;
                Ok(Some(SessionIdempotencyReceipt {
                    payload_hash: SessionMutationPayload::Replace(current.clone()).stable_hash(),
                    committed_revision: current.revision,
                }))
            }
            (_, stored) => Ok(stored),
        }
    }
    async fn count_environment_phase(
        &self,
        phase: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        self.inner.count_environment_phase(phase).await
    }
}

use awaken_session_contract::SessionReconciliation;

#[async_trait::async_trait]
impl SessionReconciliation for Divergent {
    async fn reconciliation_scan(
        &self,
        after: Option<&SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.inner.reconciliation_scan(after).await
    }

    async fn worker_demand(&self) -> Result<u64, SessionRepositoryError> {
        self.inner.worker_demand().await
    }
}
#[async_trait]
impl LifecycleOutbox for Divergent {
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

#[async_trait::async_trait]
impl awaken_session_contract::PreparedParticipantIntents for Divergent {
    async fn record_creation_intent(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        repository_id: &str,
        lease_expires_at_unix_ms: u64,
    ) -> Result<
        awaken_session_contract::PreparedParticipantIntentRecord,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .record_creation_intent(
                owner_scope,
                session_id,
                repository_id,
                lease_expires_at_unix_ms,
            )
            .await
    }

    async fn claim_creation_abort(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        awaken_session_contract::PreparedParticipantIntentClaim,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.claim_creation_abort(session_id).await
    }

    async fn complete_creation_abort(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.complete_creation_abort(session_id).await
    }

    async fn recoverable_creation_intents(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<
        Vec<awaken_session_contract::PreparedParticipantIntent>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .recoverable_creation_intents(now_unix_ms, limit)
            .await
    }
}

fn _divergent_is_a_session_store(store: &Divergent) -> &dyn SessionStore {
    store
}

#[test]
fn divergent_backends_fail_the_shared_behaviour_table() {
    for (name, mode) in [
        ("NC1 drops CAS", Divergence::DropsCas),
        ("NC2 rebuilds receipt", Divergence::RebuildsReceipt),
    ] {
        assert!(
            fails(move || run_suite(move || async move {
                Divergent {
                    inner: SqliteManagedSessionRepository::open_in_memory()
                        .await
                        .unwrap(),
                    mode,
                }
            })),
            "{name} must fail the shared suite"
        );
    }
}

#[test]
fn postgres_reopen_preserves_authority_through_an_independent_pool() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    passes(move || async move {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
            .expect("configured Session PostgreSQL fixture must connect");
        let schema = format!("t_session_t34_reopen_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create owned T34 schema");
        let connect = {
            let (url, schema) = (url.clone(), schema.clone());
            move || {
                let (url, schema) = (url.clone(), schema.clone());
                async move {
                    let options = url
                        .parse::<sqlx::postgres::PgConnectOptions>()
                        .unwrap()
                        .options([("search_path", schema.as_str())]);
                    sqlx::postgres::PgPoolOptions::new()
                        .max_connections(2)
                        .acquire_timeout(std::time::Duration::from_secs(5))
                        .connect_with(options)
                        .await
                        .expect("connect owned T34 schema")
                }
            }
        };
        let first = PostgresManagedSessionRepository::with_pool(connect().await)
            .await
            .expect("T34 PostgreSQL migrations");
        reopen_preserves_authority(first, || {
            let connect = connect.clone();
            async move {
                PostgresManagedSessionRepository::with_existing_pool(connect().await)
                    .await
                    .expect("T34 independent pool over the existing schema")
            }
        })
        .await;
        admin
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .expect("drop owned T34 schema");
        admin.close().await;
    });
}

#[test]
fn postgres_reopen_preserves_authority_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_reopen_preserves_authority_through_an_independent_pool",
    );
}
