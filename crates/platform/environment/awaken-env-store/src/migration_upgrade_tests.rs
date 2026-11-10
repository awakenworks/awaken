//! T37-C01 (Environment half): a published V8 authority is upgraded by the
//! PRODUCTION open path on real file SQLite (independent handles + reopen) and
//! on PostgreSQL (independent pools), seeding the exact registration history
//! exactly once.
//!
//! Cause/effect graph: C1 a V8 store holds `env_legacy` with v1..v3, v3 being
//! the terminal archive; C2 the first open applies V9..; C3 a second handle
//! and a reopen observe and re-run migrations. Effects: E1 v1/v2 become pending
//! Register intents and v3 a pending Withdraw (never a Register); E2 the
//! immutable revisions and the archived head are unchanged; E3 re-running the
//! bundle applies nothing and the intent list is identical (no duplicates).
//!
//! Negative control (executed): a store created empty by the current release
//! opens successfully but holds no seeded history, so "it opens" is not
//! evidence of an upgrade -- [`seeded_history_verdict`] rejects it.

use super::*;
use awaken_environment_contract::EnvironmentRegistrationOperation as Operation;

const LEGACY_ROWS: &str = r#"
    INSERT INTO env_registry_env
        (env_id, seq, name, description, metadata_json, config_json,
         archived_at, revision, scope, sandbox_policy_json)
    VALUES ('env_legacy', 0, 'legacy', '', '{}', '{"type":"self_hosted"}',
            '2026-01-01T00:00:00Z', 3, NULL, NULL);
    INSERT INTO env_registry_revision
        (env_id, revision, name, description, metadata_json, config_json,
         archived_at, scope, sandbox_policy_json)
    VALUES
        ('env_legacy', 1, 'v1', '', '{}', '{"type":"self_hosted"}', NULL, NULL, NULL),
        ('env_legacy', 2, 'v2', '', '{}', '{"type":"self_hosted"}', NULL, NULL, NULL),
        ('env_legacy', 3, 'v3', '', '{}', '{"type":"self_hosted"}',
         '2026-01-01T00:00:00Z', NULL, NULL);
"#;

fn legacy_bundle() -> MigrationBundle {
    let full = env_bundle().unwrap();
    MigrationBundle::new(full.bundle_id(), full.migrations()[..8].to_vec()).unwrap()
}

/// E1 + E2 over one handle.
async fn seeded_history_verdict<R>(registry: &R) -> Result<(), String>
where
    R: EnvironmentDefinitionQueries + EnvironmentRegistrationOutbox,
{
    let intents = registry
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .map_err(|error| error.to_string())?;
    let observed: Vec<_> = intents
        .iter()
        .map(|intent| {
            (
                intent.environment_id.as_str(),
                intent.revision.0,
                intent.operation,
                intent.delivered,
            )
        })
        .collect();
    let expected = [
        ("env_legacy", 1, Operation::Register, false),
        ("env_legacy", 2, Operation::Register, false),
        ("env_legacy", 3, Operation::Withdraw, false),
    ];
    if observed != expected {
        return Err(format!("E1 seeded intents {observed:?}"));
    }
    for (revision, name) in [(1, "v1"), (2, "v2"), (3, "v3")] {
        let item = registry
            .get_revision("env_legacy", EnvironmentRevision(revision))
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("E2 revision {revision} missing"))?;
        if item.name != name || item.archived_at.is_some() != (revision == 3) {
            return Err(format!("E2 revision {revision} drifted: {item:?}"));
        }
    }
    let head = registry
        .get("env_legacy")
        .await
        .map_err(|error| error.to_string())?
        .ok_or("E2 head missing")?;
    if head.revision != EnvironmentRevision(3) || head.archived_at.is_none() {
        return Err(format!("E2 head drifted: {head:?}"));
    }
    Ok(())
}

