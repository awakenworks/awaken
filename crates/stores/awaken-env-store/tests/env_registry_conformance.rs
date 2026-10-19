//! Trait-generic **conformance suite** for the Environment definition roles — the shared behavioural
//! contract every backend must satisfy, run against both the in-memory reference and the
//! SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! It lifts the Phase-1 `proptest` invariants (which fuzz only the in-memory backend) to
//! the PORT: the durable sqlite registry must be observably identical to the reference on
//! the archive-vs-delete distinction and fail-closed lookups. A divergence — e.g. a
//! durable `delete` that soft-hides instead of removing, or an `archive` that drops a
//! record `get` should still return — is a real cross-deployment inconsistency, caught
//! here as a contract violation. Postgres joins behind its DB harness.
//!
//! The suite lives in `awaken_store_conformance::env_registry`; this file only
//! supplies the backends.

use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_store_conformance::env_registry::run_suite;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        // The sqlx pool bounds connection admission with a Tokio timer.
        .enable_time()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(awaken_time::system_clock(), |clock| {
        std::future::ready(InMemoryEnvRegistry::new(clock))
    }));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(awaken_time::system_clock(), |clock| async move {
        SqliteEnvRegistry::open_in_memory(clock)
            .await
            .expect("sqlite in-memory registry")
    }));
}

// Each shared rule requires a fresh database, not merely another registry view
// over the previous rule's rows. Only this fixture's successful CREATE SCHEMA
// results are recorded for cleanup. No pre-existing namespace is dropped.
struct PostgresSuiteFixture {
    url: String,
    admin: sqlx::postgres::PgPool,
    schemas: std::sync::Mutex<Vec<String>>,
    pools: std::sync::Mutex<Vec<sqlx::postgres::PgPool>>,
}

impl PostgresSuiteFixture {
    async fn connect(url: String) -> Self {
        let admin = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the configured Environment PostgreSQL fixture");
        Self {
            url,
            admin,
            schemas: std::sync::Mutex::new(Vec::new()),
            pools: std::sync::Mutex::new(Vec::new()),
        }
    }

    async fn fresh(
        &self,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> awaken_env_store::PostgresEnvRegistry {
        use sqlx::Executor;

        // The database allocates the identity, avoiding process/time-based
        // collisions between concurrent fixture invocations.
        let identity: String = sqlx::query_scalar("SELECT txid_current()::text")
            .fetch_one(&self.admin)
            .await
            .expect("allocate an owned schema identity");
        assert!(!identity.is_empty() && identity.bytes().all(|b| b.is_ascii_digit()));
        let schema = format!("t_env_contract_{identity}");
        self.admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create a fresh Environment conformance schema");
        self.schemas
            .lock()
            .expect("owned schema list")
            .push(schema.clone());
        let options = self
            .url
            .parse::<sqlx::postgres::PgConnectOptions>()
            .expect("parse configured PostgreSQL fixture options")
            .options([("search_path", schema.as_str())]);
        // Keep the original checkout verification; startup options select the
        // schema without replacing the shared after_connect/before_acquire.
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(2)
            .connect_with(options)
            .await
            .expect("connect to the owned conformance schema");
        self.pools
            .lock()
            .expect("owned pool list")
            .push(pool.clone());
        awaken_env_store::PostgresEnvRegistry::with_pool(pool, clock)
            .await
            .expect("open the original Environment migrations and backend")
    }

    async fn cleanup(&self) -> Vec<String> {
        use sqlx::Executor;

        let pools = { std::mem::take(&mut *self.pools.lock().expect("owned pool list")) };
        for pool in pools {
            pool.close().await;
        }
        let schemas = { std::mem::take(&mut *self.schemas.lock().expect("owned schema list")) };
        let mut errors = Vec::new();
        for schema in schemas {
            if let Err(error) = self
                .admin
                .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
                .await
            {
                errors.push(error.to_string());
            }
        }
        self.admin.close().await;
        errors
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_backend_conforms() {
    // Tokio's async test runtime enables the I/O driver. The original blocking
    // helper above intentionally remains unchanged for Memory and SQLite.
    // Ordinary component runs retain the repository's explicit optional-fixture
    // policy. The required PostgreSQL lane sets AWAKEN_TEST_REQUIRE_POSTGRES=1,
    // so missing, blank or unreachable configuration cannot qualify this suite.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = std::sync::Arc::new(PostgresSuiteFixture::connect(url).await);
    let factory = fixture.clone();
    // The shared SQLx suite borrows connections across await points and does
    // not promise Send. Drive its unchanged rules on this test thread; a local
    // task still turns a panic into JoinError so owned schemas are cleaned up.
    let local = tokio::task::LocalSet::new();
    let suite = local
        .run_until(local.spawn_local(async move {
            run_suite(awaken_time::system_clock(), move |clock| {
                let fixture = factory.clone();
                async move { fixture.fresh(clock).await }
            })
            .await;
        }))
        .await;
    // Even a shared assertion panic is joined before closing/dropping only our
    // schemas; the original assertion failure remains a failed test.
    let cleanup_errors = fixture.cleanup().await;
    suite.expect("the unchanged shared Environment suite must pass on PostgreSQL");
    assert!(
        cleanup_errors.is_empty(),
        "owned schema cleanup failed: {cleanup_errors:?}"
    );
}
