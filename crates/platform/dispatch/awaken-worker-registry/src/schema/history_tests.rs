// @formal evidence worker.incarnation_generation_fences_stale_processes
//! The production migration is one portable SQL body for both durable backends.
use super::*;
use crate::{PostgresWorkerDirectory, SqliteWorkerDirectory};
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedSqlite;
use awaken_worker_contract::WorkerCurrentSource;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, WorkerDirectory, WorkerIdentity, WorkerManifest,
    WorkerRegistration, WorkerSnapshot, WorkerState,
};

fn current_manifest() -> WorkerManifest {
    WorkerManifest {
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        ..WorkerManifest::default()
    }
}

fn expected(generation: u64) -> RegisteredWorker {
    let manifest = current_manifest();
    RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: WorkerIdentity::new("migrated", "A", generation),
            state: WorkerState::Dead,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 2,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: 150,
        },
        heartbeat_sequence: 3,
        observation_sequence: 4,
        registered_at_ms: 100,
        heartbeat_at_ms: 101,
        drain_deadline_ms: Some(120),
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    }
}

fn seed_sql(legacy_conflict: bool) -> &'static str {
    // SQLite also supports the named $1/$2/$3 parameters. The old writer shape
    // is a closed alternative; fixture values never enter SQL text.
    if legacy_conflict {
        "INSERT INTO worker_registry_worker (
         worker_id, incarnation_id, generation, state, manifest_json,
         capability_fingerprint, in_flight, warm_environment_shapes_json,
         credential_observations_json, acp_capability_observations_json,
         expires_at_ms, heartbeat_sequence, observation_sequence,
         registered_at_ms, heartbeat_at_ms, drain_deadline_ms) VALUES \
        ('migrated','A',$1,'dead',$2,$3,2,'[]','[]','[]',150,3,4,100,101,120) \
        ON CONFLICT(worker_id) DO NOTHING"
    } else {
        "INSERT INTO worker_registry_worker (
         worker_id, incarnation_id, generation, state, manifest_json,
         capability_fingerprint, in_flight, warm_environment_shapes_json,
         credential_observations_json, acp_capability_observations_json,
         expires_at_ms, heartbeat_sequence, observation_sequence,
         registered_at_ms, heartbeat_at_ms, drain_deadline_ms) VALUES \
        ('migrated','A',$1,'dead',$2,$3,2,'[]','[]','[]',150,3,4,100,101,120)"
    }
}

async fn sqlite_seed(
    connection: &awaken_sqlite_pool::EmbeddedPool,
    generation: u64,
    legacy_conflict: bool,
) -> Result<(), sqlx::Error> {
    let row = expected(generation);
    sqlx::query(seed_sql(legacy_conflict))
        .bind(i64::try_from(generation).unwrap())
        .bind(serde_json::to_string(&row.snapshot.manifest).unwrap())
        .bind(row.snapshot.capability_fingerprint)
        .execute(connection)
        .await
        .map(|_| ())
}

async fn postgres_seed(
    pool: &sqlx::PgPool,
    generation: u64,
    legacy_conflict: bool,
) -> Result<(), sqlx::Error> {
    let row = expected(generation);
    sqlx::query(seed_sql(legacy_conflict))
        .bind(i64::try_from(generation).unwrap())
        .bind(serde_json::to_string(&row.snapshot.manifest).unwrap())
        .bind(row.snapshot.capability_fingerprint)
        .execute(pool)
        .await
        .map(|_| ())
}

