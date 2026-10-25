//! T21-C02: historical delivery and a new registration stay isolated across a
//! process rebuild, with DURABLE definitions and Work on file SQLite and
//! PostgreSQL.
//!
//! Cause/effect graph (process 1): C1 create commits v1 but its delivery
//! acknowledgement is lost (a real SQL trigger rejects the `delivered`
//! UPDATE); C2 v2 (Cloud) is delivered; C3 v3 (SelfHosted) commits while its
//! registration fails; C4 a drain restores v1 as history and fails v3.
//! Process 2 (every handle reopened, a cold catalog): C5 the drain is retried
//! and then replayed. Effects: E1 the historical success never consumes the
//! unapplied v3 (the Work watermark stays v2); E2 the failure responsibility
//! (pending v1 + v3) is retained durably across the rebuild; E3 the retry
//! converges to exactly one v3 Work effect, and a replay adds nothing.
//!
//! Negative control (executed): a delivery path that advances only the
//! catalog high-watermark (registering into the catalog without its Work
//! projection) reaches catalog revision 3 yet is rejected by
//! [`converged_verdict`], which checks the Work effect.

use super::*;
use awaken_env_store::{InMemoryEnvRegistry, PostgresEnvRegistry, SqliteEnvRegistry};
use awaken_executable_environment_contract::{
    EnvironmentWorkRevisionOperation as Operation, EnvironmentWorkRevisionOutcome as Outcome,
};

/// The real fault that loses an acknowledgement after delivery succeeded.
enum AckFault {
    Volatile(Arc<InMemoryEnvRegistry>),
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

impl AckFault {
    async fn arm(&self) {
        match self {
            Self::Volatile(registry) => registry.fail_next_acknowledgements(1),
            Self::Sqlite(pool) => {
                sqlx::raw_sql(
                    "CREATE TRIGGER t21_lose_ack BEFORE UPDATE ON env_registry_registration_intent \
                     BEGIN SELECT RAISE(ABORT, 'injected acknowledgement loss'); END;",
                )
                .execute(pool)
                .await
                .unwrap();
            }
            Self::Postgres(pool) => {
                sqlx::raw_sql(
                    "CREATE FUNCTION t21_lose_ack() RETURNS trigger LANGUAGE plpgsql AS \
                     $$ BEGIN RAISE EXCEPTION 'injected acknowledgement loss'; END $$; \
                     CREATE TRIGGER t21_lose_ack BEFORE UPDATE ON env_registry_registration_intent \
                     FOR EACH ROW EXECUTE FUNCTION t21_lose_ack();",
                )
                .execute(pool)
                .await
                .unwrap();
            }
        }
    }

