//! T20-C02: an SQL fault at the last step of Environment create (the
//! registration-intent INSERT) returns a store failure and leaves current,
//! revision, command and intent exactly as before -- on real file SQLite and
//! PostgreSQL, observed through an independent handle and after a reopen, and
//! the same command then commits exactly once.
//!
//! Cause/effect graph: C1 a real database trigger rejects the intent INSERT;
//! C2 `create_once` runs through the production adapter; C3 the trigger is
//! removed, the store reopened and the same command retried. Effects: E1 a
//! typed `CreateEnvironmentError::Store`; E2 the durable image and the command
//! lookup equal their pre-command values on every handle; E3 the retry is
//! Created once and its replay is Replayed with the same identity.
//!
//! Negative control (executed): a split-commit adapter that persists the
//! current row before the failing intent step is rejected by the same
//! [`rollback_verdict`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem, EnvUpdate,
    EnvironmentConfig, EnvironmentDefinitionQueries, EnvironmentDefinitionStore,
    EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOutbox, EnvironmentRevision, EnvironmentStoreError,
};
use awaken_store_conformance::env_registry::{DurableEnvironmentImage, durable_image};

fn command(id: &str) -> CreateEnvironmentCommand {
    CreateEnvironmentCommand {
        command_id: id.into(),
        name: "rollback".into(),
        description: None,
        metadata: Default::default(),
        scope: None,
        config: EnvironmentConfig::SelfHosted,
        sandbox_policy: None,
    }
}

/// Everything the port can observe about one command's durable effect.
#[derive(Debug, PartialEq)]
struct Observation {
    image: DurableEnvironmentImage,
    receipt: Option<EnvItem>,
}

async fn observe<R>(registry: &R, command: &CreateEnvironmentCommand) -> Observation
where
    R: EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox,
{
    Observation {
        image: durable_image(registry).await,
        receipt: registry
            .find_create_result(command)
            .await
            .expect("command lookup"),
    }
}

/// E1 + E2: a failed create is a typed store error and no durable change.
fn rollback_verdict(
    result: &Result<CreateEnvironmentOutcome, CreateEnvironmentError>,
    before: &Observation,
    after: &Observation,
) -> Result<(), String> {
    if !matches!(result, Err(CreateEnvironmentError::Store(_))) {
        return Err(format!("E1 expected a store failure, got {result:?}"));
    }
    if after != before {
        return Err(format!("E2 partial create survived: {after:?}"));
    }
    Ok(())
}

/// E3 on one handle.
async fn retry_commits_once<R>(registry: &R, command: &CreateEnvironmentCommand)
where
    R: EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox,
{
    let before = durable_image(registry).await;
    let created = match registry.create_once(command.clone()).await.unwrap() {
        CreateEnvironmentOutcome::Created(item) => item,
        other => panic!("E3 retry must create, got {other:?}"),
    };
    let replay = registry.create_once(command.clone()).await.unwrap();
    assert!(
        matches!(&replay, CreateEnvironmentOutcome::Replayed(item) if *item == created),
        "E3 exact replay, got {replay:?}"
    );
    let after = durable_image(registry).await;
    assert_eq!(after.all.len(), before.all.len() + 1, "E3 one current row");
    assert_eq!(
        after.revisions.len(),
        before.revisions.len() + 1,
        "E3 one revision"
    );
    assert_eq!(
        after
            .intents
            .iter()
            .filter(|intent| intent.environment_id == created.id)
            .count(),
        1,
        "E3 one intent"
    );
}

async fn raw_sqlite(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(path))
        .await
        .expect("raw SQLite fixture connection")
}