async fn assert_migrated(directory: &dyn WorkerDirectory, generation: u64) {
    // M1 either published history + generation 1 => complete retained history;
    // M2 either history + generation >1 => lost predecessors, never invent them.
    // E1 exact old record, observation/drain data and tombstone preserved;
    // E2 same A retry is exact, not renewed; E3 fresh B allowed only for M1;
    // E4 M2 denies new generation and preserves authority; E5 a new Worker id
    // is independent and valid. M3 reopen is idempotent (driver below).
    let original = expected(generation);
    assert_eq!(
        directory.current("migrated").await.unwrap(),
        Some(original.clone()),
        "M1/M2 E1"
    );
    assert_eq!(
        directory.list().await.unwrap(),
        vec![original.clone()],
        "current projection"
    );
    let registration = |worker: &str, incarnation: &str| WorkerRegistration {
        worker_id: worker.into(),
        incarnation_id: incarnation.into(),
        manifest: current_manifest(),
        create_ledger: None,
    };
    assert_eq!(
        directory
            .register(registration("migrated", "A"), 500, 500)
            .await
            .unwrap(),
        original,
        "E2"
    );
    let result = directory
        .register(registration("migrated", "B"), 500, 50)
        .await;
    if generation == 1 {
        assert_eq!(result.unwrap().snapshot.identity.generation, 2, "M1 E3");
        assert!(
            matches!(
                directory
                    .register(registration("migrated", "A"), 600, 50)
                    .await,
                Err(RegistryError::HistoricalIncarnation { .. })
            ),
            "retained A is historical"
        );
    } else {
        assert!(
            matches!(
                result,
                Err(RegistryError::IncompleteIncarnationHistory { .. })
            ),
            "M2 E4"
        );
        assert_eq!(
            directory.current("migrated").await.unwrap(),
            Some(original),
            "M2 unchanged"
        );
    }
    assert_eq!(
        directory
            .register(registration("new-slot", "A"), 500, 50)
            .await
            .unwrap()
            .snapshot
            .identity
            .generation,
        1,
        "E5"
    );
}

