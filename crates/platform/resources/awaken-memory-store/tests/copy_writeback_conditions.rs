//! T24-C01 (完整条件与同内容 ABA): a materialized Copy is written back with
//! `update_materialized` after the source path, identity, version or hash
//! changed independently, or after a delete + same-content recreate, with the
//! write-back and the competing change on independently opened clients.
//!
//! | Row | Competing change (client B) | Write-back of the old Copy (client A) | Expected |
//! |---|---|---|---|
//! | C1 | delete + recreate same content (new id) | old generation, new bytes | Conflict/NotFound (old id is gone); the new head untouched |
//! | C2 | rename A->B->A (same id/hash/path, newer version) | old generation, same bytes | Conflict, not an idempotent Current |
//! | C3 | rename the source away | old generation, bytes equal to current | Conflict/NotFound before any idempotent answer; nothing at the old path |
//! | C4 | concurrent rename | concurrent write-back | exactly one linearization (edit carried or Conflict) |
//!
//! Backends: real-file SQLite (two `open(path)` handles) and PostgreSQL (two
//! pools on one fresh schema). The single-handle rows stay in `conformance.rs`.
//! Oracles (groups-4): O0 all conditions are checked in one transaction;
//! O1 an old Copy neither overwrites nor deletes the new head; O2 the path
//! error precedes idempotence.
//! File SQLite and PostgreSQL run all four rows as required tests. SQLite
//! reserves its writer before reading so concurrent clients can linearize.
//! Negative control (executed): a write-back that compares only bytes/hash
//! (`HashOnlyCas`) runs through the same checker and must be reported.

#![cfg(any(feature = "sqlite", feature = "postgres"))]

use awaken_memory_store::{
    MemErr, MemoryCopyWriteback, MemoryFilesystem, MemoryQueries, MemoryVersionQueries,
    MemoryWriter,
};

#[cfg(feature = "sqlite")]
use awaken_memory_store::{Memory, MemoryActor, MemoryGeneration, SqliteMemoryRepository};

#[cfg(feature = "postgres")]
#[path = "common/pg_schema.rs"]
mod pg;

/// Every oracle violation the checker observes for `copy` (the write-back
/// client) racing `other` (an independent client of the same database):
/// rows C1-C3 followed by the concurrent row C4.
#[cfg(feature = "postgres")]
async fn writeback_violations<C, M>(copy: &C, other: &M) -> Vec<String>
where
    C: MemoryCopyWriteback + ?Sized,
    M: MemoryWriter + MemoryQueries + MemoryFilesystem + MemoryVersionQueries + ?Sized,
{
    let mut violations = condition_violations(copy, other).await;
    violations.extend(concurrent_violations(copy, other).await);
    violations
}

/// Rows C1-C3: the competing change completes before the stale write-back.
async fn condition_violations<C, M>(copy: &C, other: &M) -> Vec<String>
where
    C: MemoryCopyWriteback + ?Sized,
    M: MemoryWriter + MemoryQueries + MemoryFilesystem + MemoryVersionQueries + ?Sized,
{
    let mut violations = Vec::new();

    // C1: delete + same-content recreate.
    let store = "t24-c1";
    let old = MemoryWriter::create(other, store, "/a.md", "same")
        .await
        .unwrap();
    MemoryWriter::delete_by_path(other, store, "/a.md")
        .await
        .unwrap();
    let recreated = MemoryWriter::create(other, store, "/a.md", "same")
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(other, store)
        .await
        .unwrap();
    let result = copy
        .update_materialized(store, "/a.md", &old.generation(), "stale copy edit")
        .await;
    // The old generation's identity no longer exists, so refusing it as
    // NotFound (the conformance "MP4 ABA" contract) is as correct as Conflict;
    // O1/O0 below still prove the refusal left the new head and history alone.
    if !matches!(result, Err(MemErr::Conflict { .. } | MemErr::NotFound(_))) {
        violations.push(format!(
            "C1/O1 the old Copy was accepted over a recreated head: {result:?}"
        ));
    }
    let head = MemoryQueries::get_by_path(other, store, "/a.md")
        .await
        .unwrap();
    if head.as_ref() != Some(&recreated) {
        violations.push(format!("C1/O1 the recreated head changed: {head:?}"));
    }
    if MemoryVersionQueries::list_versions(other, store)
        .await
        .unwrap()
        != history
    {
        violations.push("C1/O0 a refused write-back appended history".into());
    }

    // C2: rename A->B->A keeps id, hash and path but advances the version.
    let store = "t24-c2";
    let first = MemoryWriter::create(other, store, "/a.md", "same")
        .await
        .unwrap();
    MemoryFilesystem::rename_as(other, store, "/a.md", "/b.md", None)
        .await
        .unwrap();
    let current = MemoryFilesystem::rename_as(other, store, "/b.md", "/a.md", None)
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(other, store)
        .await
        .unwrap();
    let result = copy
        .update_materialized(store, "/a.md", &first.generation(), "same")
        .await;
    if !matches!(&result, Err(MemErr::Conflict { current: c }) if c.as_ref() == &current) {
        violations.push(format!(
            "C2/O0 a stale-version token passed as a retry: {result:?}"
        ));
    }
    if MemoryVersionQueries::list_versions(other, store)
        .await
        .unwrap()
        != history
    {
        violations.push("C2/O0 a refused write-back appended history".into());
    }

    // C3: the source moved away; the candidate equals the current bytes.
    let store = "t24-c3";
    let source = MemoryWriter::create(other, store, "/p.md", "same")
        .await
        .unwrap();
    let moved = MemoryFilesystem::rename_as(other, store, "/p.md", "/q.md", None)
        .await
        .unwrap();
    let result = copy
        .update_materialized(store, "/p.md", &source.generation(), "same")
        .await;
    if !matches!(result, Err(MemErr::Conflict { .. } | MemErr::NotFound(_))) {
        violations.push(format!(
            "C3/O2 a moved source answered idempotently: {result:?}"
        ));
    }
    if MemoryQueries::get_by_path(other, store, "/p.md")
        .await
        .unwrap()
        .is_some()
    {
        violations.push("C3/O1 the old Copy resurrected the moved path".into());
    }
    let head = MemoryQueries::get_by_path(other, store, "/q.md")
        .await
        .unwrap();
    if head.as_ref() != Some(&moved) {
        violations.push(format!("C3/O1 the moved head changed: {head:?}"));
    }

    violations
}