#[tokio::test]
async fn file_sqlite_failed_intent_insert_rolls_back_for_every_handle_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("create-rollback.db");
    let text = path.to_str().unwrap();
    let writer = SqliteEnvRegistry::open(text, awaken_time::system_clock())
        .await
        .unwrap();
    // A sibling Environment proves the rollback is exact, not "empty".
    writer
        .create(
            "sibling".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .unwrap();
    let rejected = command("file-rollback");
    let before = observe(&writer, &rejected).await;
    let raw = raw_sqlite(&path).await;
    sqlx::raw_sql(
        "CREATE TRIGGER t20_fail_intent BEFORE INSERT ON env_registry_registration_intent \
         BEGIN SELECT RAISE(ABORT, 'injected intent failure'); END;",
    )
    .execute(&raw)
    .await
    .unwrap();
    let result = writer.create_once(rejected.clone()).await;
    rollback_verdict(&result, &before, &observe(&writer, &rejected).await)
        .unwrap_or_else(|failure| panic!("writer: {failure}"));
    let observer = SqliteEnvRegistry::open(text, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(
        observe(&observer, &rejected).await,
        before,
        "E2 independent handle"
    );
    sqlx::raw_sql("DROP TRIGGER t20_fail_intent")
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    drop((writer, observer));
    let reopened = SqliteEnvRegistry::open(text, awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(observe(&reopened, &rejected).await, before, "E2 reopen");
    retry_commits_once(&reopened, &rejected).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_failed_intent_insert_rolls_back_for_independent_pools() {
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
    let schema = format!("t_env_t20_rollback_{}_{nonce:x}", std::process::id());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
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
    writer
        .create(
            "sibling".into(),
            String::new(),
            Default::default(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .unwrap();
    let rejected = command("postgres-rollback");
    let before = observe(&writer, &rejected).await;
    let raw = connect().await;
    sqlx::raw_sql(
        "CREATE FUNCTION t20_reject_intent() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'injected intent failure'; END $$; \
         CREATE TRIGGER t20_fail_intent BEFORE INSERT ON env_registry_registration_intent \
         FOR EACH ROW EXECUTE FUNCTION t20_reject_intent();",
    )
    .execute(&raw)
    .await
    .unwrap();
    let result = writer.create_once(rejected.clone()).await;
    rollback_verdict(&result, &before, &observe(&writer, &rejected).await)
        .unwrap_or_else(|failure| panic!("writer: {failure}"));
    let observer = awaken_env_store::PostgresEnvRegistry::with_existing_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(
        observe(&observer, &rejected).await,
        before,
        "E2 independent pool"
    );
    sqlx::raw_sql(
        "DROP TRIGGER t20_fail_intent ON env_registry_registration_intent; \
         DROP FUNCTION t20_reject_intent();",
    )
    .execute(&raw)
    .await
    .unwrap();
    raw.close().await;
    drop((writer, observer));
    let reopened = awaken_env_store::PostgresEnvRegistry::with_existing_pool(
        connect().await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    assert_eq!(observe(&reopened, &rejected).await, before, "E2 reopen");
    retry_commits_once(&reopened, &rejected).await;
    drop(reopened);
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .expect("drop only this run's schema");
    admin.close().await;
}

#[test]
fn postgres_failed_intent_insert_rolls_back_for_independent_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_failed_intent_insert_rolls_back_for_independent_pools",
    );
}

/// NC: commits the current row in its own step, then fails the intent step.
struct SplitCommit {
    inner: Arc<InMemoryEnvRegistry>,
}

#[async_trait]
impl EnvironmentDefinitionQueries for SplitCommit {
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
        self.inner.get_revision(id, revision).await
    }
    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        self.inner.exists(id).await
    }
}

#[async_trait]
impl EnvironmentDefinitionStore for SplitCommit {
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
        // Step 1 commits on its own; step 2 (the intent) then "fails".
        self.inner.create_once(command).await?;
        Err(CreateEnvironmentError::Store(
            "injected intent failure after a separate current-row commit".into(),
        ))
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
impl EnvironmentRegistrationOutbox for SplitCommit {
    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.inner.registration_intent(id, revision).await
    }
    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.inner.registration_intents(filter).await
    }
    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        self.inner.mark_registration_intent_delivered(intent).await
    }
}

#[tokio::test]
async fn a_split_commit_adapter_is_rejected_by_the_rollback_verdict() {
    let mutant = SplitCommit {
        inner: Arc::new(InMemoryEnvRegistry::new(awaken_time::system_clock())),
    };
    let rejected = command("split-commit");
    let before = observe(&mutant, &rejected).await;
    let result = mutant.create_once(rejected.clone()).await;
    let verdict = rollback_verdict(&result, &before, &observe(&mutant, &rejected).await);
    assert!(
        verdict
            .as_ref()
            .is_err_and(|failure| failure.starts_with("E2")),
        "NC: a current row committed apart from its intent must be detected, got {verdict:?}"
    );
}
