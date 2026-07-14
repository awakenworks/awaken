//! Durable [`EnvRegistry`] backends: the self-hosted environment registry over a
//! store, so an environment (user-created config) survives a restart and is visible
//! to a worker on any node. SQLite (embedded) and Postgres (distributed) share one
//! portable bundle, the sibling of [`crate::work_store`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_protocol_managed::env_registry::{EnvItem, EnvRegistry, EnvUpdate};
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

/// The frozen presence timestamp the managed wire uses (parity with the registry).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The store's table namespace / bundle prefix (`env_registry_env`).
const NS: &str = "env_registry";

fn env_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.env_registry",
        vec![Migration::new(
            1,
            "self-hosted environment registry: one row per environment",
            "CREATE TABLE {prefix}_env (\
             env_id        TEXT PRIMARY KEY, \
             seq           BIGINT NOT NULL, \
             name          TEXT NOT NULL, \
             description   TEXT NOT NULL, \
             metadata_json TEXT NOT NULL, \
             config_json   TEXT NOT NULL, \
             archived_at   TEXT)",
        )?],
    )
}

/// The columns an env row projects to an [`EnvItem`], in `SELECT` order.
const COLS: &str = "env_id, name, description, metadata_json, config_json, archived_at";

fn metadata_str(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).expect("env metadata serializes")
}

fn config_str(c: &Value) -> String {
    serde_json::to_string(c).expect("env config serializes")
}

fn decode(
    id: String,
    name: String,
    description: String,
    metadata_json: &str,
    config_json: &str,
    archived_at: Option<String>,
) -> EnvItem {
    EnvItem {
        id,
        name,
        description,
        metadata: serde_json::from_str(metadata_json).unwrap_or_default(),
        config: serde_json::from_str(config_json).unwrap_or(Value::Null),
        archived_at,
    }
}

fn sqlite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnvItem> {
    let metadata_json: String = row.get(3)?;
    let config_json: String = row.get(4)?;
    Ok(decode(
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        &metadata_json,
        &config_json,
        row.get(5)?,
    ))
}

fn pg_row(row: &PgRow) -> EnvItem {
    let metadata_json: String = row.get("metadata_json");
    let config_json: String = row.get("config_json");
    decode(
        row.get("env_id"),
        row.get("name"),
        row.get("description"),
        &metadata_json,
        &config_json,
        row.get("archived_at"),
    )
}

/// SQLite persistence for the environment registry.
pub struct SqliteEnvRegistry {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteEnvRegistry {
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn read(tx: &Transaction<'_>, id: &str) -> Option<EnvItem> {
        tx.query_row(
            &format!("SELECT {COLS} FROM env_registry_env WHERE env_id = ?1"),
            params![id],
            sqlite_row,
        )
        .optional()
        .expect("read env row")
    }
}

#[async_trait]
impl EnvRegistry for SqliteEnvRegistry {
    async fn create(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        config: Value,
    ) -> EnvItem {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM env_registry_env",
                [],
                |r| r.get(0),
            )
            .expect("next seq");
        let id = format!("env_{next:016}");
        tx.execute(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                next,
                name,
                description,
                metadata_str(&metadata),
                config_str(&config)
            ],
        )
        .expect("insert env");
        tx.commit().expect("commit create");
        EnvItem {
            id,
            name,
            description,
            metadata,
            config,
            archived_at: None,
        }
    }

    async fn list_active(&self) -> Vec<EnvItem> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
            ))
            .expect("prepare list");
        let rows = stmt.query_map([], sqlite_row).expect("query list");
        rows.map(|r| r.expect("row")).collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard.transaction().expect("begin");
        Self::read(&tx, id)
    }

    async fn exists(&self, id: &str) -> bool {
        self.get(id).await.is_some()
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let mut item = Self::read(&tx, id)?;
        item.apply(patch);
        tx.execute(
            "UPDATE env_registry_env SET name = ?1, description = ?2, metadata_json = ?3, \
             config_json = ?4 WHERE env_id = ?5",
            params![
                item.name,
                item.description,
                metadata_str(&item.metadata),
                config_str(&item.config),
                id
            ],
        )
        .expect("update env");
        tx.commit().expect("commit update");
        Some(item)
    }

    async fn delete(&self, id: &str) -> bool {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        conn.execute(
            "DELETE FROM env_registry_env WHERE env_id = ?1",
            params![id],
        )
        .expect("delete env")
            > 0
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let mut item = Self::read(&tx, id)?;
        item.archived_at = Some(OBJECT_AT.to_string());
        tx.execute(
            "UPDATE env_registry_env SET archived_at = ?1 WHERE env_id = ?2",
            params![OBJECT_AT, id],
        )
        .expect("archive env");
        tx.commit().expect("commit archive");
        Some(item)
    }
}

