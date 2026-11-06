//! T26-C03: whole-store physical purge removes every table of exactly one
//! store, leaves a neighbour byte-identical, is idempotent and survives reopen.
//!
//! Seed (per store A and neighbour B): one head created+updated (two history
//! rows), the first version redacted, a second head, and one preserved copy
//! conflict proposal.
//!
//! | Rule | Action | Effect |
//! |---|---|---|
//! | P1 | purge(A) | summary = exact seeded counts; heads, versions, conflicts of A empty |
//! | P2 | purge(A) | B's heads, versions (incl. redaction) and proposals identical to before |
//! | P3 | purge(A) again | zero summary, nothing else changes |
//! | P4 | reopen (file SQLite / new PG pool) | P1+P2 still hold on the durable state |
//!
//! Backends: Volatile, real-file SQLite (purge through handle 1, observe through
//! an independently opened handle 2 and after reopen), PostgreSQL (fresh schema,
//! independent pool after purge).
//!
//! Negative control: a heads-only purge (deletes heads through the writer port
//! only) is checked by the same `purge_violations` oracle and must be detected.

use awaken_memory_store::{
    MemoryContentPurge, MemoryCopyConflictPreservation, MemoryCopyConflictQueries,
    MemoryPurgeSummary, MemoryQueries, MemoryVersionQueries, MemoryVersionRedaction, MemoryWriter,
};
use awaken_resource_contract::MemoryCopyConflict;

const A: &str = "purge-target-a";
const B: &str = "purge-neighbour-b";

/// Everything the port can observe about one store, in a deterministic order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoreImage {
    heads: Vec<String>,
    versions: Vec<awaken_memory_store::MemoryVersion>,
    conflicts: Vec<MemoryCopyConflict>,
}

impl StoreImage {
    fn is_empty(&self) -> bool {
        self.heads.is_empty() && self.versions.is_empty() && self.conflicts.is_empty()
    }
}

trait Purgeable:
    MemoryQueries
    + MemoryWriter
    + MemoryVersionQueries
    + MemoryVersionRedaction
    + MemoryCopyConflictQueries
    + MemoryCopyConflictPreservation
    + MemoryContentPurge
{
}

impl<T> Purgeable for T where
    T: MemoryQueries
        + MemoryWriter
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryContentPurge
{
}

async fn image<M: Purgeable + ?Sized>(fs: &M, store: &str) -> StoreImage {
    let mut heads = Vec::new();
    for entry in MemoryQueries::list(fs, store, "/").await.unwrap() {
        let memory = MemoryQueries::get_by_path(fs, store, &entry.path)
            .await
            .unwrap()
            .expect("listed head exists");
        heads.push(format!("{entry:?}|{memory:?}"));
    }
    heads.sort();
    StoreImage {
        heads,
        versions: MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap(),
        conflicts: MemoryCopyConflictQueries::list_copy_conflicts(fs, store)
            .await
            .unwrap(),
    }
}

/// Seed one store; returns the expected purge summary for it.
async fn seed<M: Purgeable + ?Sized>(fs: &M, store: &str) -> MemoryPurgeSummary {
    let first = MemoryWriter::create(fs, store, "/note.md", &format!("{store}-v1"))
        .await
        .unwrap();
    MemoryWriter::update(fs, store, &first.generation(), &format!("{store}-v2"))
        .await
        .unwrap();
    MemoryWriter::create(fs, store, "/other.md", &format!("{store}-other"))
        .await
        .unwrap();
    let versions = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();
    assert_eq!(versions.len(), 3, "seed history");
    let redacted = MemoryVersionRedaction::redact_version(fs, store, &versions[0].id)
        .await
        .unwrap()
        .expect("redact first version");
    assert!(redacted.content.is_none(), "seed redaction");
    MemoryCopyConflictPreservation::preserve_copy_conflict(
        fs,
        store,
        &MemoryCopyConflict {
            path: "/proposal.md".into(),
            base: None,
            content: Some(format!("{store}-proposal")),
        },
    )
    .await
    .unwrap();
    MemoryPurgeSummary {
        heads_deleted: 2,
        versions_deleted: 3,
        copy_conflicts_deleted: 1,
    }
}

