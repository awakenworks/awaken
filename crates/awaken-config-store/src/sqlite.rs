//! SQLite config-store adapter under the built-in `config` namespace.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::AgentConfig;
use crate::schema::config_bundle;
use crate::store::{ConfigStore, ConfigStoreError, StoredPublication};

/// The config component's table namespace (ADR-0029/ADR-0031). Built in.
const NS: &str = "config";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// A SQLite-backed [`ConfigStore`].
pub struct SqliteConfigStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteConfigStore {
    /// Open (or create) a database file and apply the config migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    /// Open a private in-memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        let bundle = config_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration::sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, ConfigStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, &str) -> Result<T, ConfigStoreError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| ConfigStoreError("config connection poisoned".to_string()))?;
            f(&mut guard, NS)
        })
        .await
        .map_err(|err| ConfigStoreError(err.to_string()))?
    }
}

fn reject(err: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(err.to_string())
}

#[async_trait]
impl ConfigStore for SqliteConfigStore {
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError> {
        let id = config.id.clone();
        let data = serde_json::to_string(config).map_err(reject)?;
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_agent (id, data) VALUES (?1, ?2) \
                     ON CONFLICT(id) DO UPDATE SET data = excluded.data"
                ),
                params![id, data],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        let id = id.to_string();
        self.with_conn(move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_agent WHERE id = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(reject)?;
            data.map(|s| serde_json::from_str(&s).map_err(reject))
                .transpose()
        })
        .await
    }

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        let fingerprint = publication.fingerprint.clone();
        let agent_id = publication.agent_id.clone();
        let state = publication.state.as_str().to_string();
        let record = serde_json::to_string(publication).map_err(reject)?;
        self.with_conn(move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_publication (fingerprint, agent_id, state, record) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(fingerprint) DO NOTHING"
                ),
                params![fingerprint, agent_id, state, record],
            )
            .map_err(reject)?;
            Ok(())
        })
        .await
    }

    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        let fingerprint = fingerprint.to_string();
        self.with_conn(move |conn, p| {
            let record: Option<String> = conn
                .query_row(
                    &format!("SELECT record FROM {p}_publication WHERE fingerprint = ?1"),
                    params![fingerprint],
                    |r| r.get(0),
                )
                .optional()
                .map_err(reject)?;
            record
                .map(|s| serde_json::from_str(&s).map_err(reject))
                .transpose()
        })
        .await
    }
}
