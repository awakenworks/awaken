//! T34-C01: one Environment behaviour table across Volatile, real-file SQLite
//! (with a reopen step) and PostgreSQL (independent pool reopen).
//!
//! | Row | Backend | Suite |
//! |---|---|---|
//! | P1 | Volatile | `run_suite` (env_registry_conformance.rs) |
//! | P2 | file SQLite, fresh file per rule | `run_suite` |
//! | P3 | file SQLite, drop + reopen same file | `reopen_preserves_authority` |
//! | P4 | PostgreSQL, drop pool + new pool on same schema | `reopen_preserves_authority` |
//!
//! Negative controls, executed through the SAME suites (each must fail):
//! NC1 a divergent wrapper that drops the update CAS; NC2 a wrapper that
//! rebuilds the create receipt (re-mints the command); NC3 an in-memory
//! "file" factory for the reopen step.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem, EnvUpdate,
    EnvironmentDefinitionQueries, EnvironmentDefinitionStore, EnvironmentRegistrationIntent,
    EnvironmentRegistrationIntentFilter, EnvironmentRegistrationOutbox, EnvironmentRevision,
    EnvironmentStoreError,
};
use awaken_store_conformance::env_registry::{reopen_preserves_authority, run_suite};

/// A countermodel qualifies only at its expected assertion. Setup failures,
/// cancellation and unrelated panics cannot stand in for the oracle.
async fn suite_rejects_at(
    table: impl std::future::Future<Output = ()> + 'static,
    expected_assertion: &'static str,
) -> bool {
    let local = tokio::task::LocalSet::new();
    let Err(error) = local.run_until(local.spawn_local(table)).await else {
        return false;
    };
    if !error.is_panic() {
        return false;
    }
    let payload = error.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message.is_some_and(|message| message.starts_with(expected_assertion))
}

#[tokio::test]
async fn file_sqlite_backend_conforms_with_a_fresh_file_per_rule() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    run_suite(awaken_time::system_clock(), move |clock| {
        let path = dir.path().join(format!(
            "rule-{}.db",
            next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        async move {
            SqliteEnvRegistry::open(path.to_str().unwrap(), clock)
                .await
                .unwrap()
        }
    })
    .await;
}

#[tokio::test]
async fn file_sqlite_reopen_preserves_authority_and_in_memory_reopen_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.db").to_str().unwrap().to_string();
    let first = SqliteEnvRegistry::open(&path, awaken_time::system_clock())
        .await
        .unwrap();
    let reopen_path = path.clone();
    reopen_preserves_authority(first, move || {
        let path = reopen_path.clone();
        async move {
            SqliteEnvRegistry::open(&path, awaken_time::system_clock())
                .await
                .unwrap()
        }
    })
    .await;

    // NC3: an in-memory "file" factory cannot pass the reopen row.
    assert!(
        suite_rejects_at(
            async {
                let first = SqliteEnvRegistry::open_in_memory(awaken_time::system_clock())
                    .await
                    .unwrap();
                reopen_preserves_authority(first, || async {
                    SqliteEnvRegistry::open_in_memory(awaken_time::system_clock())
                        .await
                        .unwrap()
                })
                .await;
            },
            "assertion `left == right` failed: reopen image"
        )
        .await,
        "NC3 in-memory reopen must fail the reopen row"
    );
}

#[derive(Clone, Copy)]
enum Divergence {
    DropsCas,
    RebuildsReceipt,
}

/// A deliberately divergent backend over the reference registry.
struct Divergent {
    inner: InMemoryEnvRegistry,
    mode: Divergence,
    minted: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl EnvironmentDefinitionQueries for Divergent {
    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.inner.list_active().await
    }
    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.inner.list_all().await
    }
    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.get(id).await
    }
    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.get_revision(id, revision).await
    }
    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        self.inner.exists(id).await
    }
}

#[async_trait]
impl EnvironmentDefinitionStore for Divergent {
    async fn find_create_result(
        &self,
        command: &CreateEnvironmentCommand,
    ) -> Result<Option<EnvItem>, CreateEnvironmentError> {
        self.inner.find_create_result(command).await
    }
    async fn create_once(
        &self,
        mut command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        if matches!(self.mode, Divergence::RebuildsReceipt) {
            let n = self
                .minted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            command.command_id = format!("{}:reminted:{n}", command.command_id);
        }
        self.inner.create_once(command).await
    }
    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let expected = match self.mode {
            Divergence::DropsCas => match self.inner.get(id).await? {
                Some(current) => current.revision,
                None => expected_revision,
            },
            Divergence::RebuildsReceipt => expected_revision,
        };
        self.inner.update(id, expected, patch).await
    }
    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.archive(id).await
    }
}

#[async_trait]
impl EnvironmentRegistrationOutbox for Divergent {
    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.inner.registration_intent(id, revision).await
    }
    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.inner.registration_intents(filter).await
    }
    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        self.inner.mark_registration_intent_delivered(intent).await
    }
}

#[tokio::test]
async fn divergent_backends_fail_the_shared_behaviour_table() {
    for (name, mode) in [
        ("NC1 drops CAS", Divergence::DropsCas),
        ("NC2 rebuilds receipt", Divergence::RebuildsReceipt),
    ] {
        assert!(
            suite_rejects_at(
                run_suite(awaken_time::system_clock(), move |clock| {
                    std::future::ready(Divergent {
                        inner: InMemoryEnvRegistry::new(clock),
                        mode,
                        minted: Default::default(),
                    })
                }),
                match mode {
                    Divergence::DropsCas => "assertion `left == right` failed: V3",
                    Divergence::RebuildsReceipt =>
                        "assertion `left == right` failed: R2 read-only replay",
                }
            )
            .await,
            "{name} must fail the shared suite"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_reopen_preserves_authority_through_an_independent_pool() {
    use sqlx::Executor;
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = awaken_sql_storage::postgres_pool_options()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .expect("configured Environment PostgreSQL fixture must connect");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("t_env_t34_reopen_{}_{nonce:x}", std::process::id());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let connect = {
        let (url, schema) = (url.clone(), schema.clone());
        move || {
            let (url, schema) = (url.clone(), schema.clone());
            async move {
                let options = url
                    .parse::<sqlx::postgres::PgConnectOptions>()
                    .unwrap()
                    .options([("search_path", schema.as_str())]);
                awaken_sql_storage::postgres_pool_options()
                    .max_connections(2)
                    .acquire_timeout(std::time::Duration::from_secs(3))
                    .connect_with(options)
                    .await
                    .unwrap()
            }
        }
    };
    let first = awaken_env_store::PostgresEnvRegistry::with_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    reopen_preserves_authority(first, move || {
        let connect = connect.clone();
        async move {
            awaken_env_store::PostgresEnvRegistry::with_existing_pool(
                connect().await,
                awaken_time::system_clock(),
            )
            .await
            .unwrap()
        }
    })
    .await;
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop only this fixture's schema");
    admin.close().await;
}

#[test]
fn postgres_reopen_preserves_authority_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_reopen_preserves_authority_through_an_independent_pool",
    );
}

#[tokio::test]
async fn unrelated_failure_is_not_backend_divergence_evidence() {
    assert!(
        !suite_rejects_at(
            async { panic!("database setup failed") },
            "assertion `left == right` failed: reopen image"
        )
        .await
    );
}
