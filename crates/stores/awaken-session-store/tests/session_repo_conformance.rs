//! Trait-generic **conformance suite** for the `SessionRepository` port — the
//! shared behavioural contract every backend must satisfy (ADR-0059, the
//! `awaken-store-conformance` pattern).
//!
//! Every save also persists an owner scope in the same repository operation. The generic
//! suite checks that universal invariant for both backends; durable restart persistence
//! remains in the backend-specific suite.
//!
//! The suite lives in `awaken_store_conformance::session_store`; this file only
//! supplies the backends.

use awaken_deployment_contract::{DeploymentRepository, DeploymentWriteOutcome};
use awaken_session_repository_contract::LifecycleOutbox as _;
use awaken_session_store::SqliteManagedSessionRepository;
use awaken_store_conformance::session_store::*;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

#[test]
fn sqlite_backend_conforms() {
    block(async {
        run_suite(|| {
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite in-memory repo")
        })
        .await;
        create_receipt_race_decision_table(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite create-race repo"),
        ))
        .await;
        concurrent_phase_transition_has_one_count_snapshot(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite phase-count repo"),
        ))
        .await;
        // SQLite refinement rule D8: C6 three independently opened adapters
        // share one file and execute D3-D7; C7 every handle then closes.
        // Effects: E7 D3/D4 still have one exact CAS/capacity winner; E8 reopen
        // retains the capacity winner/fact, exact revision-three frame and both
        // valid runs, excluding every rejected D4/D6 effect.
        let directory = tempfile::tempdir().expect("SQLite Deployment CAS directory");
        let database = directory.path().join("deployment-cas.db");
        let database_path = database.to_str().expect("UTF-8 Deployment CAS path");
        let left = SqliteManagedSessionRepository::open(database_path)
            .expect("left SQLite Deployment replica");
        let right = SqliteManagedSessionRepository::open(database_path)
            .expect("right SQLite Deployment replica");
        let third = SqliteManagedSessionRepository::open(database_path)
            .expect("third SQLite Deployment replica");
        deployment_cas_decision_table(&left, [&left, &right, &third]).await;
        drop(third);
        drop(right);
        drop(left);
        let reopened = SqliteManagedSessionRepository::open(database_path)
            .expect("reopen SQLite Deployment authority");
        let deployment = reopened
            .deployments()
            .await
            .expect("read reopened Deployments")
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas")
            .expect("reopened CAS winner");
        assert_eq!(deployment.record.revision, 3, "D8 persisted successor");
        assert_eq!(
            deployment.record.name, "fresh-after-stale",
            "D8 persisted frame"
        );
        assert_eq!(
            reopened
                .deployment_runs()
                .await
                .expect("read reopened DeploymentRuns")
                .len(),
            3,
            "D8 only valid claim/runs survived reopen"
        );
        let reopened_facts = reopened
            .pending_lifecycle()
            .await
            .expect("read reopened Deployment lifecycle facts");
        assert!(
            reopened_facts
                .iter()
                .all(|fact| fact.id != "failed-claim-fact"),
            "D8 failed claim fact remained absent"
        );
        assert!(
            reopened_facts.iter().any(|fact| {
                fact.id == "deployment-run-started" && fact.object_id == "drun_current"
            }),
            "D8 retried claim's exact run/fact pair survived reopen"
        );
        let capacity_rows = reopened
            .deployments()
            .await
            .expect("read reopened capacity winner")
            .into_iter()
            .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
            .collect::<Vec<_>>();
        let capacity_facts = reopened_facts
            .into_iter()
            .filter(|fact| fact.id.starts_with("capacity-"))
            .collect::<Vec<_>>();
        assert_eq!(capacity_rows.len(), 1, "D8 one durable capacity winner");
        assert_eq!(capacity_facts.len(), 1, "D8 no rejected capacity fact");
        assert_eq!(
            capacity_facts[0].object_id, capacity_rows[0].id,
            "D8 exact durable capacity row/fact pair"
        );
    });
}

#[test]
fn sqlite_lifecycle_outbox_insert_failure_rolls_back_every_root_path() {
    block(async {
        // Backend rule S1: run A1-A3 against a file-backed production adapter;
        // a SQLite BEFORE INSERT abort is the C3 database failure, and every
        // subsequent assertion reads through the same repository contract.
        let directory = tempfile::tempdir().expect("SQLite atomicity directory");
        let database = directory.path().join("lifecycle-atomicity.db");
        let database_path = database.to_str().expect("UTF-8 lifecycle atomicity path");
        let repo = SqliteManagedSessionRepository::open(database_path)
            .expect("open SQLite lifecycle atomicity repository");
        lifecycle_outbox_insert_failure_rolls_back_root_paths(&repo, || {
            rusqlite::Connection::open(&database)
                .expect("open SQLite failure injector")
                .execute_batch(
                    "CREATE TRIGGER reject_lifecycle_outbox_insert \
                     BEFORE INSERT ON managed_lifecycle_outbox \
                     BEGIN \
                       SELECT RAISE(ABORT, 'injected lifecycle outbox failure'); \
                     END;",
                )
                .expect("install SQLite lifecycle outbox rejection");
            std::future::ready(())
        })
        .await;
    });
}

