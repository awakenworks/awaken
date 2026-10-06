// @formal evidence credential.managed_rollout_abstract_ack_requires_every_target_adoption
// @formal evidence deployment.cas_prevents_lost_update
// @formal evidence deployment.claim_and_run_commit_atomically
// @formal evidence deployment.scheduled_capacity_is_transactional
// @formal evidence deployment.stale_scheduler_cannot_claim_or_overwrite
// @formal evidence session.delete_tombstone_requires_hidden_terminal_verified_cleanup
// @formal evidence session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal evidence session.owner_saved_atomically
// @formal evidence session.visible_row_always_owned
// @formal evidence session_creation.atomic_complete_root_precedes_every_projection
// @formal evidence session_event_batch.final_non_admission_excludes_late_acceptance
// @formal evidence session_resources.restart_retries_same_pending_revision
// @formal evidence webhook.delivery_requires_an_enqueued_fact
// @formal evidence webhook.failed_delivery_remains_pending
// @formal evidence webhook.lifecycle_commit_and_outbox_are_atomic
// @formal evidence webhook.outbox_fact_identity_is_stable
// @formal evidence webhook.periodic_retry_never_retires_a_failed_fact
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
use sqlx::Executor as _;
use sqlx::postgres::PgPoolOptions;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

async fn owned_postgres_pool(url: &str, tag: &str) -> (sqlx::PgPool, String) {
    let admin = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .expect("explicit Session PostgreSQL fixture must connect");
    let schema = format!("t_session_{tag}_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create owned Session schema");
    admin.close().await;
    let owned_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let schema = owned_schema.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("connect owned Session schema");
    (pool, schema)
}

async fn close_postgres_pool(pool: sqlx::PgPool, schema: &str) {
    pool.execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop exact owned Session schema");
    pool.close().await;
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
fn sqlite_unbound_preparation_demand_uses_one_shared_capacity_slot() {
    block(async {
        // Backend S1 runs shared R1-R3 over a fresh SQLite authority so a
        // prior conformance table cannot mask first-placement demand.
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        unbound_preparation_demand_decision_table(&repo).await;
    });
}

#[tokio::test]
async fn postgres_unbound_preparation_demand_uses_one_shared_capacity_slot() {
    // Backend P1 runs the same R1-R3 against an isolated real PostgreSQL
    // schema. An unset explicit fixture URL is the existing visible skip.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let (pool, schema) = owned_postgres_pool(&url, "unbound").await;
    let repo = awaken_session_store::PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .unwrap();
    unbound_preparation_demand_decision_table(&repo).await;
    drop(repo);
    close_postgres_pool(pool, &schema).await;
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

    // Fixture decision table: F1 no explicit URL -> canonical visible skip;
    // F2 configured connection/setup failure -> fail; F3 concurrent callers
    // -> isolated random schemas, no deletion of a shared predecessor. The
    // existing D1-D9 CAS/lifecycle/context rules below remain unchanged.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    assert!(
        !url.trim().is_empty(),
        "explicit Session PostgreSQL URL is empty"
    );
    let (pool, schema) = owned_postgres_pool(&url, "root_cas").await;
    let repo = std::sync::Arc::new(
        PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .expect("open Postgres Session repository"),
    );
    // PostgreSQL refinement rule D9: C8 three repository clients share one
    // migrated pool/schema and execute D3-D7. Effect E9 includes the same exact
    // capacity winner with rejected-row/fact absence, stale zero-effect and
    // reusable-claim result as SQLite. An absent fixture skips this rule;
    // a configured but unavailable endpoint fails rather than certifying it.
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
    // R1-R3 run on the JSONB adapter as well as SQLite: expired owners remain
    // exact reads, and otherwise idle live owners enter the shared index.
    worker_realization_owner_scan_decision_table(repo.as_ref()).await;
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

    // Startup audit decision table: C1 257 additional canonical roots cross
    // the 256-row verification page; C2 a derived obligation flag on the last
    // root drifts without changing its root revision. R1=C1+!C2 -> a complete
    // read-only reopen; R2=C1+C2 -> reject with the existing rebuild reason;
    // R3=R2+operational normalization -> exact repair and a successful reopen.
    // The page boundary is an integrity oracle, not a latency threshold.
    for index in 0..257 {
        let id = format!("audit-page-{index:03}");
        create_session(repo.as_ref(), "ws-audit", session(&id, &id), Vec::new()).await;
    }
    drop(
        PostgresManagedSessionRepository::with_existing_pool(pool.clone())
            .await
            .expect("R1 bounded complete Server audit"),
    );
    sqlx::query(
        "UPDATE managed_session_reconciliation_work \
         SET unbound_preparation = 1 WHERE session_id = 'audit-page-256'",
    )
    .execute(&pool)
    .await
    .expect("R2 inject late-page index drift");
    let error = match PostgresManagedSessionRepository::with_existing_pool(pool.clone()).await {
        Ok(_) => panic!("R2 late-page index drift must fail closed"),
        Err(error) => error,
    };
    assert!(
        error.contains("operational migration rebuild"),
        "R2: {error}"
    );
    drop(
        PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .expect("R3 operational index repair"),
    );
    drop(
        PostgresManagedSessionRepository::with_existing_pool(pool.clone())
            .await
            .expect("R3 repaired Server audit"),
    );
    close_postgres_pool(pool, &schema).await;
}

