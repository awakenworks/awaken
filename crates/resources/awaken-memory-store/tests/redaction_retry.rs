//! T25-C03: redaction under mid-write failure and lost response, retried by a
//! different actor and clock through an independent handle.
//!
//! | Rule | Fault | Retry (actor2, clock2, new handle) | Effect |
//! |---|---|---|---|
//! | R1 | SQL trigger aborts the redaction UPDATE | after trigger removed | first attempt: Storage error, content intact, no redaction stamp; retry redacts once as actor2 |
//! | R2 | commit succeeds, response discarded (lost) | immediately | retry returns the ORIGINAL redaction (actor1, clock1 stamp); content stays cleared |
//! | R3 | both | neighbour versions | untouched |
//!
//! Backends: real-file SQLite (two independently opened handles) and PostgreSQL
//! (two independent pools on one fresh schema).

use std::sync::Arc;

use awaken_lease_testkit::FixedClock;
use awaken_memory_store::{
    MemErr, MemoryActor, MemoryVersion, MemoryVersionQueries, MemoryVersionRedaction, MemoryWriter,
};
use awaken_time::UtcTime;

const CLOCK1: u128 = 1_700_000_000_000_000_001;
const CLOCK2: u128 = 1_800_000_000_000_000_002;

fn actor(name: &str) -> MemoryActor {
    MemoryActor::ApiActor {
        api_key_id: name.into(),
    }
}

fn clock(nanos: u128) -> Arc<dyn awaken_time::WallClock> {
    Arc::new(FixedClock::at(UtcTime::from_unix_nanos(nanos)))
}

async fn seed<M: MemoryWriter + MemoryVersionQueries + ?Sized>(fs: &M) -> Vec<MemoryVersion> {
    let first = MemoryWriter::create(fs, "s", "/note.md", "secret-v1")
        .await
        .unwrap();
    MemoryWriter::update(fs, "s", &first.generation(), "secret-v2")
        .await
        .unwrap();
    MemoryWriter::create(fs, "neighbor", "/n.md", "neighbor")
        .await
        .unwrap();
    MemoryVersionQueries::list_versions(fs, "s").await.unwrap()
}

/// R1 after the trigger fired: nothing of the redaction is visible.
async fn assert_unredacted<M: MemoryVersionQueries + ?Sized>(fs: &M, before: &[MemoryVersion]) {
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, "s").await.unwrap(),
        before,
        "R1 no partial redaction"
    );
}

