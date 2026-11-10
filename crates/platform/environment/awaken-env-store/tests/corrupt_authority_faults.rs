//! T37-C03 (Environment half): a corrupt persisted Environment fails closed
//! with one stable typed error on every handle -- independent handle, reopen,
//! PostgreSQL pool -- with zero repair and zero overwrite of the stored bytes.
//!
//! Cause/effect graph: C1 a committed Environment's `metadata_json`,
//! `config_json` or `sandbox_policy_json` is replaced by `not-json`; C2 reads
//! go through the production adapter on another handle and after reopen.
//! Effects: E1 `get` and `list_all` return the same `EnvironmentStoreError` on
//! every handle (never a default, never an empty catalog, never a panic);
//! E2 the raw column still holds exactly `not-json` afterwards.
//!
//! Negative control (executed): [`fail_closed_verdict`] rejects the outcomes a
//! defaulting adapter (`Ok(Some(..))`) or an empty-catalog rebuild (`Ok(None)` /
//! `Ok(vec![])`) would produce.

use awaken_env_store::SqliteEnvRegistry;
use awaken_environment_contract::{
    EnvItem, EnvironmentConfig, EnvironmentDefinitionQueries, EnvironmentDefinitionStore,
    EnvironmentStoreError,
};

const COLUMNS: [&str; 3] = ["metadata_json", "config_json", "sandbox_policy_json"];

type Reads = (
    Result<Option<EnvItem>, EnvironmentStoreError>,
    Result<Vec<EnvItem>, EnvironmentStoreError>,
);

/// E1: both reads fail with one typed error.
fn fail_closed_verdict(reads: &Reads) -> Result<EnvironmentStoreError, String> {
    match reads {
        (Err(get), Err(list)) if get == list => Ok(get.clone()),
        (Err(get), Err(list)) => Err(format!("E1 unstable errors: {get:?} vs {list:?}")),
        other => Err(format!("E1 corruption was admitted: {other:?}")),
    }
}

async fn reads<R: EnvironmentDefinitionQueries>(registry: &R, id: &str) -> Reads {
    (registry.get(id).await, registry.list_all().await)
}

fn assert_negative_control(id: &str, committed: &EnvItem) {
    for mutant in [
        (Ok(Some(committed.clone())), Ok(vec![committed.clone()])),
        (Ok(None), Ok(Vec::new())),
    ] {
        assert!(
            fail_closed_verdict(&mutant).is_err(),
            "NC: a defaulting or empty-catalog adapter for {id} is detected"
        );
    }
}

#[tokio::test]
async fn file_sqlite_corrupt_rows_fail_closed_on_every_handle_without_repair() {
    for column in COLUMNS {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt.db");
        let text = path.to_str().unwrap();
        let writer = SqliteEnvRegistry::open(text, awaken_time::system_clock())
            .await
            .unwrap();
        let committed = writer
            .create(
                format!("corrupt-{column}"),
                String::new(),
                Default::default(),
                EnvironmentConfig::SelfHosted,
            )
            .await
            .unwrap();
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(3))
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        sqlx::query(
            // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
            sqlx::AssertSqlSafe(format!(
                "UPDATE env_registry_env SET {column} = 'not-json' WHERE env_id = ?1"
            )),
        )
        .bind(&committed.id)
        .execute(&raw)
        .await
        .unwrap();

        let observer = SqliteEnvRegistry::open(text, awaken_time::system_clock())
            .await
            .expect("an adapter open never rewrites a corrupt row");
        let on_writer = fail_closed_verdict(&reads(&writer, &committed.id).await)
            .unwrap_or_else(|failure| panic!("{column} writer: {failure}"));
        let on_observer = fail_closed_verdict(&reads(&observer, &committed.id).await)
            .unwrap_or_else(|failure| panic!("{column} observer: {failure}"));
        drop((writer, observer));
        let reopened = SqliteEnvRegistry::open(text, awaken_time::system_clock())
            .await
            .unwrap();
        let on_reopen = fail_closed_verdict(&reads(&reopened, &committed.id).await)
            .unwrap_or_else(|failure| panic!("{column} reopen: {failure}"));
        assert_eq!(on_writer, on_observer, "{column} E1 stable across handles");
        assert_eq!(on_writer, on_reopen, "{column} E1 stable across reopen");
        let stored: String = sqlx::query_scalar(
            // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
            sqlx::AssertSqlSafe(format!(
                "SELECT {column} FROM env_registry_env WHERE env_id = ?1"
            )),
        )
        .bind(&committed.id)
        .fetch_one(&raw)
        .await
        .unwrap();
        assert_eq!(stored, "not-json", "{column} E2 zero overwrite");
        assert_negative_control(column, &committed);
        raw.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_corrupt_rows_fail_closed_on_independent_pools_without_repair() {
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
    for column in COLUMNS {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_env_t37_corrupt_{}_{nonce:x}", std::process::id());
        admin
            .execute(
                // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
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
        let committed = writer
            .create(
                format!("corrupt-{column}"),
                String::new(),
                Default::default(),
                EnvironmentConfig::SelfHosted,
            )
            .await
            .unwrap();
        let raw = connect().await;
        // The registry stores these facts as TEXT on both backends.
        sqlx::query(
            // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
            sqlx::AssertSqlSafe(format!(
                "UPDATE env_registry_env SET {column} = 'not-json' WHERE env_id = $1"
            )),
        )
        .bind(&committed.id)
        .execute(&raw)
        .await
        .unwrap();
        let observer = awaken_env_store::PostgresEnvRegistry::with_existing_pool(
            connect().await,
            awaken_time::system_clock(),
        )
        .await
        .unwrap();
        let on_writer = fail_closed_verdict(&reads(&writer, &committed.id).await)
            .unwrap_or_else(|failure| panic!("{column} writer: {failure}"));
        let on_observer = fail_closed_verdict(&reads(&observer, &committed.id).await)
            .unwrap_or_else(|failure| panic!("{column} observer: {failure}"));
        assert_eq!(on_writer, on_observer, "{column} E1 stable across pools");
        let stored: String = sqlx::query_scalar(
            // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
            sqlx::AssertSqlSafe(format!(
                "SELECT {column}::text FROM env_registry_env WHERE env_id = $1"
            )),
        )
        .bind(&committed.id)
        .fetch_one(&raw)
        .await
        .unwrap();
        assert_eq!(stored, "not-json", "{column} E2 zero overwrite");
        assert_negative_control(column, &committed);
        drop((writer, observer));
        raw.close().await;
        admin
            .execute(
                // Columns are the fixed COLUMNS fault matrix; schema names contain only integer fixture coordinates.
                sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")),
            )
            .await
            .expect("drop only this run's schema");
    }
    admin.close().await;
}

#[test]
fn postgres_corrupt_rows_fail_closed_on_independent_pools_without_repair_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_corrupt_rows_fail_closed_on_independent_pools_without_repair",
    );
}