#[tokio::test]
#[ignore = "explicit local PostgreSQL capacity observation; seeds many canonical Session roots"]
async fn postgres_session_capacity_reference_and_indexed_observation() {
    use awaken_session_repository_contract::SessionRepository as _;
    use awaken_session_resource_contract::SessionRealizationLease;
    use awaken_session_state_contract::{SessionScanQuery, SessionWorkerObligation};
    use awaken_session_store::PostgresManagedSessionRepository;

    // Causes: C1 10k..1M otherwise idle, canonical Session roots retain one
    // physical owner; C2 their root-derived index rows have exact revisions;
    // C3 the old paged/full-decode algorithm, indexed aggregate and cold Server
    // audit read the same PostgreSQL schema. Effects: P1=C1+C2+C3 -> all three
    // agree on one owner; P2 prints separate reference/index/startup times and
    // process high-water RSS. This observation has no universal latency pass
    // threshold and cannot establish a multi-Cell Agent-capacity claim.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        panic!("explicit capacity run requires AWAKEN_TEST_DATABASE_URL");
    };
    let rows: i64 = std::env::var("AWAKEN_SESSION_PERF_ROOTS")
        .ok()
        .map(|value| value.parse().expect("capacity row count"))
        .unwrap_or(100_000);
    assert!((10_000..=1_000_000).contains(&rows));
    let (pool, schema) = owned_postgres_pool(&url, "capacity").await;
    let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .unwrap();
    let mut template = session("capacity-template", "capacity-template");
    template.resources = Default::default();
    template.mcp = Default::default();
    template.realization = Some(SessionRealizationLease {
        owner: "capacity-owner".into(),
        runtime_incarnation: "capacity-owner:pod".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    });
    create_session(&repo, "ws-capacity", template, Vec::new()).await;
    let encoded: String = sqlx::query_scalar(
        "SELECT aggregate_json FROM managed_session WHERE session_id='capacity-template'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO managed_session(session_id, scope_id, revision, aggregate_json) \
         SELECT 'capacity-' || g, 'ws-capacity', 1, \
                replace($1, 'capacity-template', 'capacity-' || g) \
         FROM generate_series(1, $2::bigint) g",
    )
    .bind(encoded)
    .bind(rows)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO managed_session_reconciliation_work \
         (session_id, observed_revision, worker_owner, preparation_candidate, \
          unbound_preparation, unbound_terminal, demand_invalid) \
         SELECT 'capacity-' || g, 1, 'capacity-owner', 0, 0, 0, 0 \
         FROM generate_series(1, $1::bigint) g",
    )
    .bind(rows)
    .execute(&pool)
    .await
    .unwrap();

    let reference_started = std::time::Instant::now();
    let mut cursor = None;
    let mut owners = std::collections::HashSet::new();
    let mut unbound = false;
    loop {
        let page = repo
            .scan(SessionScanQuery::Reconcilable {
                after: cursor.as_ref(),
            })
            .await
            .unwrap();
        assert!(page.quarantined.is_empty(), "P1 canonical fixture");
        for scoped in page.sessions {
            match scoped.session.worker_obligation().unwrap() {
                SessionWorkerObligation::RetainedOwner(owner) => {
                    owners.insert(owner.to_owned());
                }
                SessionWorkerObligation::UnboundInitialPreparation
                | SessionWorkerObligation::UnboundTerminalCleanup => unbound = true,
                SessionWorkerObligation::None => {}
            }
        }
        let Some(next) = page.next_cursor else { break };
        cursor = Some(next);
    }
    let reference_ms = reference_started.elapsed().as_secs_f64() * 1_000.0;
    let indexed_started = std::time::Instant::now();
    let indexed = repo.worker_demand().await.unwrap();
    let indexed_ms = indexed_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(indexed, owners.len() as u64 + u64::from(unbound), "P1");
    let startup_started = std::time::Instant::now();
    drop(
        PostgresManagedSessionRepository::with_existing_pool(pool.clone())
            .await
            .unwrap(),
    );
    let startup_ms = startup_started.elapsed().as_secs_f64() * 1_000.0;
    let high_water_kib = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or(0);
    println!(
        "session_capacity roots={rows} reference_ms={reference_ms:.3} indexed_ms={indexed_ms:.3} startup_ms={startup_ms:.3} high_water_kib={high_water_kib} owners={indexed}"
    );
    drop(repo);
    close_postgres_pool(pool, &schema).await;
}

#[test]
fn root_cas_postgres_fixture_absence_and_configured_failure_are_distinct() {
    // Cause/effect table: F1 no URL -> visible SKIPPED with no fallback;
    // F2 explicit empty or unreachable URL -> failed backend process, never
    // a passing skip. The actual CAS backend case runs in a child to avoid
    // changing process-global environment for another Session test.
    let run = |url: Option<&str>| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args([
            "postgres_root_cas_conforms_to_the_same_decision_table",
            "--exact",
            "--nocapture",
        ]);
        child.env_remove("AWAKEN_TEST_DATABASE_URL");
        if let Some(url) = url {
            child.env("AWAKEN_TEST_DATABASE_URL", url);
        }
        child.output().expect("Session fixture rule child")
    };
    let absent = run(None);
    assert!(absent.status.success(), "F1");
    assert!(
        String::from_utf8_lossy(&absent.stderr).contains("SKIPPED PostgreSQL fixture"),
        "F1 visible skip"
    );
    for url in ["", "postgres://127.0.0.1:1/unreachable"] {
        let bad = run(Some(url));
        assert!(!bad.status.success(), "F2 configured failure");
        assert!(
            !String::from_utf8_lossy(&bad.stderr).contains("SKIPPED PostgreSQL fixture"),
            "F2 no false skip"
        );
    }
}
