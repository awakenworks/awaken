// @formal evidence memory.absent_conditional_delete_is_idempotent
// @formal evidence memory.cas_has_one_winner_per_generation
// @formal evidence memory.idempotent_write_does_not_advance_generation
// @formal evidence memory.matched_conditional_delete_removes_exact_head
// @formal evidence memory.rename_replace_preserves_source_identity
// @formal evidence memory.stale_cas_never_clobbers_live_content
// @formal evidence memory.stale_conditional_delete_never_removes_live_head
//! Backend-generic conformance for the path-addressed `MemoryRepository` port (ADR-0053).
//! The cross-node CAS backend (`PostgresMemoryRepository`) keeps the same POSIX-replace and
//! compare-and-swap semantics as the in-process backends.

#[cfg(feature = "test-support")]
use awaken_memory_store::VolatileMemoryRepository;
use awaken_memory_store::{MemErr, MemoryRepository};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
use std::sync::Arc;

#[path = "common/memory_repository.rs"]
mod shared;

#[cfg(feature = "test-support")]
#[tokio::test]
async fn volatile_materialized_path_cas() {
    let fs = VolatileMemoryRepository::new();
    shared::materialized_path_cas(&fs).await;
    shared::complete_generation_rejects_same_content_aba(&fs).await;
    shared::materialized_update_races_rename(&fs).await;
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
#[tokio::test]
async fn sqlite_materialized_path_cas() {
    let fs = awaken_memory_store::SqliteMemoryRepository::open_in_memory().unwrap();
    shared::materialized_path_cas(&fs).await;
    shared::complete_generation_rejects_same_content_aba(&fs).await;
    shared::materialized_update_races_rename(&fs).await;
}
#[allow(unused_imports)]
use shared::{
    atomic_head_update_conformance, concurrent_cas_one_winner, concurrent_create_one_winner,
    conformance as memory_repository_conformance,
    extended_conformance as memory_repository_extended,
    not_found_paths as memory_repository_not_found,
};

// --- Concurrency (ADR-0053 P2.5), extended from the in-memory-only unit tests to the
//     durable backends so the real write_lock (fs) / connection-mutex (sqlite)
//     serialize concurrent writers exactly one winner deep. ---

async fn conditional_delete_never_removes_a_changed_or_recreated_head(fs: &dyn MemoryRepository) {
    // Causes: C1 path absent/present; C2 present id matches; C3 present hash
    // matches; C4 a deleted path is recreated with equal content but fresh id.
    // Effects: E1 absent returns false and leaves history unchanged; E2 either
    // generation mismatch conflicts and leaves head/history unchanged; E3 exact token
    // returns true, removes that head and appends its one deletion version.
    // Decision rules: D1=!C1=>E1; D2=C1&&(!C2||!C3)=>E2;
    // D3=C1&&C2&&C3=>E3; D4=C4 implies !C2 and therefore E2.
    let first = fs.create("guarded", "/note.md", "v1").await.unwrap();
    let changed = fs
        .update("guarded", &first.generation(), "v2")
        .await
        .unwrap();
    let before_stale = fs.list_versions("guarded").await.unwrap();
    assert!(matches!(
        fs.delete_if_match("guarded", "/note.md", &first.generation())
            .await,
        Err(MemErr::Conflict { .. })
    ));
    assert_eq!(fs.list_versions("guarded").await.unwrap(), before_stale);
    assert_eq!(
        fs.get_by_path("guarded", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2")
    );
    assert!(
        fs.delete_if_match("guarded", "/note.md", &changed.generation())
            .await
            .unwrap()
    );
    let after_delete = fs.list_versions("guarded").await.unwrap();
    assert_eq!(after_delete.len(), before_stale.len() + 1);
    assert_eq!(
        after_delete.last().unwrap().operation,
        awaken_memory_store::MemoryVersionOperation::Deleted
    );
    assert_eq!(after_delete.last().unwrap().memory_id, changed.id);
    assert!(
        !fs.delete_if_match("guarded", "/note.md", &changed.generation())
            .await
            .unwrap(),
        "an already absent head is an idempotent no-op"
    );
    assert_eq!(fs.list_versions("guarded").await.unwrap(), after_delete);

    let recreated = fs.create("guarded", "/note.md", "v2").await.unwrap();
    assert_ne!(recreated.id, changed.id);
    let before_recreated_stale = fs.list_versions("guarded").await.unwrap();
    assert!(matches!(
        fs.delete_if_match("guarded", "/note.md", &changed.generation())
            .await,
        Err(MemErr::Conflict { .. })
    ));
    assert_eq!(
        fs.get_by_path("guarded", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .id,
        recreated.id
    );
    assert_eq!(
        fs.list_versions("guarded").await.unwrap(),
        before_recreated_stale
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn in_memory_conditional_delete_is_atomic() {
    conditional_delete_never_removes_a_changed_or_recreated_head(&VolatileMemoryRepository::new())
        .await;
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
#[tokio::test]
async fn sqlite_conditional_delete_is_atomic() {
    use awaken_memory_store::SqliteMemoryRepository;
    conditional_delete_never_removes_a_changed_or_recreated_head(
        &SqliteMemoryRepository::open_in_memory().unwrap(),
    )
    .await;
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
#[tokio::test]
async fn sqlite_concurrent_create_has_exactly_one_winner() {
    use awaken_memory_store::SqliteMemoryRepository;
    concurrent_create_one_winner(Arc::new(SqliteMemoryRepository::open_in_memory().unwrap())).await;
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
#[tokio::test]
async fn sqlite_concurrent_cas_has_exactly_one_winner() {
    use awaken_memory_store::SqliteMemoryRepository;
    // Fixture decision table: R1 fresh store; R2 an unrelated committed create
    // precedes CAS in the same store (as in the PostgreSQL gate). Both require
    // one CAS winner, one appended Modified row and an unchanged history prefix.
    // Separate repositories isolate the two rules; R2 deliberately shares its
    // repository across create and CAS so an empty-store assumption is exposed.
    for with_history in [false, true] {
        let fs = Arc::new(SqliteMemoryRepository::open_in_memory().unwrap());
        if with_history {
            concurrent_create_one_winner(fs.clone()).await;
        }
        concurrent_cas_one_winner(fs).await;
    }
}

/// Live Postgres conformance on a fresh schema. An explicitly configured
/// `AWAKEN_TEST_DATABASE_URL` is a required gate; only an absent configuration
/// may skip an unavailable default development database.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_memory_store::PostgresMemoryRepository;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        let admin = match PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(3))
            .connect(&database_url())
            .await
        {
            Ok(pool) => pool,
            Err(err) => {
                assert!(
                    std::env::var_os("AWAKEN_TEST_DATABASE_URL").is_none(),
                    "explicit Postgres test endpoint must be reachable: {err}"
                );
                println!("[skip] no Postgres reachable: {err}");
                return None;
            }
        };
        // Harness rule PG-H: parallel/repeated runs must never drop another
        // test's schema. Allocate a fresh namespace; an explicit endpoint that
        // fails connection/setup is a failed gate, never a successful skip.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("{schema}_{}_{nonce:x}", std::process::id());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(move |conn, _meta| {
                let schema = schema.clone();
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .map(Some)
            .expect("connect to the newly created Postgres test schema")
    }

    /// The FULL path-addressed `MemoryRepository` conformance + extended + not-found suites
    /// against `PostgresMemoryRepository` — the cross-node CAS backend whose `FOR UPDATE` row locks,
    /// transactional POSIX-replace, and `MemErr::Conflict`-on-stale-base are its whole
    /// reason to exist and previously had zero coverage.
    #[tokio::test]
    async fn postgres_memory_repository_conforms() {
        let Some(pool) = schema_pool("t_memoryfs").await else {
            return;
        };

        // Causal graph and decision table:
        // absent ledger + verify -> fail/no DDL; migrate -> current ledger;
        // current ledger + verify -> serve/no DDL.
        assert!(
            PostgresMemoryRepository::with_existing_pool(pool.clone())
                .await
                .is_err()
        );
        let ledger_after_verify: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('memory_store_schema_migrations')::text")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ledger_after_verify, None, "verify never creates its ledger");

        let fs = PostgresMemoryRepository::with_pool(pool.clone());
        fs.ensure_schema().await.unwrap();
        PostgresMemoryRepository::with_existing_pool(pool)
            .await
            .unwrap();
        memory_repository_conformance(&fs).await;
        memory_repository_extended(&fs).await;
        memory_repository_not_found(&fs).await;
        atomic_head_update_conformance(&fs).await;
        conditional_delete_never_removes_a_changed_or_recreated_head(&fs).await;
        shared::materialized_path_cas(&fs).await;
        shared::complete_generation_rejects_same_content_aba(&fs).await;
        shared::materialized_update_races_rename(&fs).await;
        shared::rename_replace_orphans_the_destination_id(&fs).await;
    }

    /// Shared concurrency cause/effect table for the real Postgres adapter:
    /// C1 eight creates share one absent path; C2 eight updates share one base
    /// hash. E1 exactly one create commits and seven return `PathConflict`; E2
    /// exactly one update commits and seven return `Conflict`. Raw storage
    /// errors, multiple winners, and lost heads are forbidden outcomes.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// | PGC1 | T | F | E1 one create winner |
    /// | PGC2 | F | T | E2 one CAS winner |
    #[tokio::test]
    async fn postgres_concurrent_create_and_cas_have_one_winner() {
        let Some(pool) = schema_pool("t_memory_repo_race").await else {
            return;
        };
        let fs = Arc::new(PostgresMemoryRepository::with_pool(pool));
        fs.ensure_schema().await.unwrap();
        concurrent_create_one_winner(fs.clone()).await;
        concurrent_cas_one_winner(fs).await;
    }

    /// The Postgres high-water row is durable and transactionally incremented, so
    /// deletion cannot make an id reusable.
    #[tokio::test]
    async fn postgres_never_reuses_a_deleted_top_ordinal() {
        let Some(pool) = schema_pool("t_memory_repo_reuse").await else {
            return;
        };
        let fs = PostgresMemoryRepository::with_pool(pool);
        fs.ensure_schema().await.unwrap();
        shared::ids_stay_monotonic_across_delete(&fs).await;
    }

    #[tokio::test]
    async fn postgres_negative_memory_version_fails_closed() {
        // Cause/effect graph: C1 the SQL version is non-negative or negative;
        // C2 get/list decodes the authority. Effects: E1 exact version for C1>=0
        // and E2 typed storage failure for C1<0; negative never wraps to u64.
        //
        // | Rule | version | Read | Effect |
        // | V1 | non-negative | get/list | E1 exact |
        // | V2 | negative | get | E2 storage error |
        // | V3 | negative | list | E2 storage error |
        let Some(pool) = schema_pool("t_memory_negative_version").await else {
            return;
        };
        let fs = PostgresMemoryRepository::with_pool(pool.clone());
        fs.ensure_schema().await.unwrap();
        fs.create("s", "/negative.md", "value")
            .await
            .expect("V1 create");
        sqlx::query(
            "UPDATE memory_store_memories SET version = -1 WHERE store_id = 's' AND path = '/negative.md'",
        )
        .execute(&pool)
        .await
        .expect("inject historical corruption");
        assert!(
            matches!(
                fs.get_by_path("s", "/negative.md").await,
                Err(MemErr::Storage(_))
            ),
            "V2/E2"
        );
        assert!(
            matches!(fs.list("s", "/").await, Err(MemErr::Storage(_))),
            "V3/E2"
        );
    }
}
