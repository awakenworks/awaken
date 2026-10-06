use super::*;

#[test]
fn credential_bundle_lints() {
    let bundle = credential_bundle().expect("bundle builds");
    awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
}

/// Cause/effect table for the V9 timestamp backfill, shared by both dialects.
/// C1 a legacy Vault/credential row lacks `created_at`/`updated_at`;
/// C2 a row already carries them; C3 a pending mutation snapshot describes an
/// existing child; C4 a pending create snapshot has no child row yet.
/// E1 (C1) both fields become the row's durable insert instant;
/// E2 (C2) the row is left byte-for-byte untouched;
/// E3 (C3) before/after snapshots take the child row's instant, so they stay
///     equal to the backfilled durable child;
/// E4 (C4) the snapshot takes the mutation row's own insert instant.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod backfill {
    use awaken_credential_contract::CredentialSourceId;
    use awaken_credential_vault::catalog::{
        ManagedCredentialAuth, ManagedCredentialLifecycle, ManagedVault, ManagedVaultCredential,
    };
    use serde_json::Value;

    pub(super) const VAULT_INSERTED: &str = "2025-03-04T05:06:07Z";
    pub(super) const CHILD_INSERTED: &str = "2025-03-04T05:06:08Z";
    pub(super) const PENDING_INSERTED: &str = "2025-03-04T05:06:09Z";
    pub(super) const MODERN: &str = "2026-09-17T10:11:12.345Z";

    fn strip(mut value: Value) -> Value {
        let object = value.as_object_mut().unwrap();
        assert!(object.remove("created_at").is_some());
        assert!(object.remove("updated_at").is_some());
        value
    }

    fn vault(id: &str) -> ManagedVault {
        ManagedVault {
            id: id.into(),
            workspace_id: "ws".into(),
            display_name: id.into(),
            metadata: Default::default(),
            created_at: MODERN.into(),
            updated_at: MODERN.into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        }
    }

    fn child(id: &str) -> ManagedVaultCredential {
        ManagedVaultCredential {
            id: id.into(),
            vault_id: "legacy-vault".into(),
            workspace_id: "ws".into(),
            source_id: CredentialSourceId(format!("source:{id}")),
            auth: ManagedCredentialAuth::StaticBearer {
                mcp_server_url: "https://mcp.example.test".into(),
            },
            metadata: Default::default(),
            display_name: None,
            created_at: MODERN.into(),
            updated_at: MODERN.into(),
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        }
    }

    /// Legacy `(id, data)` rows for the vault and credential tables, plus one
    /// modern row per table that must survive unchanged.
    pub(super) fn vault_rows() -> [(String, Value); 2] {
        [
            (
                "legacy-vault".into(),
                strip(serde_json::to_value(vault("legacy-vault")).unwrap()),
            ),
            (
                "modern-vault".into(),
                serde_json::to_value(vault("modern-vault")).unwrap(),
            ),
        ]
    }

    pub(super) fn credential_rows() -> [(String, Value); 2] {
        [
            (
                "legacy-child".into(),
                strip(serde_json::to_value(child("legacy-child")).unwrap()),
            ),
            (
                "modern-child".into(),
                serde_json::to_value(child("modern-child")).unwrap(),
            ),
        ]
    }

    /// Pending mutation snapshots, JSON-shaped exactly as persisted: an update
    /// over the existing legacy child and a create with no child row.
    pub(super) fn mutation_rows() -> [(String, Value); 2] {
        let legacy_child = strip(serde_json::to_value(child("legacy-child")).unwrap());
        let unborn = strip(serde_json::to_value(child("unborn-child")).unwrap());
        [
            (
                "source:legacy-child".into(),
                serde_json::json!({
                    "operation_id": "update",
                    "before_credential": legacy_child.clone(),
                    "after_credential": legacy_child,
                }),
            ),
            (
                "source:unborn-child".into(),
                serde_json::json!({
                    "operation_id": "create",
                    "before_credential": null,
                    "after_credential": unborn,
                }),
            ),
        ]
    }

    fn decoded_times(value: &Value) -> (String, String) {
        let credential: ManagedVaultCredential =
            serde_json::from_value(value.clone()).expect("backfilled snapshot decodes");
        (credential.created_at, credential.updated_at)
    }

    pub(super) fn assert_backfilled(
        vaults: &[(String, Value)],
        credentials: &[(String, Value)],
        mutations: &[(String, Value)],
    ) {
        let at = |instant: &str| (instant.to_owned(), instant.to_owned());
        for (id, data) in vaults {
            let vault: ManagedVault = serde_json::from_value(data.clone()).unwrap();
            let expected = if id == "legacy-vault" {
                VAULT_INSERTED
            } else {
                MODERN
            };
            assert_eq!(
                (vault.created_at, vault.updated_at),
                at(expected),
                "E1/E2 {id}"
            );
        }
        for (id, data) in credentials {
            let expected = if id == "legacy-child" {
                CHILD_INSERTED
            } else {
                MODERN
            };
            assert_eq!(decoded_times(data), at(expected), "E1/E2 {id}");
        }
        let durable_child: ManagedVaultCredential = serde_json::from_value(
            credentials
                .iter()
                .find(|(id, _)| id == "legacy-child")
                .unwrap()
                .1
                .clone(),
        )
        .unwrap();
        for (source_id, data) in mutations {
            if source_id == "source:legacy-child" {
                assert_eq!(
                    decoded_times(&data["before_credential"]),
                    at(CHILD_INSERTED)
                );
                assert_eq!(decoded_times(&data["after_credential"]), at(CHILD_INSERTED));
                let before: ManagedVaultCredential =
                    serde_json::from_value(data["before_credential"].clone()).unwrap();
                assert_eq!(before, durable_child, "E3 snapshot equals durable child");
            } else {
                assert_eq!(data["before_credential"], Value::Null);
                assert_eq!(
                    decoded_times(&data["after_credential"]),
                    at(PENDING_INSERTED),
                    "E4"
                );
            }
        }
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_backfills_legacy_managed_timestamps_from_insert_time() {
    use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
    use backfill::*;

    let pool = awaken_sqlite_pool::EmbeddedSqlite::memory()
        .open()
        .await
        .unwrap();
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), "credential").unwrap();
    runner
        .run_bundle(&credential_bundle_through(SPECS.len()).unwrap())
        .await
        .unwrap();
    let sqlite_time = |instant: &str| instant.replace('T', " ").replace('Z', "");
    for (id, data) in vault_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_vault (id, workspace_id, data, created_at) \
             VALUES (?1, 'ws', ?2, ?3)",
        )
        .bind(id)
        .bind(data.to_string())
        .bind(sqlite_time(VAULT_INSERTED))
        .execute(&pool)
        .await
        .unwrap();
    }
    for (id, data) in credential_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_vault_credential \
             (id, vault_id, workspace_id, source_id, data, created_at) \
             VALUES (?1, 'legacy-vault', 'ws', ?2, ?3, ?4)",
        )
        .bind(id)
        .bind(data["source_id"].as_str().unwrap().to_owned())
        .bind(data.to_string())
        .bind(sqlite_time(CHILD_INSERTED))
        .execute(&pool)
        .await
        .unwrap();
    }
    for (source_id, data) in mutation_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_credential_mutation (source_id, data, created_at) \
             VALUES (?1, ?2, ?3)",
        )
        .bind(source_id)
        .bind(data.to_string())
        .bind(sqlite_time(PENDING_INSERTED))
        .execute(&pool)
        .await
        .unwrap();
    }
    let modern_data = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT data FROM credential_managed_vault WHERE id = 'modern-vault'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let modern_before = modern_data().await;

    runner
        .run_bundle(&credential_bundle().unwrap())
        .await
        .unwrap();

    let rows = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, String)>(sql)
                .fetch_all(&pool)
                .await
                .unwrap()
                .into_iter()
                .map(|(id, data)| (id, serde_json::from_str(&data).unwrap()))
                .collect::<Vec<(String, serde_json::Value)>>()
        }
    };
    assert_backfilled(
        &rows("SELECT id, data FROM credential_managed_vault").await,
        &rows("SELECT id, data FROM credential_managed_vault_credential").await,
        &rows("SELECT source_id, data FROM credential_managed_credential_mutation").await,
    );
    let modern_after = modern_data().await;
    assert_eq!(modern_before, modern_after, "E2 modern row untouched");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_backfills_legacy_managed_timestamps_from_insert_time() {
    use awaken_scoped_migration::postgres::PostgresMigrationRunner;
    use backfill::*;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::{Executor, Row};

    const SCHEMA: &str = "credential_timestamp_backfill";
    let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    });
    let admin = match PgPoolOptions::new().max_connections(1).connect(&url).await {
        Ok(pool) => pool,
        Err(error) => {
            assert!(
                std::env::var_os("AWAKEN_TEST_DATABASE_URL").is_none(),
                "explicit PostgreSQL fixture is unavailable: {error}"
            );
            println!("[skip] no Postgres reachable: {error}");
            return;
        }
    };
    admin
        .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
        .await
        .unwrap();
    admin
        .execute(format!("CREATE SCHEMA {SCHEMA}").as_str())
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute(format!("SET search_path = {SCHEMA}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let runner = PostgresMigrationRunner::with_prefix(pool.clone(), "credential").unwrap();
    runner
        .run_bundle(&credential_bundle_through(SPECS.len()).unwrap())
        .await
        .unwrap();
    for (id, data) in vault_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_vault (id, workspace_id, data, created_at) \
             VALUES ($1, 'ws', $2, $3::timestamptz)",
        )
        .bind(id)
        .bind(sqlx::types::Json(data))
        .bind(VAULT_INSERTED)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (id, data) in credential_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_vault_credential \
             (id, vault_id, workspace_id, source_id, data, created_at) \
             VALUES ($1, 'legacy-vault', 'ws', $2, $3, $4::timestamptz)",
        )
        .bind(id)
        .bind(data["source_id"].as_str().unwrap().to_owned())
        .bind(sqlx::types::Json(data))
        .bind(CHILD_INSERTED)
        .execute(&pool)
        .await
        .unwrap();
    }
    for (source_id, data) in mutation_rows() {
        sqlx::query(
            "INSERT INTO credential_managed_credential_mutation (source_id, data, created_at) \
             VALUES ($1, $2, $3::timestamptz)",
        )
        .bind(source_id)
        .bind(sqlx::types::Json(data))
        .bind(PENDING_INSERTED)
        .execute(&pool)
        .await
        .unwrap();
    }

    runner
        .run_bundle(&credential_bundle().unwrap())
        .await
        .unwrap();

    let rows = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sql)
                .fetch_all(&pool)
                .await
                .unwrap()
                .into_iter()
                .map(|row| {
                    let sqlx::types::Json(data): sqlx::types::Json<serde_json::Value> =
                        row.try_get(1).unwrap();
                    (row.try_get::<String, _>(0).unwrap(), data)
                })
                .collect::<Vec<_>>()
        }
    };
    assert_backfilled(
        &rows("SELECT id, data FROM credential_managed_vault").await,
        &rows("SELECT id, data FROM credential_managed_vault_credential").await,
        &rows("SELECT source_id, data FROM credential_managed_credential_mutation").await,
    );
}