#[tokio::test]
async fn file_sqlite_v8_upgrade_seeds_history_once_across_handles_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v8.db");
    let path = path.to_str().unwrap();
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
        .unwrap()
        .run_bundle(&legacy_bundle())
        .await
        .unwrap();
    sqlx::raw_sql(LEGACY_ROWS)
        .execute(pool.sqlx())
        .await
        .unwrap();
    pool.close().await;

    let upgraded = SqliteEnvRegistry::open(path, system_clock())
        .await
        .expect("C2 production open upgrades V8");
    let peer = SqliteEnvRegistry::open(path, system_clock())
        .await
        .expect("C3 independent handle");
    for handle in [&upgraded, &peer] {
        seeded_history_verdict(handle)
            .await
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
    let before = peer
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .unwrap();
    drop((upgraded, peer));
    let reopened = SqliteEnvRegistry::open(path, system_clock())
        .await
        .expect("reopen");
    assert!(
        SqliteMigrationRunner::with_prefix(reopened.pool.sqlx().clone(), NS)
            .unwrap()
            .run_bundle(&env_bundle().unwrap())
            .await
            .unwrap()
            .is_empty(),
        "E3 nothing reapplies"
    );
    assert_eq!(
        reopened
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap(),
        before,
        "E3 no duplicated seed"
    );

    // NC: an empty current-release store opens but has no upgraded history.
    let fresh = SqliteEnvRegistry::open(
        directory.path().join("fresh.db").to_str().unwrap(),
        system_clock(),
    )
    .await
    .unwrap();
    assert!(
        seeded_history_verdict(&fresh).await.is_err(),
        "NC: an empty schema cannot satisfy the upgrade oracle"
    );
}

#[tokio::test]
async fn postgres_v8_upgrade_seeds_history_once_across_independent_pools() {
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
    let schema = format!("t_env_v8_upgrade_{}_{nonce:x}", std::process::id());
    admin
        .execute(
            // The schema is a fixed fixture prefix plus integer process/time coordinates.
            sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")),
        )
        .await
        .unwrap();
    let connect = || {
        let options = url
            .parse::<sqlx::postgres::PgConnectOptions>()
            .unwrap()
            .options([("search_path", schema.as_str())]);
        async move {
            awaken_sql_storage::postgres_pool_options()
                .max_connections(2)
                .acquire_timeout(std::time::Duration::from_secs(3))
                .connect_with(options)
                .await
                .unwrap()
        }
    };
    let legacy_pool = connect().await;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
        legacy_pool.clone(),
        NS,
    )
    .unwrap()
    .run_bundle(&legacy_bundle())
    .await
    .unwrap();
    sqlx::raw_sql(LEGACY_ROWS)
        .execute(&legacy_pool)
        .await
        .unwrap();
    legacy_pool.close().await;

    let upgraded = PostgresEnvRegistry::with_pool(connect().await, system_clock())
        .await
        .expect("C2 production migration upgrades V8");
    let peer = PostgresEnvRegistry::with_existing_pool(connect().await, system_clock())
        .await
        .expect("C3 independent pool verifies the ledger");
    for handle in [&upgraded, &peer] {
        seeded_history_verdict(handle)
            .await
            .unwrap_or_else(|failure| panic!("{failure}"));
    }
    let before = peer
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .unwrap();
    upgraded.pool.close().await;
    peer.pool.close().await;
    let replay_pool = connect().await;
    assert!(
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            replay_pool.clone(),
            NS
        )
        .unwrap()
        .run_bundle(&env_bundle().unwrap())
        .await
        .unwrap()
        .is_empty(),
        "E3 nothing reapplies"
    );
    let reopened = PostgresEnvRegistry::with_existing_pool(replay_pool, system_clock())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap(),
        before,
        "E3 no duplicated seed"
    );
    reopened.pool.close().await;
    admin
        .execute(
            // The schema is a fixed fixture prefix plus integer process/time coordinates.
            sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")),
        )
        .await
        .expect("drop only this run's schema");
    admin.close().await;
}
