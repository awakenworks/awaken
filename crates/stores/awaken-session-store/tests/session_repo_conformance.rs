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

/// A raw fixture connection with SQLite's plain defaults (file created when
/// missing, foreign keys off, journal mode untouched), outside the store policy.
async fn raw_sqlite(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .foreign_keys(false),
        )
        .await
        .expect("open raw SQLite fixture connection")
}

fn block<F: std::future::Future>(f: F) -> F::Output {
    // The SQLite pool bounds connection admission with a Tokio timer.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

async fn owned_postgres_pool(url: &str, tag: &str) -> (sqlx::PgPool, String) {
    let admin = PgPoolOptions::new()
        .max_connections(1)
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
        .max_connections(4)
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

#[tokio::test]
async fn postgres_backend_conforms() {
    // Apply the entire existing Session/Deployment port suite to PostgreSQL,
    // as sqlite_backend_conforms does. A smaller CAS-only suite cannot qualify
    // the missing lifecycle, outbox, scheduler and ownership rules.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixtures = tokio::sync::Mutex::new(Vec::new());
    run_suite(|| async {
        let (pool, schema) = owned_postgres_pool(&url, "conformance").await;
        let repo = awaken_session_store::PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .expect("PostgreSQL Session repository migrations");
        fixtures.lock().await.push((pool, schema));
        repo
    })
    .await;
    for (pool, schema) in fixtures.into_inner() {
        close_postgres_pool(pool, &schema).await;
    }
}

#[test]
fn postgres_fixture_absence_and_configured_failure_are_distinct() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract("postgres_backend_conforms");
}

#[test]
fn sqlite_backend_conforms() {
    // Stack rule S1: the complete original SQLite suite must run on the default
    // libtest stack. The real red overflowed while the inline root-CAS phase
    // polled materialization then legacy root decoding; GDB measured an 868KB
    // run_suite frame. Its owned phase is now pinned on the heap. Every row and
    // following race/reopen assertion remains in this same original entry.
    // S2 PostgreSQL exercises the same run_suite above; no stack override applies.
    block(async {
        run_suite(|| async {
            SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("sqlite in-memory repo")
        })
        .await;
        create_receipt_race_decision_table(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("sqlite create-race repo"),
        ))
        .await;
        concurrent_phase_transition_has_one_count_snapshot(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("sqlite phase-count repo"),
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
            .await
            .expect("left SQLite Deployment replica");
        let right = SqliteManagedSessionRepository::open(database_path)
            .await
            .expect("right SQLite Deployment replica");
        let third = SqliteManagedSessionRepository::open(database_path)
            .await
            .expect("third SQLite Deployment replica");
        deployment_cas_decision_table(&left, [&left, &right, &third]).await;
        drop(third);
        drop(right);
        drop(left);
        let reopened = SqliteManagedSessionRepository::open(database_path)
            .await
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
fn sqlite_backend_conforms_with_bounded_polling_stack() {
    // Keep the original default-stack entry above and run the same complete
    // backend laws under a smaller budget to detect growth of native poll frames.
    std::thread::Builder::new()
        .name("sqlite-conformance".into())
        .stack_size(1024 * 1024)
        .spawn(sqlite_backend_conforms)
        .expect("spawn bounded SQLite conformance")
        .join()
        .expect("bounded SQLite conformance")
}

#[test]
fn sqlite_unbound_preparation_demand_uses_one_shared_capacity_slot() {
    block(async {
        // Backend S1 runs shared R1-R3 over a fresh SQLite authority so a
        // prior conformance table cannot mask first-placement demand.
        let repo = SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap();
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
            .await
            .expect("open SQLite lifecycle atomicity repository");
        lifecycle_outbox_insert_failure_rolls_back_root_paths(&repo, || async {
            let injector = raw_sqlite(&database).await;
            sqlx::raw_sql(
                "CREATE TRIGGER reject_lifecycle_outbox_insert \
                 BEFORE INSERT ON managed_lifecycle_outbox \
                 BEGIN \
                   SELECT RAISE(ABORT, 'injected lifecycle outbox failure'); \
                 END;",
            )
            .execute(&injector)
            .await
            .expect("install SQLite lifecycle outbox rejection");
            injector.close().await;
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
            .await
            .expect("initialize old-record compatibility database");
        let old_record = include_str!("fixtures/deployment-record-v0-managed-wire.json");

        let fixture = raw_sqlite(&database).await;
        sqlx::query(
            "INSERT INTO managed_deployment \
             (deployment_id, workspace_id, revision, data) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind("depl_upgrade_fixture")
        .bind("workspace_upgrade_fixture")
        .bind(1_i64)
        .bind(old_record)
        .execute(&fixture)
        .await
        .expect("insert exact old-release Deployment document");
        fixture.close().await;

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

        let inspector = raw_sqlite(&database).await;
        let stored: String =
            sqlx::query_scalar("SELECT data FROM managed_deployment WHERE deployment_id=?1")
                .bind("depl_upgrade_fixture")
                .fetch_one(&inspector)
                .await
                .expect("rewritten Deployment document");
        inspector.close().await;
        let stored: serde_json::Value =
            serde_json::from_str(&stored).expect("valid rewritten Deployment JSON");
        assert_eq!(stored["initial_events"][0]["type"], "user_message");

        let reopened = SqliteManagedSessionRepository::open(database_path)
            .await
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
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_repository_contract::{SessionReconciliation as _, SessionRepository as _};
    use awaken_session_state_contract::{
        SessionEnvironmentPhase, SessionEnvironmentState, SessionWorkerObligation,
    };
    use awaken_session_store::PostgresManagedSessionRepository;

    // Causes: C1 10k..1M otherwise idle, canonical Session roots retain one
    // physical owner; C2 their root-derived index rows have exact revisions;
    // C3 the old paged/full-decode algorithm, indexed aggregate and cold Server
    // audit read the same PostgreSQL schema; C4 the complete global Environment
    // count streams every root to EOF. Effects: P1=C1+C2+C3 -> all three
    // agree on one owner; P2 prints separate reference/index/startup times and
    // process high-water RSS; P3=C4 -> roots+1 exact Unmaterialized count,
    // including the template, with separate streaming time. No recovery page
    // limit or index-only zero substitutes for this complete decode barrier.
    // This observation has no universal latency pass
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
    assert!(matches!(
        template.environment,
        SessionEnvironmentState::Unmaterialized
    ));
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
        let page = repo.reconciliation_scan(cursor.as_ref()).await.unwrap();
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
    let count_started = std::time::Instant::now();
    let global_count = repo
        .count_environment_phase(SessionEnvironmentPhase::Unmaterialized)
        .await
        .unwrap();
    let count_ms = count_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(
        global_count,
        u64::try_from(rows).unwrap().checked_add(1).unwrap(),
        "P3 complete canonical-root count"
    );
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
        "session_capacity roots={rows} reference_ms={reference_ms:.3} indexed_ms={indexed_ms:.3} startup_ms={startup_ms:.3} count_ms={count_ms:.3} global_count={global_count} high_water_kib={high_water_kib} owners={indexed}"
    );
    drop(repo);
    close_postgres_pool(pool, &schema).await;
}

#[test]
fn root_cas_postgres_fixture_absence_and_configured_failure_are_distinct() {
    // Child rule table against the exact Session CAS backend: R1 optional
    // missing => visible skip; R2 required missing => failure; R3 required
    // empty/unreachable => failure without skip; R4 required live => success
    // under the PostgreSQL gate. The shared testkit isolates both inherited
    // inputs; the domain-owned schema and CAS assertions remain in the child.
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_root_cas_conforms_to_the_same_decision_table",
    );
}

#[path = "session_repo_conformance/mcp_candidate_recovery.rs"]
mod mcp_candidate_recovery;
