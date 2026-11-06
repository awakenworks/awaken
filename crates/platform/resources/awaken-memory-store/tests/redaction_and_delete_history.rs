//! T25-C02 (幂等脱敏与作者): the current and a historical version are each
//! redacted twice by different actors, the head is then deleted normally, and
//! the backend is reopened.
//!
//! | Step | Action | Expected |
//! |---|---|---|
//! | S1 | create_as(A) then update_head_as(A) | two versions created by A |
//! | S2 | redact the historical version as R1, then again as R2 | one redaction: R1, one timestamp, content cleared |
//! | S3 | redact the current version as R1, then again as R2 | the same rule for the current version |
//! | S4 | delete_by_path_as(D) | history kept; one Deleted row by D appended |
//! | S5 | reopen (new handle / new pool) | exactly the S4 history |
//!
//! Backends: Volatile (no reopen), real-file SQLite with reopen, PostgreSQL
//! with an independent pool. Oracles (groups-4): O0 created_by is kept;
//! O1 redacted_by/time and the content-clearing rule are stable; O2 an
//! ordinary delete keeps history.
//! Negative control (executed): a delete that also erases the store's
//! history (`PurgingDelete`) runs through the same checker and is reported.

#![cfg(any(feature = "sqlite", feature = "postgres", feature = "test-support"))]

use awaken_memory_store::{
    MemoryActor, MemoryVersion, MemoryVersionOperation, MemoryVersionQueries,
    MemoryVersionRedaction, MemoryWriter,
};

#[cfg(feature = "sqlite")]
use awaken_memory_store::{MemErr, Memory, MemoryContentPurge, MemoryGeneration};

#[cfg(feature = "postgres")]
#[path = "common/pg_schema.rs"]
mod pg;

const STORE: &str = "t25-history";

fn actor(name: &str) -> MemoryActor {
    MemoryActor::ApiActor {
        api_key_id: name.into(),
    }
}

/// S1-S4 through `fs`; returns the history after the delete and every
/// violation observed on the way.
async fn exercise<M>(fs: &M) -> (Vec<MemoryVersion>, Vec<String>)
where
    M: MemoryWriter + MemoryVersionQueries + MemoryVersionRedaction + ?Sized,
{
    let mut violations = Vec::new();
    let created = MemoryWriter::create_as(fs, STORE, "/n.md", "v1", Some(&actor("author")))
        .await
        .unwrap();
    MemoryWriter::update_head_as(
        fs,
        STORE,
        &created.generation(),
        "v2",
        None,
        Some(&actor("author")),
    )
    .await
    .unwrap();
    let written = MemoryVersionQueries::list_versions(fs, STORE)
        .await
        .unwrap();
    for (row, version) in [("S2 historical", &written[0]), ("S3 current", &written[1])] {
        let first = fs
            .redact_version_as(STORE, &version.id, Some(&actor("redactor-1")))
            .await
            .unwrap();
        let again = fs
            .redact_version_as(STORE, &version.id, Some(&actor("redactor-2")))
            .await
            .unwrap();
        if first != again {
            violations.push(format!(
                "{row}/O1 a repeated redaction changed it: {first:?} -> {again:?}"
            ));
        }
    }
    let before_delete = MemoryVersionQueries::list_versions(fs, STORE)
        .await
        .unwrap();
    MemoryWriter::delete_by_path_as(fs, STORE, "/n.md", Some(&actor("deleter")))
        .await
        .unwrap();
    let after = MemoryVersionQueries::list_versions(fs, STORE)
        .await
        .unwrap();
    violations.extend(history_violations(&before_delete, &after));
    (after, violations)
}