/// Row C4: concurrent write-back and rename on the two clients.
async fn concurrent_violations<C, M>(copy: &C, other: &M) -> Vec<String>
where
    C: MemoryCopyWriteback + ?Sized,
    M: MemoryWriter + MemoryQueries + MemoryFilesystem + MemoryVersionQueries + ?Sized,
{
    let mut violations = Vec::new();
    for index in 0..8 {
        let store = format!("t24-c4-{index}");
        let base = MemoryWriter::create(other, &store, "/a.md", "base")
            .await
            .unwrap();
        let generation = base.generation();
        let (update, rename) = tokio::join!(
            copy.update_materialized(&store, "/a.md", &generation, "edit"),
            MemoryFilesystem::rename_as(other, &store, "/a.md", "/b.md", None),
        );
        let renamed = match rename {
            Ok(renamed) => renamed,
            Err(error) => {
                violations.push(format!("C4/O0 the competing rename failed: {error:?}"));
                continue;
            }
        };
        let expected = match update {
            Ok(_) => "edit",
            Err(MemErr::Conflict { .. }) => "base",
            outcome => {
                violations.push(format!("C4/O0 illegal outcome: {outcome:?}"));
                continue;
            }
        };
        let head = MemoryQueries::get_by_path(other, &store, "/b.md")
            .await
            .unwrap();
        if head.and_then(|m| m.content).as_deref() != Some(expected) || renamed.path != "/b.md" {
            violations.push(format!(
                "C4/O0 the linearization lost the {expected} outcome"
            ));
        }
        if MemoryQueries::get_by_path(other, &store, "/a.md")
            .await
            .unwrap()
            .is_some()
        {
            violations.push("C4/O1 the write-back resurrected the renamed path".into());
        }
    }
    violations
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_sqlite_writeback_checks_every_condition_across_clients() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy.db");
    let path = path.to_str().unwrap();
    let copy = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let other = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        condition_violations(&copy, &other).await,
        Vec::<String>::new()
    );
}

/// Row C4 on two file-SQLite handles. Both pools carry a 30 s busy wait;
/// every write transaction reserves the lock with `BEGIN IMMEDIATE`, so the
/// losing writer waits for one linearization instead of failing a deferred
/// read->write upgrade with `SQLITE_BUSY`.
#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_sqlite_concurrent_writeback_and_rename_linearize_across_clients() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy-c4.db");
    let path = path.to_str().unwrap();
    let copy = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let other = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        concurrent_violations(&copy, &other).await,
        Vec::<String>::new()
    );
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_writeback_checks_every_condition_across_pools() {
    use awaken_memory_store::PostgresMemoryRepository;
    let Some((url, schema)) = pg::fresh_schema("t24_copy_writeback").await else {
        return;
    };
    let other = PostgresMemoryRepository::with_pool(
        pg::connect(&url, &schema, "t24-other").await,
        awaken_time::system_clock(),
    );
    other.ensure_schema().await.unwrap();
    let copy = PostgresMemoryRepository::with_existing_pool(
        pg::connect(&url, &schema, "t24-copy").await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        writeback_violations(&copy, &other).await,
        Vec::<String>::new()
    );
    pg::drop_schema(&url, &schema).await;
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_writeback_checks_every_condition_across_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_writeback_checks_every_condition_across_pools",
    );
}

/// The negative control: a write-back that admits by content hash alone,
/// ignoring the generation's identity and version.
#[cfg(feature = "sqlite")]
struct HashOnlyCas(SqliteMemoryRepository);

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl MemoryCopyWriteback for HashOnlyCas {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryCopyWriteback::create_as(&self.0, store, path, content, actor).await
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        match MemoryQueries::get_by_path(&self.0, store, path).await? {
            Some(current) if current.content_sha256 == base.content_sha256() => {
                MemoryWriter::update(&self.0, store, &current.generation(), content).await
            }
            _ => {
                MemoryCopyWriteback::update_materialized(&self.0, store, path, base, content).await
            }
        }
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        MemoryCopyWriteback::delete_if_match_as(&self.0, store, path, base, actor).await
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hash_only_writeback_is_detected_by_the_same_checker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hash-only.db");
    let path = path.to_str().unwrap();
    let copy = HashOnlyCas(
        SqliteMemoryRepository::open(path, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let other = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let violations = condition_violations(&copy, &other).await;
    assert!(
        violations.iter().any(|v| v.starts_with("C1/O1")),
        "NC: a bytes/hash-only CAS overwrites the recreated head and is reported: {violations:#?}"
    );
}