/// Postgres persistence for the environment registry (distributed deployments).
pub struct PostgresEnvRegistry {
    pool: PgPool,
}

impl PostgresEnvRegistry {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool })
    }

    async fn read(&self, id: &str) -> Option<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .expect("read env row")
        .map(|r| pg_row(&r))
    }
}

#[async_trait]
impl EnvRegistry for PostgresEnvRegistry {
    async fn create(
        &self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        config: Value,
    ) -> EnvItem {
        let mut tx = self.pool.begin().await.expect("begin");
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM env_registry_env")
                .fetch_one(&mut *tx)
                .await
                .expect("next seq");
        let id = format!("env_{next:016}");
        sqlx::query(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(next)
        .bind(&name)
        .bind(&description)
        .bind(metadata_str(&metadata))
        .bind(config_str(&config))
        .execute(&mut *tx)
        .await
        .expect("insert env");
        tx.commit().await.expect("commit create");
        EnvItem {
            id,
            name,
            description,
            metadata,
            config,
            archived_at: None,
        }
    }

    async fn list_active(&self) -> Vec<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .expect("query list")
        .iter()
        .map(pg_row)
        .collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        self.read(id).await
    }

    async fn exists(&self, id: &str) -> bool {
        self.read(id).await.is_some()
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut item = self.read(id).await?;
        item.apply(patch);
        sqlx::query(
            "UPDATE env_registry_env SET name = $1, description = $2, metadata_json = $3, \
             config_json = $4 WHERE env_id = $5",
        )
        .bind(&item.name)
        .bind(&item.description)
        .bind(metadata_str(&item.metadata))
        .bind(config_str(&item.config))
        .bind(id)
        .execute(&self.pool)
        .await
        .expect("update env");
        Some(item)
    }

    async fn delete(&self, id: &str) -> bool {
        sqlx::query("DELETE FROM env_registry_env WHERE env_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .expect("delete env")
            .rows_affected()
            > 0
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut item = self.read(id).await?;
        item.archived_at = Some(OBJECT_AT.to_string());
        sqlx::query("UPDATE env_registry_env SET archived_at = $1 WHERE env_id = $2")
            .bind(OBJECT_AT)
            .bind(id)
            .execute(&self.pool)
            .await
            .expect("archive env");
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r() -> SqliteEnvRegistry {
        SqliteEnvRegistry::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn create_get_list_archive_survive_the_store() {
        let r = r();
        let e = r
            .create(
                "prod".into(),
                "d".into(),
                BTreeMap::new(),
                json!({"type":"self_hosted"}),
            )
            .await;
        assert!(e.is_self_hosted());
        let got = r.get(&e.id).await.expect("get");
        assert_eq!(got.name, "prod");
        assert_eq!(r.list_active().await.len(), 1);
        r.archive(&e.id).await.expect("archive");
        assert!(
            r.list_active().await.is_empty(),
            "archived drops from active"
        );
        assert!(r.get(&e.id).await.is_some(), "still retrievable");
    }

    #[tokio::test]
    async fn update_patches_and_metadata_null_deletes() {
        let r = r();
        let e = r
            .create(
                "e".into(),
                String::new(),
                BTreeMap::from([("keep".into(), "1".into()), ("drop".into(), "2".into())]),
                json!({"networking":{"type":"none"}}),
            )
            .await;
        let up = r
            .update(
                &e.id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(up.name, "renamed");
        assert!(up.metadata.contains_key("keep"));
        assert!(!up.metadata.contains_key("drop"), "null deletes");
        // config preserved through the round-trip
        assert!(up.network_policy().is_restricted());
    }

    #[tokio::test]
    async fn delete_reports_existence() {
        let r = r();
        let e = r
            .create("e".into(), String::new(), BTreeMap::new(), json!({}))
            .await;
        assert!(r.exists(&e.id).await);
        assert!(r.delete(&e.id).await);
        assert!(!r.delete(&e.id).await, "second delete is false");
        assert!(!r.exists(&e.id).await);
    }
}
