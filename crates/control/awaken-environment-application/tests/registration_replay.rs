//! Actual Coordinator/Work replay regressions. PG requires the explicit fixture lane.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_environment_application::{
    EnvironmentApplication, execution::CoordinatorEnvironmentRegistrar,
};
use awaken_environment_contract::{
    CreateEnvironmentCommand, EnvItem, EnvUpdate, EnvironmentConfig, EnvironmentConfigMutation,
    EnvironmentDefinitionQueries, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOutbox, EnvironmentRevision,
};
use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
};
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome, WorkQueries,
};
use awaken_work_store::{InMemoryWorkQueue, PostgresWorkQueue, SqliteWorkQueue};

#[path = "registration_replay/durable_pending.rs"]
mod durable_pending;
#[path = "registration_replay/history_visibility.rs"]
mod history_visibility;

fn registration(id: &str, revision: EnvironmentRevision) -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(
        EnvItem {
            id: id.into(),
            revision,
            name: id.into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
            created_at: None,
            updated_at: None,
            archived_at: None,
        },
        None,
    )
}

async fn acknowledged_withdrawal<Q>(work: Arc<Q>)
where
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let coordinator = CoordinatorEnvironmentRegistrar::new(
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
        work.clone(),
    );
    // The caller loses the first successful response; the physical registration
    // and its healthcheck already exist, so the same old request may arrive late.
    coordinator
        .register(registration("delayed-replay", EnvironmentRevision(1)))
        .await
        .unwrap();
    assert_eq!(work.list("delayed-replay").await.unwrap().len(), 1);
    coordinator
        .withdraw(ExecutableEnvironmentWithdrawal {
            environment_id: "delayed-replay".into(),
            lifecycle_revision: EnvironmentRevision(2),
        })
        .await
        .unwrap();
    assert!(catalog.current("delayed-replay").is_none());
    assert!(work.list("delayed-replay").await.unwrap().is_empty());
}

async fn delayed_registration<Q>(work: Arc<Q>)
where
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    // This intentionally uses a freshly rebuilt catalog. Durable Work revision
    // protection must not disappear merely because a process projection is cold.
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let coordinator = CoordinatorEnvironmentRegistrar::new(
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
        work.clone(),
    );
    coordinator
        .register(registration("delayed-replay", EnvironmentRevision(1)))
        .await
        .unwrap();
    assert!(
        work.list("delayed-replay").await.unwrap().is_empty(),
        "late revision 1 resurrected work after acknowledged withdrawal 2"
    );
}

struct PostgresFixture {
    pool: sqlx::PgPool,
    options: sqlx::postgres::PgConnectOptions,
    schema: String,
}

impl PostgresFixture {
    async fn configured() -> Option<Self> {
        use std::str::FromStr as _;
        let url = awaken_reliability_testkit::postgres_fixture_url()?;
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&url).expect("valid explicit fixture URL");
        let admin = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_with(options.clone())
            .await
            .expect("configured PostgreSQL fixture must connect");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("env_replay_{}_{}", std::process::id(), stamp);
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .expect("new owned replay schema");
        admin.close().await;
        let options = options.options([("search_path", schema.as_str())]);
        let pool = Self::connect(&options).await;
        let actual: String = sqlx::query_scalar("SELECT current_schema()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(actual, schema, "fixture never uses another schema");
        Some(Self {
            pool,
            options,
            schema,
        })
    }

    async fn connect(options: &sqlx::postgres::PgConnectOptions) -> sqlx::PgPool {
        awaken_sql_storage::postgres_pool_options()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_with(options.clone())
            .await
            .expect("connect exact owned replay schema")
    }

    async fn cleanup(self) {
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.pool)
            .await
            .expect("drop only this replay fixture schema");
        self.pool.close().await;
    }
}

#[tokio::test]
async fn volatile_delayed_registration_cannot_restore_withdrawn_work() {
    let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
    acknowledged_withdrawal(work.clone()).await;
    delayed_registration(work).await;
}