/// The shared T26-C03 oracle on store A's post-purge state.
fn purge_violations(after: &StoreImage) -> Vec<&'static str> {
    let mut violations = Vec::new();
    if !after.heads.is_empty() {
        violations.push("heads");
    }
    if !after.versions.is_empty() {
        violations.push("versions");
    }
    if !after.conflicts.is_empty() {
        violations.push("copy_conflicts");
    }
    violations
}

const ZERO: MemoryPurgeSummary = MemoryPurgeSummary {
    heads_deleted: 0,
    versions_deleted: 0,
    copy_conflicts_deleted: 0,
};

/// Seed A and B through `writer`, purge A through `writer`, and observe
/// through `observer` (which may be an independent handle). Returns B's image
/// before the purge so a reopen step can re-assert it.
async fn purge_one_store_conformance<W, O>(writer: &W, observer: &O) -> StoreImage
where
    W: Purgeable + ?Sized,
    O: Purgeable + ?Sized,
{
    let expected = seed(writer, A).await;
    seed(writer, B).await;
    let neighbour = image(observer, B).await;
    assert!(!image(observer, A).await.is_empty(), "A seeded");
    assert_eq!(
        neighbour
            .versions
            .iter()
            .filter(|v| v.content.is_none())
            .count(),
        1,
        "B carries one redacted version"
    );

    assert_eq!(
        MemoryContentPurge::purge_store(writer, A).await.unwrap(),
        expected,
        "P1 exact summary"
    );
    let after = image(observer, A).await;
    assert_eq!(purge_violations(&after), Vec::<&str>::new(), "P1 {after:?}");
    assert_eq!(
        image(observer, B).await,
        neighbour,
        "P2 neighbour identical"
    );

    assert_eq!(
        MemoryContentPurge::purge_store(writer, A).await.unwrap(),
        ZERO,
        "P3 idempotent"
    );
    assert!(image(observer, A).await.is_empty(), "P3");
    assert_eq!(
        image(observer, B).await,
        neighbour,
        "P3 neighbour identical"
    );
    neighbour
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn assert_reopened<M: Purgeable + ?Sized>(fs: &M, neighbour: &StoreImage) {
    assert!(image(fs, A).await.is_empty(), "P4 A empty after reopen");
    assert_eq!(
        &image(fs, B).await,
        neighbour,
        "P4 B identical after reopen"
    );
    assert_eq!(
        MemoryContentPurge::purge_store(fs, A).await.unwrap(),
        ZERO,
        "P4 idempotent after reopen"
    );
}

/// Negative-control mutant: removes only heads, through the writer port.
async fn heads_only_purge<M: Purgeable + ?Sized>(fs: &M, store: &str) {
    for entry in MemoryQueries::list(fs, store, "/").await.unwrap() {
        MemoryWriter::delete_by_path_as(fs, store, &entry.path, None)
            .await
            .unwrap();
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn volatile_purge_removes_exactly_one_store_and_heads_only_purge_is_detected() {
    let fs = awaken_memory_store::VolatileMemoryRepository::new(awaken_time::system_clock());
    purge_one_store_conformance(&fs, &fs).await;

    // NC: the same oracle must flag a heads-only purge.
    let mutant_store = "purge-mutant";
    seed(&fs, mutant_store).await;
    heads_only_purge(&fs, mutant_store).await;
    let violations = purge_violations(&image(&fs, mutant_store).await);
    assert!(
        violations.contains(&"versions") && violations.contains(&"copy_conflicts"),
        "NC heads-only purge detected: {violations:?}"
    );
}

#[cfg(feature = "sqlite")]
async fn sqlite_rows(path: &str, store: &str) -> [i64; 3] {
    use sqlx::Connection as _;
    let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{path}"))
        .await
        .unwrap();
    let mut counts = [0; 3];
    for (slot, table) in ["memories", "versions", "copy_conflicts"]
        .iter()
        .enumerate()
    {
        counts[slot] = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM memory_store_{table} WHERE store_id = ?1"
        ))
        .bind(store)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    }
    conn.close().await.unwrap();
    counts
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_file_purge_removes_exactly_one_store_across_handles_and_reopen() {
    use awaken_memory_store::SqliteMemoryRepository;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("purge-isolation.db");
    let path = path.to_str().unwrap().to_string();
    let neighbour = {
        let writer = SqliteMemoryRepository::open(&path, awaken_time::system_clock())
            .await
            .unwrap();
        let observer = SqliteMemoryRepository::open(&path, awaken_time::system_clock())
            .await
            .unwrap();
        purge_one_store_conformance(&writer, &observer).await
    };
    assert_eq!(sqlite_rows(&path, A).await, [0, 0, 0], "raw A tables empty");
    assert_eq!(sqlite_rows(&path, B).await, [2, 3, 1], "raw B rows intact");
    let reopened = SqliteMemoryRepository::open(&path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_reopened(&reopened, &neighbour).await;

    // NC on the durable backend too.
    seed(&reopened, "purge-mutant").await;
    heads_only_purge(&reopened, "purge-mutant").await;
    assert!(
        !purge_violations(&image(&reopened, "purge-mutant").await).is_empty(),
        "NC heads-only purge detected"
    );
    assert_ne!(
        sqlite_rows(&path, "purge-mutant").await[1],
        0,
        "NC history remains"
    );
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_memory_store::PostgresMemoryRepository;
    use sqlx::Executor;
    use sqlx::postgres::{PgConnectOptions, PgPool};

    async fn schema_pool() -> Option<(String, PgPool)> {
        let url = awaken_reliability_testkit::postgres_fixture_url()?;
        let admin = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(3))
            .connect(&url)
            .await
            .expect("configured Memory PostgreSQL fixture must connect");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_memory_purge_iso_{}_{nonce:x}", std::process::id());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        Some((schema.clone(), connect(&url, &schema).await))
    }

    async fn connect(url: &str, schema: &str) -> PgPool {
        let options = url
            .parse::<PgConnectOptions>()
            .expect("parse fixture url")
            .options([("search_path", schema)]);
        awaken_sql_storage::postgres_pool_options()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test schema")
    }

    async fn pg_rows(pool: &PgPool, store: &str) -> [i64; 3] {
        let mut counts = [0; 3];
        for (slot, table) in ["memories", "versions", "copy_conflicts"]
            .iter()
            .enumerate()
        {
            counts[slot] = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM memory_store_{table} WHERE store_id = $1"
            ))
            .bind(store)
            .fetch_one(pool)
            .await
            .unwrap();
        }
        counts
    }

    #[tokio::test]
    async fn postgres_purge_removes_exactly_one_store_across_pools_and_reopen() {
        let Some((schema, pool)) = schema_pool().await else {
            return;
        };
        let url = awaken_reliability_testkit::postgres_fixture_url().unwrap();
        let writer = PostgresMemoryRepository::with_pool(pool.clone(), awaken_time::system_clock());
        writer.ensure_schema().await.unwrap();
        let observer_pool = connect(&url, &schema).await;
        let observer =
            PostgresMemoryRepository::with_pool(observer_pool.clone(), awaken_time::system_clock());
        let neighbour = purge_one_store_conformance(&writer, &observer).await;
        assert_eq!(pg_rows(&observer_pool, A).await, [0, 0, 0], "raw A empty");
        assert_eq!(pg_rows(&observer_pool, B).await, [2, 3, 1], "raw B intact");
        pool.close().await;
        observer_pool.close().await;

        let reopened_pool = connect(&url, &schema).await;
        let reopened = PostgresMemoryRepository::with_existing_pool(
            reopened_pool.clone(),
            awaken_time::system_clock(),
        )
        .await
        .unwrap();
        assert_reopened(&reopened, &neighbour).await;
        seed(&reopened, "purge-mutant").await;
        heads_only_purge(&reopened, "purge-mutant").await;
        assert!(
            !purge_violations(&image(&reopened, "purge-mutant").await).is_empty(),
            "NC heads-only purge detected"
        );
    }

    #[test]
    fn postgres_purge_removes_exactly_one_store_fixture_admission() {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::postgres_purge_removes_exactly_one_store_across_pools_and_reopen",
        );
    }
}