#[test]
fn sqlite_restores_and_canonically_rewrites_the_pre_typed_deployment_record() {
    block(async {
        // This fixture is frozen old-release output, not JSON produced by the
        // current Rust types. It covers the upgrade edge that ordinary
        // write-then-read repository tests cannot exercise.
        let directory = tempfile::tempdir().expect("temporary compatibility database");
        let database = directory.path().join("managed.db");
        let database_path = database.to_str().expect("UTF-8 test path");
        let repo = SqliteManagedSessionRepository::open(database_path)
            .expect("initialize old-record compatibility database");
        let old_record = include_str!("fixtures/deployment-record-v0-managed-wire.json");

        rusqlite::Connection::open(&database)
            .expect("open fixture database")
            .execute(
                "INSERT INTO managed_deployment \
                 (deployment_id, workspace_id, revision, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "depl_upgrade_fixture",
                    "workspace_upgrade_fixture",
                    1_i64,
                    old_record
                ],
            )
            .expect("insert exact old-release Deployment document");

        let mut restored = repo
            .deployments()
            .await
            .expect("old official Managed event spelling must restore")
            .pop()
            .expect("fixture Deployment");
        assert_eq!(restored.record.revision, 1);
        assert_eq!(restored.record.initial_events.len(), 1);

        restored.record.revision = 2;
        restored.record.updated_at = "2026-08-25T00:00:00Z".into();
        assert_eq!(
            repo.write_deployment(restored, Some(1), 10, None)
                .await
                .expect("rewrite restored Deployment"),
            DeploymentWriteOutcome::Applied
        );
        drop(repo);

        let stored: String = rusqlite::Connection::open(&database)
            .expect("inspect rewritten database")
            .query_row(
                "SELECT data FROM managed_deployment WHERE deployment_id=?1",
                ["depl_upgrade_fixture"],
                |row| row.get(0),
            )
            .expect("rewritten Deployment document");
        let stored: serde_json::Value =
            serde_json::from_str(&stored).expect("valid rewritten Deployment JSON");
        assert_eq!(stored["initial_events"][0]["type"], "user_message");

        let reopened = SqliteManagedSessionRepository::open(database_path)
            .expect("reopen compatibility database");
        assert_eq!(
            reopened
                .deployments()
                .await
                .expect("rewritten Deployment survives restart")[0]
                .record
                .revision,
            2
        );
    });
}

#[tokio::test]
async fn postgres_root_cas_conforms_to_the_same_decision_table() {
    use awaken_session_store::PostgresManagedSessionRepository;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    // Fixture admission table: no configured URL + unreachable local default
    // may skip; configured URL + unreachable database must fail the gate.
    let configured = match std::env::var("AWAKEN_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("AWAKEN_TEST_DATABASE_URL is not valid Unicode")
        }
    };
    let url = configured.clone().unwrap_or_else(|| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_owned()
    });
    let admin = match PgPool::connect(&url).await {
        Ok(pool) => pool,
        Err(error) if configured.is_none() => {
            println!("[skip] no Postgres reachable: {error}");
            return;
        }
        Err(error) => panic!("configured AWAKEN_TEST_DATABASE_URL is unreachable: {error}"),
    };
    let _ = admin
        .execute("DROP SCHEMA IF EXISTS t_session_root_cas CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_session_root_cas")
        .await
        .expect("create Session CAS schema");
    admin.close().await;
    let pool = PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection
                    .execute("SET search_path = t_session_root_cas")
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect Session CAS schema");
    let repo = std::sync::Arc::new(
        PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .expect("open Postgres Session repository"),
    );
    // PostgreSQL refinement rule D9: C8 three repository clients share one
    // migrated pool/schema and execute D3-D7. Effect E9 includes the same exact
    // capacity winner with rejected-row/fact absence, stale zero-effect and
    // reusable-claim result as SQLite. An unavailable endpoint skips this
    // entire rule and is never reported as live PostgreSQL evidence.
    let second_deployment_replica = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("open second Postgres Deployment replica");
    let third_deployment_replica = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("open third Postgres Deployment replica");
    global_environment_phase_count_has_no_recovery_batch_limit(repo.as_ref()).await;
    tombstone_requires_hidden_completed_cleanup(repo.as_ref()).await;
    deleted_event_admission_decision_table(repo.as_ref()).await;
    root_cas_decision_table(repo.as_ref()).await;
    create_receipt_decision_table(repo.as_ref()).await;
    create_receipt_race_decision_table(repo.clone()).await;
    concurrent_root_cas_has_one_authoritative_winner(repo.clone()).await;
    concurrent_phase_transition_has_one_count_snapshot(repo.clone()).await;
    credential_source_dependency_decision_table(repo.as_ref()).await;
    vault_reference_index_returns_only_live_scoped_sessions(repo.as_ref()).await;
    scan_query_decision_table(repo.as_ref()).await;
    deployment_cas_decision_table(
        repo.as_ref(),
        [
            repo.as_ref(),
            &second_deployment_replica,
            &third_deployment_replica,
        ],
    )
    .await;
    lifecycle_outbox_tracks_every_committed_transition(repo.as_ref()).await;
    lifecycle_outbox_insert_failure_rolls_back_root_paths(repo.as_ref(), || {
        let pool = pool.clone();
        async move {
            // Backend rule P1: a NOT VALID constraint preserves successful
            // fixture rows but rejects every new outbox INSERT. A1-A3 then
            // observe PostgreSQL's real transaction rollback through the port.
            sqlx::query(
                "ALTER TABLE managed_lifecycle_outbox \
                 ADD CONSTRAINT reject_lifecycle_outbox_insert CHECK (false) NOT VALID",
            )
            .execute(&pool)
            .await
            .expect("install PostgreSQL lifecycle outbox rejection");
        }
    })
    .await;
}