    async fn disarm(&self) {
        match self {
            Self::Volatile(_) => {}
            Self::Sqlite(pool) => {
                sqlx::raw_sql("DROP TRIGGER t21_lose_ack")
                    .execute(pool)
                    .await
                    .unwrap();
            }
            Self::Postgres(pool) => {
                sqlx::raw_sql(
                    "DROP TRIGGER t21_lose_ack ON env_registry_registration_intent; \
                     DROP FUNCTION t21_lose_ack();",
                )
                .execute(pool)
                .await
                .unwrap();
            }
        }
    }
}

fn outage_registrar(
    catalog: &Arc<ExecutableEnvironmentCatalog>,
    fail: bool,
) -> Arc<LatestOutageRegistrar> {
    Arc::new(LatestOutageRegistrar {
        inner: LocalExecutableEnvironmentRegistrar::new(catalog.clone()),
        fail_latest: AtomicBool::new(fail),
        attempts: AtomicUsize::new(0),
        histories: Mutex::new(Vec::new()),
    })
}

async fn pending_revisions<D: EnvironmentRegistrationOutbox>(definitions: &D) -> Vec<u64> {
    definitions
        .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
        .await
        .unwrap()
        .iter()
        .map(|intent| intent.revision.0)
        .collect()
}

/// Process 1: C1-C4, E1. Returns the Environment id.
async fn first_process<D, Q>(definitions: Arc<D>, work: Arc<Q>, fault: &AckFault) -> String
where
    D: EnvironmentDefinitionQueries
        + awaken_environment_contract::EnvironmentDefinitionStore
        + EnvironmentRegistrationOutbox
        + 'static,
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let delegate = outage_registrar(&catalog, false);
    let application = EnvironmentApplication::new(
        definitions.clone(),
        Arc::new(CoordinatorEnvironmentRegistrar::new(
            delegate.clone(),
            work.clone(),
        )),
        None,
    );
    fault.arm().await;
    assert!(
        application
            .create(CreateEnvironmentCommand {
                command_id: "durable-pending".into(),
                name: "durable-pending".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await
            .is_err(),
        "C1 the acknowledgement was lost"
    );
    fault.disarm().await;
    let id = definitions.list_active().await.unwrap().pop().unwrap().id;
    let cloud = application
        .update(
            &id,
            EnvUpdate {
                config: Some(EnvironmentConfigMutation::Replace(
                    EnvironmentConfig::Cloud {
                        networking: Default::default(),
                        packages: Default::default(),
                    },
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cloud.revision, EnvironmentRevision(2), "C2");
    delegate.fail_latest.store(true, Ordering::SeqCst);
    assert!(
        application
            .update(
                &id,
                EnvUpdate {
                    config: Some(EnvironmentConfigMutation::Replace(
                        EnvironmentConfig::SelfHosted
                    )),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
        "C3"
    );
    assert!(
        application.drain_registration_intents().await.is_err(),
        "C4"
    );
    assert_eq!(pending_revisions(definitions.as_ref()).await, [1, 3], "E2");
    assert!(
        delegate
            .histories
            .lock()
            .unwrap()
            .iter()
            .all(|revision| *revision < EnvironmentRevision(3)),
        "E1 the latest Pending never masquerades as history"
    );
    assert!(
        work.list(&id).await.unwrap().is_empty(),
        "E1 no v3 Work yet"
    );
    assert_eq!(
        work.apply_environment_revision(
            &id,
            EnvironmentRevision(2),
            Operation::Register { healthcheck: false }
        )
        .await
        .unwrap(),
        Outcome::AlreadyApplied,
        "E1 the history never advanced the v2 watermark"
    );
    id
}

/// E3: the catalog and the Work effect both converge on v3.
async fn converged_verdict<Q>(
    catalog: &ExecutableEnvironmentCatalog,
    work: &Q,
    id: &str,
) -> Result<(), String>
where
    Q: EnvironmentWorkLifecycle + WorkQueries,
{
    let catalog_revision = catalog
        .current(id)
        .map(|current| current.definition.revision);
    if catalog_revision != Some(EnvironmentRevision(3)) {
        return Err(format!("E3 catalog at {catalog_revision:?}"));
    }
    let items = work.list(id).await.map_err(|error| error.to_string())?;
    if items.len() != 1 {
        return Err(format!("E3 v3 Work effect has {} rows", items.len()));
    }
    let replay = work
        .apply_environment_revision(
            id,
            EnvironmentRevision(3),
            Operation::Register { healthcheck: true },
        )
        .await;
    match replay {
        Ok(Outcome::AlreadyApplied) => Ok(()),
        other => Err(format!("E3 v3 watermark {other:?}")),
    }
}

/// Process 2 over reopened handles: E2 survives, E3 converges once.
async fn second_process<D, Q>(definitions: Arc<D>, work: Arc<Q>, id: &str)
where
    D: EnvironmentDefinitionQueries
        + awaken_environment_contract::EnvironmentDefinitionStore
        + EnvironmentRegistrationOutbox
        + 'static,
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    assert_eq!(
        pending_revisions(definitions.as_ref()).await,
        [1, 3],
        "E2 the failure responsibility survived the rebuild"
    );
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let delegate = outage_registrar(&catalog, false);
    let application = EnvironmentApplication::new(
        definitions.clone(),
        Arc::new(CoordinatorEnvironmentRegistrar::new(
            delegate.clone(),
            work.clone(),
        )),
        None,
    );
    assert_eq!(application.drain_registration_intents().await.unwrap(), 2);
    assert_eq!(
        delegate.attempts.load(Ordering::SeqCst),
        1,
        "E3 one v3 delivery"
    );
    assert!(
        pending_revisions(definitions.as_ref()).await.is_empty(),
        "E3"
    );
    converged_verdict(&catalog, work.as_ref(), id)
        .await
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_eq!(
        work.apply_environment_revision(
            id,
            EnvironmentRevision(2),
            Operation::Register { healthcheck: false }
        )
        .await
        .unwrap(),
        Outcome::Superseded,
        "E3"
    );
    assert_eq!(application.drain_registration_intents().await.unwrap(), 0);
    assert_eq!(
        work.list(id).await.unwrap().len(),
        1,
        "E3 replay adds nothing"
    );
}

#[tokio::test]
async fn sqlite_pending_history_survives_a_rebuild_and_converges_once() {
    let directory = tempfile::tempdir().unwrap();
    let definitions_path = directory.path().join("definitions.db");
    let work_path = directory.path().join("work.db");
    let (definitions_text, work_text) = (
        definitions_path.to_str().unwrap(),
        work_path.to_str().unwrap(),
    );
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&definitions_path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    let definitions = Arc::new(
        SqliteEnvRegistry::open(definitions_text, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let work = Arc::new(
        SqliteWorkQueue::open(work_text, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let fault = AckFault::Sqlite(raw);
    let id = first_process(definitions, work, &fault).await;
    if let AckFault::Sqlite(raw) = fault {
        raw.close().await;
    }
    let definitions = Arc::new(
        SqliteEnvRegistry::open(definitions_text, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let work = Arc::new(
        SqliteWorkQueue::open(work_text, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    second_process(definitions, work, &id).await;
}

#[tokio::test]
async fn postgres_pending_history_survives_a_rebuild_through_independent_pools() {
    let Some(fixture) = PostgresFixture::configured().await else {
        return;
    };
    let definitions = Arc::new(
        PostgresEnvRegistry::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let work = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let fault = AckFault::Postgres(fixture.pool.clone());
    let id = first_process(definitions, work, &fault).await;
    let rebuilt = PostgresFixture::connect(&fixture.options).await;
    let definitions = Arc::new(
        PostgresEnvRegistry::with_existing_pool(rebuilt.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let work = Arc::new(
        PostgresWorkQueue::with_existing_pool(rebuilt.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    second_process(definitions, work, &id).await;
    rebuilt.close().await;
    fixture.cleanup().await;
}

#[test]
fn postgres_pending_history_survives_a_rebuild_through_independent_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "durable_pending::postgres_pending_history_survives_a_rebuild_through_independent_pools",
    );
}

#[test]
fn postgres_history_cannot_consume_unapplied_higher_registration_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_history_cannot_consume_unapplied_higher_registration",
    );
}

#[test]
fn postgres_pending_history_does_not_swallow_new_registration_effect_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_pending_history_does_not_swallow_new_registration_effect",
    );
}

#[tokio::test]
async fn a_catalog_high_watermark_projection_is_rejected_by_the_convergence_verdict() {
    let definitions = Arc::new(InMemoryEnvRegistry::new(awaken_time::system_clock()));
    let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
    let fault = AckFault::Volatile(definitions.clone());
    let id = first_process(definitions.clone(), work.clone(), &fault).await;
    // Mutant: deliver into the catalog only, never through the Work projection.
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let application = EnvironmentApplication::new(
        definitions,
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
        None,
    );
    assert_eq!(application.drain_registration_intents().await.unwrap(), 2);
    assert_eq!(
        catalog
            .current(&id)
            .map(|current| current.definition.revision),
        Some(EnvironmentRevision(3)),
        "the mutant did advance the catalog high-watermark"
    );
    let verdict = converged_verdict(&catalog, work.as_ref(), &id).await;
    assert!(
        verdict
            .as_ref()
            .is_err_and(|failure| failure.contains("Work effect")),
        "NC: a catalog-only convergence must be rejected, got {verdict:?}"
    );
}
