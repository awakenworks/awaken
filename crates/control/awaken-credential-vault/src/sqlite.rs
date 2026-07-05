//! SQLite adapters (feature `sqlite`, ADR-0043 sqlite-repos) for the credential
//! domain, over the crate's own `credential` migration scope
//! ([`credential_bundle`]): [`SqliteCredentialRepo`] persists the **secret-free**
//! source/pool rows (serde in the `data {json}` column, keyed columns for
//! lookups), and [`SqliteSealedBlobStore`] persists opaque sealed blobs in
//! `{prefix}_secret`.
//!
//! Deliberately **no bare SQLite [`SecretStore`](crate::SecretStore)** is
//! exposed: it would write plaintext at rest — a footgun this module refuses to
//! offer. The one durable secret path is the AEAD decorator over the blob port:
//! `SealedAeadSecretStore::over(&key, Arc::new(SqliteSealedBlobStore::open(..)?))`
//! (features `sealed-aead` + `sqlite`), which stores only `nonce ‖ ciphertext`.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::repo::CredentialRepo;
use crate::schema::credential_bundle;
use crate::{
    CredentialError, CredentialPool, CredentialPoolId, CredentialSource, CredentialSourceId,
    SealedBlobStore, SecretRef,
};

/// The credential component's table namespace (its bundle prefix).
const NS: &str = "credential";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

fn open_migrated(conn: Connection) -> Result<Arc<Mutex<Connection>>, StoreError> {
    let bundle = credential_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&conn, &bundle)
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn storage(err: impl std::fmt::Display) -> CredentialError {
    CredentialError::Storage(err.to_string())
}

async fn with_conn<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, CredentialError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection, &str) -> Result<T, CredentialError> + Send + 'static,
{
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = conn
            .lock()
            .map_err(|_| storage("credential connection poisoned"))?;
        f(&mut guard, NS)
    })
    .await
    .map_err(storage)?
}

/// One JSON row by primary key, deserialized; `not_found` shapes the miss.
fn get_row<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    key: &str,
    not_found: impl FnOnce(String) -> CredentialError,
) -> Result<T, CredentialError> {
    let data: Option<String> = conn
        .query_row(sql, params![key], |r| r.get(0))
        .optional()
        .map_err(storage)?;
    let data = data.ok_or_else(|| not_found(key.to_string()))?;
    serde_json::from_str(&data).map_err(storage)
}

/// All JSON rows of one workspace, in insertion order.
fn list_rows<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    workspace_id: &str,
) -> Result<Vec<T>, CredentialError> {
    let mut stmt = conn.prepare(sql).map_err(storage)?;
    let rows = stmt
        .query_map(params![workspace_id], |r| r.get::<_, String>(0))
        .map_err(storage)?;
    rows.map(|data| serde_json::from_str(&data.map_err(storage)?).map_err(storage))
        .collect()
}

/// A SQLite-backed [`CredentialRepo`] (secret-free rows only; the sealed
/// material goes through [`SqliteSealedBlobStore`]).
pub struct SqliteCredentialRepo {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCredentialRepo {
    /// Open (or create) a database file and apply the credential migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }
}

#[async_trait::async_trait]
impl CredentialRepo for SqliteCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        let id = source.id.0.clone();
        let workspace_id = source.workspace_id.clone();
        let data = serde_json::to_string(&source).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET \
                     workspace_id = excluded.workspace_id, data = excluded.data"
                ),
                params![id, workspace_id, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            get_row(
                conn,
                &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                &id,
                CredentialError::SourceNotFound,
            )
        })
        .await
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        let workspace_id = workspace_id.to_string();
        with_conn(&self.conn, move |conn, p| {
            list_rows(
                conn,
                &format!("SELECT data FROM {p}_source WHERE workspace_id = ?1 ORDER BY rowid"),
                &workspace_id,
            )
        })
        .await
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        let id = pool.id.0.clone();
        let workspace_id = pool.workspace_id.clone();
        let data = serde_json::to_string(&pool).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_pool (id, workspace_id, data) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET \
                     workspace_id = excluded.workspace_id, data = excluded.data"
                ),
                params![id, workspace_id, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            get_row(
                conn,
                &format!("SELECT data FROM {p}_pool WHERE id = ?1"),
                &id,
                CredentialError::PoolNotFound,
            )
        })
        .await
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        let workspace_id = workspace_id.to_string();
        with_conn(&self.conn, move |conn, p| {
            list_rows(
                conn,
                &format!("SELECT data FROM {p}_pool WHERE workspace_id = ?1 ORDER BY rowid"),
                &workspace_id,
            )
        })
        .await
    }
}

/// A SQLite-backed [`SealedBlobStore`]: opaque `nonce ‖ ciphertext` blobs in
/// `{prefix}_secret`, keyed by [`SecretRef`]. Not a `SecretStore` — compose it
/// under `SealedAeadSecretStore::over` so only sealed bytes ever hit the disk.
pub struct SqliteSealedBlobStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSealedBlobStore {
    /// Open (or create) a database file and apply the credential migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }
}

#[async_trait::async_trait]
impl SealedBlobStore for SqliteSealedBlobStore {
    async fn put_blob(&self, r: &SecretRef, blob: Vec<u8>) -> Result<(), CredentialError> {
        let key = r.0.clone();
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_secret (secret_ref, sealed) VALUES (?1, ?2) \
                     ON CONFLICT(secret_ref) DO UPDATE SET sealed = excluded.sealed"
                ),
                params![key, blob],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get_blob(&self, r: &SecretRef) -> Result<Vec<u8>, CredentialError> {
        let key = r.0.clone();
        with_conn(&self.conn, move |conn, p| {
            let blob: Option<Vec<u8>> = conn
                .query_row(
                    &format!("SELECT sealed FROM {p}_secret WHERE secret_ref = ?1"),
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            blob.ok_or(CredentialError::SecretNotFound(key))
        })
        .await
    }
}
