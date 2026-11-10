//! T20-C01: the registration-outbox decision table (definition, immutable
//! revision and the correct Register/Withdraw intent committed as one pair;
//! identical retries add no fact; history never follows the current row) on
//! real file SQLite and PostgreSQL, observed again through an independent
//! handle and after a reopen.
//!
//! | Row | Backend | Writer | Observer |
//! |---|---|---|---|
//! | R1 | file SQLite | handle A | independent handle B, then reopened C |
//! | R2 | PostgreSQL | pool A | independent pool B, then pool C |
//!
//! Negative controls, executed through the SAME table (each must fail):
//! N1 `get_revision` answers from the current row (history drifts with it);
//! N2 the outbox is derived from current rows instead of authored intents.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem, EnvUpdate,
    EnvironmentDefinitionQueries, EnvironmentDefinitionStore, EnvironmentRegistrationIntent,
    EnvironmentRegistrationIntentFilter, EnvironmentRegistrationOutbox, EnvironmentRevision,
    EnvironmentStoreError,
};
use awaken_store_conformance::env_registry::{durable_image, registration_outbox_decision_table};

/// A countermodel qualifies only at its expected assertion. Setup failures,
/// cancellation and unrelated panics cannot stand in for the oracle.
async fn table_rejects_at(
    table: impl std::future::Future<Output = ()> + 'static,
    expected_assertion: &'static str,
) -> bool {
    let local = tokio::task::LocalSet::new();
    let Err(error) = local.run_until(local.spawn_local(table)).await else {
        return false;
    };
    if !error.is_panic() {
        return false;
    }
    let payload = error.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message.is_some_and(|message| message.starts_with(expected_assertion))
}

#[tokio::test]
async fn unrelated_failures_cannot_qualify_an_outbox_countermodel() {
    const ASSERTION: &str = "assertion `left == right` failed: O5 v1 name";
    assert!(!table_rejects_at(async { panic!("fixture setup failed") }, ASSERTION).await);
    assert!(
        !table_rejects_at(
            async { panic!("setup mentioned assertion `left == right` failed: O5 v1 name") },
            ASSERTION
        )
        .await
    );
    assert!(!table_rejects_at(async {}, ASSERTION).await);
    assert!(
        table_rejects_at(
            async { assert_eq!("current", "authored", "O5 v1 name") },
            ASSERTION
        )
        .await
    );
}

#[tokio::test]
async fn file_sqlite_outbox_pairs_survive_an_independent_handle_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("outbox.db");
    let path = path.to_str().unwrap();
    let writer = SqliteEnvRegistry::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    registration_outbox_decision_table(&writer).await;
    let written = durable_image(&writer).await;
    let observer = SqliteEnvRegistry::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        durable_image(&observer).await,
        written,
        "R1 the independent handle sees the same committed pairs"
    );
    drop((writer, observer));
    let reopened = SqliteEnvRegistry::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(durable_image(&reopened).await, written, "R1 reopen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_outbox_pairs_survive_independent_pools() {
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
    let schema = format!("t_env_t20_outbox_{}_{nonce:x}", std::process::id());
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
    let writer = awaken_env_store::PostgresEnvRegistry::with_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    registration_outbox_decision_table(&writer).await;
    let written = durable_image(&writer).await;
    let observer = awaken_env_store::PostgresEnvRegistry::with_existing_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        durable_image(&observer).await,
        written,
        "R2 independent pool"
    );
    drop((writer, observer));
    let reopened = awaken_env_store::PostgresEnvRegistry::with_existing_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(durable_image(&reopened).await, written, "R2 reopen");
    drop(reopened);
    admin
        .execute(
            // The schema is a fixed fixture prefix plus integer process/time coordinates.
            sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")),
        )
        .await
        .expect("drop only this run's schema");
    admin.close().await;
}

#[test]
fn postgres_outbox_pairs_survive_independent_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_outbox_pairs_survive_independent_pools",
    );
}

#[derive(Clone, Copy, Debug)]
enum CurrentRowShortcut {
    /// N1: history is read from the current row.
    HistoryFollowsCurrent,
    /// N2: the outbox is reconstructed from current rows.
    IntentsFromCurrentRows,
}

