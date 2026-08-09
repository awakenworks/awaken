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
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::repo::{CredentialMutationIntent, CredentialRepo};
use crate::schema::credential_bundle;
use crate::{
    CredentialError, CredentialPool, CredentialPoolId, CredentialSource, CredentialSourceId,
    SealedBlobStore, SecretRef,
};

/// The credential component's table namespace (its bundle prefix).
const NS: &str = "credential";
const FILE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the shared `credential` scoped bundle to a connection. Both stores key the
/// same scope, so this is the migration half they share; the wrap-without-migrate
/// half is each store's `over`.
fn run_migrations(conn: &Connection) -> Result<(), StoreError> {
    let bundle = credential_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(conn, &bundle)
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    Ok(())
}

fn storage(err: impl std::fmt::Display) -> CredentialError {
    CredentialError::Storage(err.to_string())
}

fn open_file_connection(path: &str) -> Result<Connection, StoreError> {
    let connection = Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?;
    connection
        .busy_timeout(FILE_BUSY_TIMEOUT)
        .map_err(|err| StoreError::Open(err.to_string()))?;
    connection
        .execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|err| StoreError::Open(err.to_string()))?;
    Ok(connection)
}

/// Apply the credential bundle once and construct both adapters over the same
/// serialized connection. This is the canonical application startup path: the
/// metadata and sealed-material halves are one credential persistence boundary,
/// so they must not create competing SQLite writers or repeat migration work.
pub fn open_migrated_pair(
    path: &str,
) -> Result<(SqliteCredentialRepo, SqliteSealedBlobStore), StoreError> {
    let connection = open_file_connection(path)?;
    run_migrations(&connection)?;
    let conn = Arc::new(Mutex::new(connection));
    Ok((
        SqliteCredentialRepo { conn: conn.clone() },
        SqliteSealedBlobStore { conn },
    ))
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
    /// Open (or create) a database file and apply the credential migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let store = Self::over(open_file_connection(path)?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(
            Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `credential` scope so this store
    /// shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `credential` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("credential connection poisoned".to_string()))?;
        run_migrations(&conn)
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

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        let id = source.id.0.clone();
        let workspace_id = source.workspace_id.clone();
        let data = serde_json::to_string(&source).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3)"
                ),
                params![id, workspace_id, data],
            )
            .map_err(storage)?;
            get_row(
                conn,
                &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                &source.id.0,
                CredentialError::SourceNotFound,
            )
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

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        let id = intent.after.id.0.clone();
        let data = serde_json::to_string(&intent).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO {p}_creation_intent (source_id, data) VALUES (?1, ?2)"
                ),
                params![id, data],
            )
            .map_err(storage)?;
            let durable: CredentialMutationIntent = get_row(
                conn,
                &format!("SELECT data FROM {p}_creation_intent WHERE source_id = ?1"),
                &id,
                CredentialError::SourceNotFound,
            )?;
            if durable != intent {
                return Err(CredentialError::MutationConflict(
                    "another credential mutation is pending".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        let intent = intent.clone();
        with_conn(&self.conn, move |conn, p| {
            // This mutation reads its durable intent/current revision before it
            // writes. In WAL mode a deferred transaction cannot upgrade an old
            // read snapshot after a sibling adapter commits; SQLite returns BUSY
            // immediately instead of honoring the configured busy timeout. Take
            // the write reservation up front so bounded sibling contention waits
            // at transaction start and the read-to-write snapshot stays valid.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let intent_data: Option<String> = tx
                .query_row(
                    &format!("SELECT data FROM {p}_creation_intent WHERE source_id = ?1"),
                    params![intent.after.id.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            let durable: CredentialMutationIntent = intent_data
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "credential mutation has no matching durable intent".into(),
                    )
                })?;
            if durable != intent {
                return Err(CredentialError::MutationConflict(
                    "credential mutation does not match durable intent".into(),
                ));
            }
            let current_data: Option<String> = tx
                .query_row(
                    &format!("SELECT data FROM {p}_source WHERE id = ?1"),
                    params![intent.after.id.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            let current: Option<CredentialSource> = current_data
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            if current.as_ref() == Some(&intent.after) {
                tx.commit().map_err(storage)?;
                return Ok(());
            }
            if current != intent.before {
                return Err(CredentialError::MutationConflict(
                    "credential revision changed during mutation".into(),
                ));
            }
            let id = intent.after.id.0.clone();
            let workspace_id = intent.after.workspace_id.clone();
            let data = serde_json::to_string(&intent.after).map_err(storage)?;
            tx.execute(
                &format!(
                    "INSERT INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET workspace_id = excluded.workspace_id, data = excluded.data"
                ),
                params![id, workspace_id, data],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)
        })
        .await
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        with_conn(&self.conn, move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT data FROM {p}_creation_intent ORDER BY created_at, source_id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(storage))
                .collect()
        })
        .await
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!("DELETE FROM {p}_creation_intent WHERE source_id = ?1"),
                params![id],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn material_refs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        with_conn(&self.conn, move |conn, p| {
            let mut statement = conn
                .prepare(&format!("SELECT data FROM {p}_source ORDER BY id"))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            let sources: Vec<CredentialSource> = rows
                .map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(storage))
                .collect::<Result<_, _>>()?;
            Ok(sources
                .into_iter()
                .flat_map(|source| {
                    source
                        .material_ref
                        .into_iter()
                        .chain(source.auxiliary_material_refs.into_values())
                })
                .collect())
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
    /// Open (or create) a database file and apply the credential migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let store = Self::over(open_file_connection(path)?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(
            Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `credential` scope so this store
    /// shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `credential` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("credential connection poisoned".to_string()))?;
        run_migrations(&conn)
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

    async fn delete_blob(&self, r: &SecretRef) -> Result<(), CredentialError> {
        let key = r.0.clone();
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!("DELETE FROM {p}_secret WHERE secret_ref = ?1"),
                params![key],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn inventory_blobs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        with_conn(&self.conn, move |conn, p| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT secret_ref FROM {p}_secret ORDER BY secret_ref"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| row.map(SecretRef).map_err(storage))
                .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialKind, CredentialStatus};

    fn source(id: &str) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(id.into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            protocol_endpoint_id: None,
            env_key: Some("ANTHROPIC_API_KEY".into()),
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    /// Both stores' injection seam: a caller-owned connection, `over` wraps it WITHOUT
    /// migrating, `ensure_schema` applies the shared `credential` scope — so a unified
    /// migration pipeline can own the schema and these stores share the caller's
    /// database (mirrors the resource-family stores). Round-trips the repo and the
    /// sealed blob store, which key the same scope.
    #[tokio::test]
    async fn over_then_ensure_schema_round_trips_on_a_caller_owned_connection() {
        let repo = SqliteCredentialRepo::over(Connection::open_in_memory().unwrap());
        repo.ensure_schema().unwrap();
        repo.put(source("cred:a")).await.unwrap();
        assert_eq!(repo.list("ws").await.unwrap().len(), 1);

        let blobs = SqliteSealedBlobStore::over(Connection::open_in_memory().unwrap());
        blobs.ensure_schema().unwrap();
        let r = SecretRef("sec:a".into());
        blobs.put_blob(&r, b"sealed".to_vec()).await.unwrap();
        assert_eq!(blobs.get_blob(&r).await.unwrap(), b"sealed");
    }

    #[tokio::test]
    async fn file_stores_wait_for_short_lived_sibling_writer_contention() {
        // Cause/effect decision table:
        // | Rule | adapter | sibling write lock | Effect |
        // | R1 | metadata | absent | normal write succeeds (covered by conformance) |
        // | R2 | metadata | short-lived | waits, then writes without leaking SQLITE_BUSY |
        // | R3 | sealed blob | short-lived | waits, then writes without leaking SQLITE_BUSY |
        // | R4 | metadata read | long-lived | WAL snapshot reads without waiting for the writer |
        // | R5 | mutation read-to-write | short-lived | reserves the writer first, waits, then commits without SQLITE_BUSY |
        // A lock held longer than the bounded timeout still fails closed; this test
        // covers the transient write contention and concurrent provisioning reads
        // produced by sibling adapters in one host.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.db");
        let path = path.to_string_lossy().into_owned();
        let repo = SqliteCredentialRepo::open(&path).unwrap();
        let blobs = SqliteSealedBlobStore::open(&path).unwrap();

        for write in ["metadata", "blob"] {
            let blocker_path = path.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let blocker = std::thread::spawn(move || {
                let connection = Connection::open(blocker_path).unwrap();
                connection.execute_batch("BEGIN IMMEDIATE").unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                connection.execute_batch("COMMIT").unwrap();
            });
            ready_rx.recv().unwrap();
            if write == "metadata" {
                repo.put(source("cred:contended")).await.unwrap();
            } else {
                blobs
                    .put_blob(&SecretRef("sec:contended".into()), b"sealed".to_vec())
                    .await
                    .unwrap();
            }
            blocker.join().unwrap();
        }

        let mut contended = source("cred:mutation-contended");
        contended.provider_id = Some("provider-before".into());
        repo.put(contended.clone()).await.unwrap();
        let mut updated = contended.clone();
        updated.provider_id = Some("provider-after".into());
        updated.version += 1;
        let intent = CredentialMutationIntent {
            before: Some(contended),
            after: updated.clone(),
        };
        repo.begin_mutation(intent.clone()).await.unwrap();
        let blocker_path = path.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let connection = Connection::open(blocker_path).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            connection
                .execute(
                    "INSERT INTO credential_secret (secret_ref, sealed) VALUES (?1, ?2)",
                    params!["sec:mutation-writer", b"sealed"],
                )
                .unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            connection.execute_batch("COMMIT").unwrap();
        });
        ready_rx.recv().unwrap();
        repo.apply_mutation(&intent).await.unwrap();
        blocker.join().unwrap();
        assert_eq!(
            repo.get(&CredentialSourceId("cred:mutation-contended".into()))
                .await
                .unwrap(),
            updated,
            "R5"
        );

        repo.put(source("cred:read-during-write")).await.unwrap();
        let blocker_path = path.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let connection = Connection::open(blocker_path).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            connection
                .execute(
                    "INSERT INTO credential_secret (secret_ref, sealed) VALUES (?1, ?2)",
                    params!["sec:writer", b"sealed"],
                )
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            connection.execute_batch("COMMIT").unwrap();
        });
        ready_rx.recv().unwrap();
        let read = tokio::time::timeout(
            Duration::from_millis(500),
            repo.get(&CredentialSourceId("cred:read-during-write".into())),
        )
        .await
        .expect("WAL reader must not wait for a sibling writer")
        .unwrap();
        assert_eq!(read.id.0, "cred:read-during-write");
        release_tx.send(()).unwrap();
        blocker.join().unwrap();
    }

    #[tokio::test]
    async fn migrated_pair_owns_one_credential_persistence_boundary() {
        // Cause/effect decision table:
        // | Rule | startup path | metadata write | sealed write | Effect |
        // | R1 | canonical pair | succeeds | succeeds | both halves share one migrated scope |
        // | R2 | standalone seam | succeeds | n/a | retained for isolated adapter tests |
        // | R3 | standalone seam | n/a | succeeds | retained for isolated adapter tests |
        // Standalone coverage lives above; this rule prevents application
        // composition from reopening and migrating one bounded context twice.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.db");
        let path = path.to_string_lossy().into_owned();
        let (repo, blobs) = open_migrated_pair(&path).unwrap();

        repo.put(source("cred:paired")).await.unwrap();
        let reference = SecretRef("sec:paired".into());
        blobs
            .put_blob(&reference, b"sealed".to_vec())
            .await
            .unwrap();

        assert_eq!(
            repo.get(&CredentialSourceId("cred:paired".into()))
                .await
                .unwrap()
                .id
                .0,
            "cred:paired"
        );
        assert_eq!(blobs.get_blob(&reference).await.unwrap(), b"sealed");
    }
}