/// O0-O2 over the history before and after the ordinary delete.
fn history_violations(before_delete: &[MemoryVersion], after: &[MemoryVersion]) -> Vec<String> {
    let mut violations = Vec::new();
    if after.len() != before_delete.len() + 1
        || after.get(..before_delete.len()) != Some(before_delete)
    {
        violations.push(format!(
            "O2 the delete did not keep the history it found ({} -> {} rows)",
            before_delete.len(),
            after.len()
        ));
        return violations;
    }
    for (row, version) in before_delete.iter().enumerate() {
        if version.created_by != Some(actor("author")) {
            violations.push(format!(
                "O0 row {row}: created_by lost: {:?}",
                version.created_by
            ));
        }
        if version.redacted_by != Some(actor("redactor-1"))
            || version.redacted_unix_nanos.is_none()
            || version.content.is_some()
        {
            violations.push(format!("O1 row {row}: redaction not stable: {version:?}"));
        }
    }
    let deleted = after.last().unwrap();
    if deleted.operation != MemoryVersionOperation::Deleted
        || deleted.created_by != Some(actor("deleter"))
    {
        violations.push(format!("O2 the delete row is not attributed: {deleted:?}"));
    }
    violations
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn volatile_redaction_is_idempotent_and_delete_keeps_history() {
    let fs = awaken_memory_store::VolatileMemoryRepository::new(awaken_time::system_clock());
    let (_, violations) = exercise(&fs).await;
    assert_eq!(violations, Vec::<String>::new());
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn file_sqlite_redaction_and_delete_history_survive_reopen() {
    use awaken_memory_store::SqliteMemoryRepository;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let path = path.to_str().unwrap();
    let fs = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let (history, violations) = exercise(&fs).await;
    assert_eq!(violations, Vec::<String>::new());
    drop(fs);
    let reopened = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        reopened.list_versions(STORE).await.unwrap(),
        history,
        "S5 the reopened history is exactly the committed one"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_redaction_and_delete_history_survive_an_independent_pool() {
    use awaken_memory_store::PostgresMemoryRepository;
    let Some((url, schema)) = pg::fresh_schema("t25_history").await else {
        return;
    };
    let pool = pg::connect(&url, &schema, "t25-first").await;
    let fs = PostgresMemoryRepository::with_pool(pool.clone(), awaken_time::system_clock());
    fs.ensure_schema().await.unwrap();
    let (history, violations) = exercise(&fs).await;
    assert_eq!(violations, Vec::<String>::new());
    pool.close().await;
    let reopened = PostgresMemoryRepository::with_existing_pool(
        pg::connect(&url, &schema, "t25-reopen").await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened.list_versions(STORE).await.unwrap(),
        history,
        "S5 an independent pool reads exactly the committed history"
    );
    pg::drop_schema(&url, &schema).await;
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_redaction_and_delete_history_survive_an_independent_pool_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_redaction_and_delete_history_survive_an_independent_pool",
    );
}

/// The negative control: an ordinary delete that also purges the history.
#[cfg(feature = "sqlite")]
struct PurgingDelete(awaken_memory_store::SqliteMemoryRepository);

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl MemoryWriter for PurgingDelete {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryWriter::create_as(&self.0, store, path, content, actor).await
    }

    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        MemoryWriter::update_head_as(&self.0, store, base, content, target_path, actor).await
    }

    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        MemoryWriter::delete_by_path_as(&self.0, store, path, actor).await?;
        // The divergence: deleting the head is treated as deleting its history.
        MemoryContentPurge::purge_store(&self.0, store)
            .await
            .map(|_| ())
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        MemoryWriter::delete_if_match_as(&self.0, store, path, base, actor).await
    }
}

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl MemoryVersionQueries for PurgingDelete {
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        MemoryVersionQueries::list_versions(&self.0, store).await
    }
}

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl MemoryVersionRedaction for PurgingDelete {
    async fn redact_version_as(
        &self,
        store: &str,
        version: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        MemoryVersionRedaction::redact_version_as(&self.0, store, version, actor).await
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_delete_that_erases_history_is_detected_by_the_same_checker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("purging.db");
    let fs = PurgingDelete(
        awaken_memory_store::SqliteMemoryRepository::open(
            path.to_str().unwrap(),
            awaken_time::system_clock(),
        )
        .await
        .unwrap(),
    );
    let (_, violations) = exercise(&fs).await;
    assert!(
        violations.iter().any(|v| v.starts_with("O2")),
        "NC: a delete that erases history is reported: {violations:#?}"
    );
}