#[tokio::test]
async fn sqlite_delayed_registration_after_reopen_cannot_restore_withdrawn_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("work.db");
    let work = Arc::new(
        SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    acknowledged_withdrawal(work.clone()).await;
    drop(work);
    let reopened = Arc::new(
        SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    delayed_registration(reopened).await;
}

#[tokio::test]
async fn postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work() {
    let Some(fixture) = PostgresFixture::configured().await else {
        return;
    };
    let work = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    acknowledged_withdrawal(work.clone()).await;
    drop(work);
    let independent_pool = PostgresFixture::connect(&fixture.options).await;
    let work = Arc::new(
        PostgresWorkQueue::with_pool(independent_pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    delayed_registration(work).await;
    independent_pool.close().await;
    fixture.cleanup().await;
}

// GREEN_EXTENSION_MARKER

struct LatestOutageRegistrar {
    inner: LocalExecutableEnvironmentRegistrar,
    fail_latest: AtomicBool,
    attempts: AtomicUsize,
    histories: Mutex<Vec<EnvironmentRevision>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for LatestOutageRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        if registration.definition.revision == EnvironmentRevision(3) {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail_latest.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "latest convergence failure".into(),
                ));
            }
        }
        self.inner.register(registration).await
    }
    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.inner.withdraw(withdrawal).await
    }
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        self.histories
            .lock()
            .unwrap()
            .push(registration.definition.revision);
        self.inner
            .restore_historical_registration(registration)
            .await
    }
}

async fn unapplied_higher_registration<Q>(work: Arc<Q>)
where
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    use awaken_executable_environment_contract::{
        EnvironmentWorkRevisionOperation as Operation, EnvironmentWorkRevisionOutcome as Outcome,
    };
    acknowledged_withdrawal(work.clone()).await;
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let delegate = Arc::new(LatestOutageRegistrar {
        inner: LocalExecutableEnvironmentRegistrar::new(catalog.clone()),
        fail_latest: AtomicBool::new(true),
        attempts: AtomicUsize::new(0),
        histories: Mutex::new(Vec::new()),
    });
    let coordinator = CoordinatorEnvironmentRegistrar::new(delegate.clone(), work.clone());
    // This higher registration is legal at the low-level versioned port. It is
    // deliberately not described as Control unarchiving an immutable definition.
    assert!(
        coordinator
            .register(registration("delayed-replay", EnvironmentRevision(3)))
            .await
            .is_err()
    );
    coordinator
        .restore_historical_registration(registration("delayed-replay", EnvironmentRevision(1)))
        .await
        .unwrap();
    assert!(catalog.current("delayed-replay").is_none());
    assert_eq!(
        work.apply_environment_revision(
            "delayed-replay",
            EnvironmentRevision(2),
            Operation::Withdraw
        )
        .await
        .unwrap(),
        Outcome::AlreadyApplied
    );
    assert!(work.list("delayed-replay").await.unwrap().is_empty());
    delegate.fail_latest.store(false, Ordering::SeqCst);
    coordinator
        .register(registration("delayed-replay", EnvironmentRevision(3)))
        .await
        .unwrap();
    assert_eq!(delegate.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(work.list("delayed-replay").await.unwrap().len(), 1);
    assert_eq!(
        work.apply_environment_revision(
            "delayed-replay",
            EnvironmentRevision(2),
            Operation::Withdraw
        )
        .await
        .unwrap(),
        Outcome::Superseded
    );
}

