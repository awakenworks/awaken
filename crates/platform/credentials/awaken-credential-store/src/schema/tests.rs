use super::*;

#[test]
fn credential_bundle_lints() {
    let bundle = credential_bundle().expect("bundle builds");
    awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn material_close_tail_preserves_existing_v10_ciphertext_as_open() {
    // DT-M: C1 published V10 database with ciphertext; C2 V11 applied/replayed.
    // M1 C1+C2 first => same bytes and open default, all older receipts accepted;
    // M2 C2 replay => one V11 receipt and unchanged material. Upgrading must not
    // reinterpret an existing material row as closed or edit a published checksum.
    use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
    let pool = awaken_sqlite_pool::EmbeddedSqlite::memory()
        .open()
        .await
        .unwrap();
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), "credential").unwrap();
    runner
        .run_bundle(&credential_bundle_through(10).unwrap())
        .await
        .unwrap();
    let bytes = vec![1_u8, 2, 3];
    sqlx::query("INSERT INTO credential_secret(secret_ref,sealed) VALUES('legacy-material',?1)")
        .bind(&bytes)
        .execute(&pool)
        .await
        .unwrap();
    for _ in 0..2 {
        runner
            .run_bundle(&credential_bundle().unwrap())
            .await
            .unwrap();
        let actual: (Vec<u8>, i64) = sqlx::query_as(
            "SELECT sealed,closed FROM credential_secret WHERE secret_ref='legacy-material'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            actual,
            (bytes.clone(), 0),
            "M1/M2 legacy material remains open"
        );
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn receipt_delivery_tail_preserves_legacy_pending_and_malformed_rows() {
    use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
    // Causes: C1 V9 exact pending payload; C2 undecodable SQLite payload; C3
    // apply V10 then replay migration. Effects: E1 both rows and their raw
    // payloads survive byte-for-byte; E2 default delivery is pending; E3 the
    // new indexed lifecycle is idempotent. A malformed row cannot make index
    // construction fail or silently vanish from supervised inspection.
    let pool = awaken_sqlite_pool::EmbeddedSqlite::memory()
        .open()
        .await
        .unwrap();
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), "credential").unwrap();
    runner
        .run_bundle(&credential_bundle_through(9).unwrap())
        .await
        .unwrap();
    let payloads = [
        (
            "legacy",
            r#"{"id":"legacy","workspace_id":"w","vault_id":"v","credential_id":"c","source_id":"s","source_version":2,"credential_revision":2,"operation":"update"}"#,
        ),
        ("poison", "{"),
    ];
    for (id, data) in payloads {
        sqlx::query(
            "INSERT INTO credential_managed_credential_rollout(event_id,data) VALUES(?1,?2)",
        )
        .bind(id)
        .bind(data)
        .execute(&pool)
        .await
        .unwrap();
    }
    runner
        .run_bundle(&credential_bundle().unwrap())
        .await
        .unwrap();
    runner
        .run_bundle(&credential_bundle().unwrap())
        .await
        .unwrap();
    for (id, data) in payloads {
        let actual = sqlx::query_as::<_, (String, bool)>(
            "SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=?1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            actual,
            (data.into(), false),
            "{id}: raw evidence retained, pending"
        );
    }
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
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("configured PostgreSQL backfill fixture must connect");
    admin
        .execute("DROP SCHEMA IF EXISTS credential_timestamp_backfill CASCADE")
        .await
        .unwrap();
    admin
        .execute("CREATE SCHEMA credential_timestamp_backfill")
        .await
        .unwrap();
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(SCHEMA)
                    .execute(conn)
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
    // V10 receipt-tail causes: one legacy pending event and one future-shaped
    // JSONB record exist before the tail. Both raw semantic payloads survive,
    // default Pending, and the new indexes tolerate unsupported record shapes.
    let receipt_rows = [
        (
            "receipt-legacy",
            serde_json::json!({"id":"receipt-legacy","workspace_id":"w","vault_id":"v","credential_id":"c","source_id":"s","source_version":2,"credential_revision":2,"operation":"update"}),
        ),
        ("receipt-future", serde_json::json!({"format_version":2})),
    ];
    for (id, data) in &receipt_rows {
        sqlx::query(
            "INSERT INTO credential_managed_credential_rollout(event_id,data) VALUES($1,$2)",
        )
        .bind(id)
        .bind(sqlx::types::Json(data))
        .execute(&pool)
        .await
        .unwrap();
    }
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

    runner
        .run_bundle(&credential_bundle().unwrap())
        .await
        .unwrap();
    for (id, data) in &receipt_rows {
        let (sqlx::types::Json(actual), delivered) = sqlx::query_as::<
            _,
            (sqlx::types::Json<serde_json::Value>, bool),
        >(
            "SELECT data,delivered FROM credential_managed_credential_rollout WHERE event_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (&actual, delivered),
            (data, false),
            "{id}: receipt evidence retained across idempotent V10 migration"
        );
    }

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