#[cfg(test)]
#[tokio::test]
async fn sqlite_retained_history_migrates_both_published_streams() {
    // M1/M2 x legacy-JSON/relational terminal schemas, followed by real close and
    // reopen. Constraints M4 old ON CONFLICT(worker_id) writers must fail;
    // M5 one incarnation cannot appear in two generations. No checksum rewrite.
    // M6 obstruct the temporary table: migration fails after DROP INDEX, then
    // rollback restores that index and retains the old row/receipt. Removing
    // only the obstruction lets the same production startup finish normally.
    for published in [
        registry_bundle().unwrap(),
        relational_registry_bundle().unwrap(),
    ] {
        for generation in [1, 7] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("worker.sqlite");
            let conn = EmbeddedSqlite::file(&path).open().await.unwrap();
            let runner = SqliteMigrationRunner::with_prefix(conn.sqlx().clone(), NS).unwrap();
            runner.run_bundle(&published).await.unwrap();
            sqlite_seed(&conn, generation, false).await.unwrap();
            sqlx::raw_sql("CREATE TABLE worker_registry_worker_history (obstruction INTEGER)")
                .execute(&conn)
                .await
                .unwrap();
            conn.close().await;
            assert!(
                SqliteWorkerDirectory::open(&path).await.is_err(),
                "M6 migration failure"
            );
            let conn = EmbeddedSqlite::file(&path).open().await.unwrap();
            let indices: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='index' AND name='worker_registry_expiry_idx'").fetch_one(&conn).await.unwrap();
            assert_eq!(indices, 1, "M6 DDL prefix rolled back");
            let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM worker_registry_schema_migrations WHERE bundle_id=?1 AND version=2").bind(CONVERGED_BUNDLE_ID).fetch_one(&conn).await.unwrap();
            assert_eq!(receipts, 0, "M6 failed migration has no receipt");
            sqlx::raw_sql("DROP TABLE worker_registry_worker_history")
                .execute(&conn)
                .await
                .unwrap();
            conn.close().await;
            let store = SqliteWorkerDirectory::open(&path).await.unwrap();
            assert_migrated(&store, generation).await;
            let before = store.current("migrated").await.unwrap();
            drop(store);
            let reopened = SqliteWorkerDirectory::open(&path).await.unwrap();
            assert_eq!(
                reopened.current("migrated").await.unwrap(),
                before,
                "M3 reopen"
            );
            drop(reopened);
            let conn = EmbeddedSqlite::file(&path).open().await.unwrap();
            let runner = SqliteMigrationRunner::with_prefix(conn.sqlx().clone(), NS).unwrap();
            assert!(
                sqlite_seed(&conn, 99, true).await.is_err(),
                "M4 old writer cannot overwrite history"
            );
            assert!(
                sqlite_seed(&conn, 99, false).await.is_err(),
                "M5 incarnation unique across generations"
            );
            assert!(
                runner.run_bundle(&published).await.unwrap().is_empty(),
                "published receipts unchanged"
            );
            assert!(
                runner
                    .run_bundle(&converged_registry_bundle().unwrap())
                    .await
                    .unwrap()
                    .is_empty(),
                "M3 no DDL replay"
            );
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn postgres_retained_history_migrates_both_published_streams() {
    // The same M1-M5 matrix executes actual PostgreSQL DDL after both published
    // streams. Missing URL is an explicit local skip; configured failure is red.
    // The strict PG gate supplies the URL and runs this library test explicitly.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    for (stream, published) in [
        registry_bundle().unwrap(),
        relational_registry_bundle().unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        for generation in [1, 7] {
            let schema = format!(
                "registry_history_{}_{}_{}",
                std::process::id(),
                stream,
                generation
            );
            // schema contains only a fixed prefix and the integer process,
            // stream and generation coordinates from the fixture above.
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&admin)
                .await
                .unwrap();
            let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect_with(options.options([("search_path", schema.as_str())]))
                .await
                .unwrap();
            let actual: String = sqlx::query_scalar("SELECT current_schema()")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(actual, schema, "never seed public schema");
            let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
                pool.clone(),
                NS,
            )
            .unwrap();
            runner.run_bundle(&published).await.unwrap();
            postgres_seed(&pool, generation, false).await.unwrap();
            // M6 same failing DDL prefix and retry under actual PG transactions.
            sqlx::query("CREATE TABLE worker_registry_worker_history (obstruction INTEGER)")
                .execute(&pool)
                .await
                .unwrap();
            assert!(
                PostgresWorkerDirectory::with_pool(pool.clone())
                    .await
                    .is_err(),
                "M6 migration failure"
            );
            let index: Option<String> =
                sqlx::query_scalar("SELECT to_regclass('worker_registry_expiry_idx')::text")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(index.is_some(), "M6 index restored");
            let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM worker_registry_schema_migrations WHERE bundle_id=$1 AND version=2").bind(CONVERGED_BUNDLE_ID).fetch_one(&pool).await.unwrap();
            assert_eq!(receipts, 0, "M6 no false receipt");
            sqlx::query("DROP TABLE worker_registry_worker_history")
                .execute(&pool)
                .await
                .unwrap();
            let store = PostgresWorkerDirectory::with_pool(pool.clone())
                .await
                .unwrap();
            assert_migrated(&store, generation).await;
            let before = store.current("migrated").await.unwrap();
            drop(store);
            let reopened = PostgresWorkerDirectory::with_existing_pool(pool.clone())
                .await
                .unwrap();
            assert_eq!(
                reopened.current("migrated").await.unwrap(),
                before,
                "M3 verify-only reopen"
            );
            assert!(
                postgres_seed(&pool, 99, true).await.is_err(),
                "M4 old writer"
            );
            assert!(
                postgres_seed(&pool, 99, false).await.is_err(),
                "M5 incarnation uniqueness"
            );
            assert!(runner.run_bundle(&published).await.unwrap().is_empty());
            assert!(
                runner
                    .run_bundle(&converged_registry_bundle().unwrap())
                    .await
                    .unwrap()
                    .is_empty()
            );
            drop(reopened);
            drop(runner);
            pool.close().await;
            // Same generated integer-only identifier as the CREATE above.
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                .execute(&admin)
                .await
                .unwrap();
        }
    }
    admin.close().await;
}