async fn pending_history<Q>(work: Arc<Q>, all: bool)
where
    Q: EnvironmentWorkLifecycle + WorkQueries + 'static,
{
    use awaken_executable_environment_contract::{
        EnvironmentWorkRevisionOperation as Operation, EnvironmentWorkRevisionOutcome as Outcome,
    };
    let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
        awaken_time::system_clock(),
    ));
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let delegate = Arc::new(LatestOutageRegistrar {
        inner: LocalExecutableEnvironmentRegistrar::new(catalog.clone()),
        fail_latest: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        histories: Mutex::new(Vec::new()),
    });
    let application = EnvironmentApplication::new(
        definitions.clone(),
        Arc::new(CoordinatorEnvironmentRegistrar::new(
            delegate.clone(),
            work.clone(),
        )),
        None,
    );
    definitions.fail_next_acknowledgements(1);
    assert!(
        application
            .create(CreateEnvironmentCommand {
                command_id: "new-pending-cross".into(),
                name: "new-pending-cross".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await
            .is_err()
    );
    let first = definitions.list_active().await.unwrap().pop().unwrap();
    let cloud = application
        .update(
            &first.id,
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
    assert_eq!(cloud.revision, EnvironmentRevision(2));
    delegate.fail_latest.store(true, Ordering::SeqCst);
    assert!(
        application
            .update(
                &first.id,
                EnvUpdate {
                    config: Some(EnvironmentConfigMutation::Replace(
                        EnvironmentConfig::SelfHosted
                    )),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        definitions.get(&first.id).await.unwrap().unwrap().revision,
        EnvironmentRevision(3)
    );
    if all {
        assert!(application.recover_registration_intents().await.is_err());
    } else {
        assert!(application.drain_registration_intents().await.is_err());
    }
    let pending = definitions
        .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|intent| intent.revision)
            .collect::<Vec<_>>(),
        [EnvironmentRevision(1), EnvironmentRevision(3)]
    );
    assert!(
        delegate
            .histories
            .lock()
            .unwrap()
            .contains(&EnvironmentRevision(1))
    );
    assert!(
        delegate
            .histories
            .lock()
            .unwrap()
            .iter()
            .all(|revision| *revision < EnvironmentRevision(3)),
        "latest Pending must not masquerade as historical delivery"
    );
    assert_eq!(
        catalog.current(&first.id).unwrap().definition.revision,
        EnvironmentRevision(2)
    );
    assert!(work.list(&first.id).await.unwrap().is_empty());
    assert_eq!(
        work.apply_environment_revision(
            &first.id,
            EnvironmentRevision(2),
            Operation::Register { healthcheck: false }
        )
        .await
        .unwrap(),
        Outcome::AlreadyApplied,
        "failed v3/historical restore never advances v2 queue watermark"
    );
    delegate.fail_latest.store(false, Ordering::SeqCst);
    assert_eq!(application.drain_registration_intents().await.unwrap(), 2);
    assert_eq!(delegate.attempts.load(Ordering::SeqCst), 3);
    assert!(
        definitions
            .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        catalog.current(&first.id).unwrap().definition.revision,
        EnvironmentRevision(3)
    );
    assert_eq!(
        work.list(&first.id).await.unwrap().len(),
        1,
        "the new v3 queue effect was actually applied"
    );
    assert_eq!(
        work.apply_environment_revision(
            &first.id,
            EnvironmentRevision(2),
            Operation::Register { healthcheck: false }
        )
        .await
        .unwrap(),
        Outcome::Superseded
    );
    assert_eq!(
        work.apply_environment_revision(
            &first.id,
            EnvironmentRevision(3),
            Operation::Register { healthcheck: true }
        )
        .await
        .unwrap(),
        Outcome::AlreadyApplied
    );
}

#[tokio::test]
async fn volatile_history_cannot_consume_unapplied_higher_registration() {
    unapplied_higher_registration(Arc::new(
        InMemoryWorkQueue::new(awaken_time::system_clock()),
    ))
    .await;
}

#[tokio::test]
async fn sqlite_history_cannot_consume_unapplied_higher_registration() {
    unapplied_higher_registration(Arc::new(
        SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap(),
    ))
    .await;
}

#[tokio::test]
async fn postgres_history_cannot_consume_unapplied_higher_registration() {
    let Some(fixture) = PostgresFixture::configured().await else {
        return;
    };
    unapplied_higher_registration(Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    ))
    .await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn volatile_pending_history_does_not_swallow_new_registration_effect() {
    for all in [false, true] {
        pending_history(
            Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock())),
            all,
        )
        .await;
    }
}

#[tokio::test]
async fn sqlite_pending_history_does_not_swallow_new_registration_effect() {
    for all in [false, true] {
        pending_history(
            Arc::new(
                SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
                    .await
                    .unwrap(),
            ),
            all,
        )
        .await;
    }
}

#[tokio::test]
async fn postgres_pending_history_does_not_swallow_new_registration_effect() {
    for all in [false, true] {
        let Some(fixture) = PostgresFixture::configured().await else {
            return;
        };
        pending_history(
            Arc::new(
                PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
                    .await
                    .unwrap(),
            ),
            all,
        )
        .await;
        fixture.cleanup().await;
    }
}