/// R1 retry + R2 lost response, shared by both SQL backends. `first` is the
/// original handle (clock1); `second` an independent handle (clock2).
async fn lost_response_conformance<M1, M2>(first: &M1, second: &M2, neighbor: &[MemoryVersion])
where
    M1: MemoryVersionRedaction + MemoryVersionQueries + ?Sized,
    M2: MemoryVersionRedaction + MemoryVersionQueries + ?Sized,
{
    let versions = MemoryVersionQueries::list_versions(first, "s")
        .await
        .unwrap();
    // R1 retry after the trigger is gone: actor2/clock2 own this redaction.
    let r1 = MemoryVersionRedaction::redact_version_as(
        second,
        "s",
        &versions[0].id,
        Some(&actor("actor-2")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(r1.content.is_none(), "R1 retry clears content");
    assert_eq!(r1.redacted_by, Some(actor("actor-2")), "R1 retry actor");
    assert_eq!(r1.redacted_unix_nanos, Some(CLOCK2), "R1 retry clock");

    // R2: the first redaction of version[1] commits; its response is lost.
    let lost = MemoryVersionRedaction::redact_version_as(
        first,
        "s",
        &versions[1].id,
        Some(&actor("actor-1")),
    )
    .await;
    drop(lost);
    let retried = MemoryVersionRedaction::redact_version_as(
        second,
        "s",
        &versions[1].id,
        Some(&actor("actor-2")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(retried.content.is_none(), "R2 content stays cleared");
    assert_eq!(
        retried.redacted_by,
        Some(actor("actor-1")),
        "R2 original redaction actor kept"
    );
    assert_eq!(
        retried.redacted_unix_nanos,
        Some(CLOCK1),
        "R2 original redaction stamp kept"
    );
    let stored = MemoryVersionQueries::list_versions(first, "s")
        .await
        .unwrap();
    assert_eq!(stored[1], retried, "R2 durable row equals retry receipt");
    assert_eq!(stored[0], r1, "R1 row unchanged by R2");
    assert_eq!(
        stored
            .iter()
            .map(|version| (&version.id, &version.memory_id, &version.path))
            .collect::<Vec<_>>(),
        versions
            .iter()
            .map(|version| (&version.id, &version.memory_id, &version.path))
            .collect::<Vec<_>>(),
        "history identity unchanged"
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(first, "neighbor")
            .await
            .unwrap(),
        neighbor,
        "R3 neighbour untouched"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_file_redaction_survives_trigger_failure_and_lost_response() {
    use awaken_memory_store::SqliteMemoryRepository;
    use sqlx::Connection as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("redaction.db");
    let path = path.to_str().unwrap().to_string();
    let first = SqliteMemoryRepository::open(&path, clock(CLOCK1))
        .await
        .unwrap();
    let before = seed(&first).await;
    let neighbor = first.list_versions("neighbor").await.unwrap();

    let mut admin = sqlx::SqliteConnection::connect(&format!("sqlite://{path}"))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER t25_fail_redaction BEFORE UPDATE ON memory_store_versions \
         BEGIN SELECT RAISE(ABORT, 'injected redaction failure'); END",
    )
    .execute(&mut admin)
    .await
    .unwrap();
    let failed = first
        .redact_version_as("s", &before[0].id, Some(&actor("actor-1")))
        .await;
    assert!(
        matches!(failed, Err(MemErr::Storage(_))),
        "R1 classified: {failed:?}"
    );
    assert_unredacted(&first, &before).await;
    sqlx::query("DROP TRIGGER t25_fail_redaction")
        .execute(&mut admin)
        .await
        .unwrap();
    admin.close().await.unwrap();

    let second = SqliteMemoryRepository::open(&path, clock(CLOCK2))
        .await
        .unwrap();
    assert_unredacted(&second, &before).await;
    lost_response_conformance(&first, &second, &neighbor).await;
    drop(first);
    drop(second);
    let reopened = SqliteMemoryRepository::open(&path, clock(CLOCK2))
        .await
        .unwrap();
    let versions = reopened.list_versions("s").await.unwrap();
    assert_eq!(versions[0].redacted_by, Some(actor("actor-2")), "reopen R1");
    assert_eq!(versions[1].redacted_by, Some(actor("actor-1")), "reopen R2");
    assert!(versions.iter().all(|version| version.content.is_none()));
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_memory_store::PostgresMemoryRepository;
    use sqlx::Executor;
    use sqlx::postgres::{PgConnectOptions, PgPool};

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

    #[tokio::test]
    async fn postgres_redaction_survives_trigger_failure_and_lost_response() {
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
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
        let schema = format!("t_memory_redact_retry_{}_{nonce:x}", std::process::id());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        admin.close().await;

        let pool1 = connect(&url, &schema).await;
        let first = PostgresMemoryRepository::with_pool(pool1.clone(), clock(CLOCK1));
        first.ensure_schema().await.unwrap();
        let before = seed(&first).await;
        let neighbor = first.list_versions("neighbor").await.unwrap();
        pool1
            .execute(
                "CREATE FUNCTION t25_fail_redaction() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RAISE EXCEPTION 'injected redaction failure'; END $$",
            )
            .await
            .unwrap();
        pool1
            .execute(
                "CREATE TRIGGER t25_fail_redaction BEFORE UPDATE ON memory_store_versions \
                 FOR EACH ROW EXECUTE FUNCTION t25_fail_redaction()",
            )
            .await
            .unwrap();
        let failed = first
            .redact_version_as("s", &before[0].id, Some(&actor("actor-1")))
            .await;
        assert!(
            matches!(failed, Err(MemErr::Storage(_))),
            "R1 classified: {failed:?}"
        );
        assert_unredacted(&first, &before).await;
        pool1
            .execute("DROP TRIGGER t25_fail_redaction ON memory_store_versions")
            .await
            .unwrap();

        let pool2 = connect(&url, &schema).await;
        let second = PostgresMemoryRepository::with_pool(pool2.clone(), clock(CLOCK2));
        assert_unredacted(&second, &before).await;
        lost_response_conformance(&first, &second, &neighbor).await;
        pool1.close().await;
        pool2.close().await;
        let reopened = PostgresMemoryRepository::with_existing_pool(
            connect(&url, &schema).await,
            clock(CLOCK2),
        )
        .await
        .unwrap();
        let versions = reopened.list_versions("s").await.unwrap();
        assert_eq!(versions[0].redacted_by, Some(actor("actor-2")), "reopen R1");
        assert_eq!(versions[1].redacted_by, Some(actor("actor-1")), "reopen R2");
        assert!(versions.iter().all(|version| version.content.is_none()));
    }

    #[test]
    fn postgres_redaction_survives_trigger_failure_and_lost_response_fixture_admission() {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::postgres_redaction_survives_trigger_failure_and_lost_response",
        );
    }
}