/// A deliberately wrong adapter: it checks "a current row exists" instead of
/// the immutable revision/intent pair.
struct CurrentRowOnly<R> {
    inner: R,
    shortcut: CurrentRowShortcut,
}

#[async_trait]
impl<R: EnvironmentDefinitionQueries + Send + Sync> EnvironmentDefinitionQueries
    for CurrentRowOnly<R>
{
    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.inner.list_active().await
    }
    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.inner.list_all().await
    }
    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.get(id).await
    }
    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        match self.shortcut {
            CurrentRowShortcut::HistoryFollowsCurrent => Ok(self
                .inner
                .get(id)
                .await?
                .filter(|current| current.revision.0 >= revision.0)),
            CurrentRowShortcut::IntentsFromCurrentRows => {
                self.inner.get_revision(id, revision).await
            }
        }
    }
    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        self.inner.exists(id).await
    }
}

#[async_trait]
impl<R: EnvironmentDefinitionStore + Send + Sync> EnvironmentDefinitionStore for CurrentRowOnly<R> {
    async fn find_create_result(
        &self,
        command: &CreateEnvironmentCommand,
    ) -> Result<Option<EnvItem>, CreateEnvironmentError> {
        self.inner.find_create_result(command).await
    }
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        self.inner.create_once(command).await
    }
    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.update(id, expected_revision, patch).await
    }
    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.inner.archive(id).await
    }
}

#[async_trait]
impl<R> EnvironmentRegistrationOutbox for CurrentRowOnly<R>
where
    R: EnvironmentDefinitionQueries + EnvironmentRegistrationOutbox + Send + Sync,
{
    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        match self.shortcut {
            CurrentRowShortcut::IntentsFromCurrentRows => Ok(self
                .inner
                .get(id)
                .await?
                .filter(|current| current.revision == revision)
                .map(|current| EnvironmentRegistrationIntent::for_item(&current))),
            CurrentRowShortcut::HistoryFollowsCurrent => {
                self.inner.registration_intent(id, revision).await
            }
        }
    }
    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        match self.shortcut {
            CurrentRowShortcut::IntentsFromCurrentRows => Ok(self
                .inner
                .list_all()
                .await?
                .iter()
                .map(EnvironmentRegistrationIntent::for_item)
                .collect()),
            CurrentRowShortcut::HistoryFollowsCurrent => {
                self.inner.registration_intents(filter).await
            }
        }
    }
    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        self.inner.mark_registration_intent_delivered(intent).await
    }
}

#[tokio::test]
async fn current_row_shortcuts_fail_the_outbox_table_on_volatile_and_file_sqlite() {
    for shortcut in [
        CurrentRowShortcut::HistoryFollowsCurrent,
        CurrentRowShortcut::IntentsFromCurrentRows,
    ] {
        let expected_assertion = match shortcut {
            CurrentRowShortcut::HistoryFollowsCurrent => {
                "assertion `left == right` failed: O5 v1 name"
            }
            CurrentRowShortcut::IntentsFromCurrentRows => "assertion `left == right` failed: O2b",
        };
        assert!(
            table_rejects_at(
                async move {
                    let wrapper = CurrentRowOnly {
                        inner: InMemoryEnvRegistry::new(awaken_time::system_clock()),
                        shortcut,
                    };
                    registration_outbox_decision_table(&wrapper).await;
                },
                expected_assertion
            )
            .await,
            "{shortcut:?} over Volatile must fail the table"
        );
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let path = directory.path().join("shortcut.db");
        assert!(
            table_rejects_at(
                async move {
                    let _owned = directory;
                    let wrapper = CurrentRowOnly {
                        inner: SqliteEnvRegistry::open(
                            path.to_str().unwrap(),
                            awaken_time::system_clock(),
                        )
                        .await
                        .unwrap(),
                        shortcut,
                    };
                    registration_outbox_decision_table(&wrapper).await;
                },
                expected_assertion
            )
            .await,
            "{shortcut:?} over file SQLite must fail the table"
        );
    }
}
