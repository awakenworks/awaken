//! SQLite adapter for the data-subject domain, over the crate's own
//! `data_subject` migration scope ([`data_subject_bundle`]). The subject
//! aggregate serializes into the `data {json}` column; `id`/`org` are keyed
//! columns for lookups.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::data_subject_bundle;
use crate::{DataSubject, DataSubjectError, DataSubjectId, DataSubjectRepo};

/// The component's table namespace (its bundle prefix).
const NS: &str = "data_subject";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

fn storage(err: impl std::fmt::Display) -> DataSubjectError {
    DataSubjectError::Storage(err.to_string())
}

async fn with_conn<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, DataSubjectError>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection, &str) -> Result<T, DataSubjectError> + Send + 'static,
{
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = conn
            .lock()
            .map_err(|_| storage("data_subject connection poisoned"))?;
        f(&mut guard, NS)
    })
    .await
    .map_err(storage)?
}

/// A SQLite-backed [`DataSubjectRepo`].
pub struct SqliteDataSubjectRepo {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDataSubjectRepo {
    /// Open (or create) a database file and apply the data-subject migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let store =
            Self::over(Connection::open(path).map_err(|err| StoreError::Open(err.to_string()))?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(
            Connection::open_in_memory().map_err(|err| StoreError::Open(err.to_string()))?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `data_subject` scope so this store
    /// shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `data_subject` scoped migration bundle (idempotent). Optional: skip
    /// it when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("data_subject connection poisoned".to_string()))?;
        let bundle = data_subject_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&conn, &bundle)
            .map_err(|err| StoreError::Migrate(err.to_string()))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DataSubjectRepo for SqliteDataSubjectRepo {
    async fn put(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let id = subject.id.0.clone();
        let org = subject.org.clone();
        let data = serde_json::to_string(&subject).map_err(storage)?;
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!(
                    "INSERT INTO {p}_subject (id, org, data) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET org = excluded.org, data = excluded.data"
                ),
                params![id, org, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            let data: Option<String> = conn
                .query_row(
                    &format!("SELECT data FROM {p}_subject WHERE id = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            let data = data.ok_or_else(|| DataSubjectError::NotFound(id.clone()))?;
            serde_json::from_str(&data).map_err(storage)
        })
        .await
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        let org = org.to_string();
        with_conn(&self.conn, move |conn, p| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT data FROM {p}_subject WHERE org = ?1 ORDER BY rowid"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![org], |r| r.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|data| serde_json::from_str(&data.map_err(storage)?).map_err(storage))
                .collect()
        })
        .await
    }

    async fn delete(&self, id: &DataSubjectId) -> Result<(), DataSubjectError> {
        let id = id.0.clone();
        with_conn(&self.conn, move |conn, p| {
            conn.execute(
                &format!("DELETE FROM {p}_subject WHERE id = ?1"),
                params![id],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injection seam: a caller supplies its own connection, `over` wraps it
    /// WITHOUT migrating, and `ensure_schema` applies the `data_subject` scope — so a
    /// unified migration pipeline can own the schema and this store shares the
    /// caller's database (mirrors the resource-family stores' `over`/`ensure_schema`).
    #[tokio::test]
    async fn over_then_ensure_schema_round_trips_on_a_caller_owned_connection() {
        let conn = Connection::open_in_memory().unwrap();
        let repo = SqliteDataSubjectRepo::over(conn);
        repo.ensure_schema().unwrap();
        repo.put(DataSubject::new(
            DataSubjectId("dsub_a".into()),
            "org_1",
            100,
        ))
        .await
        .unwrap();
        assert_eq!(
            repo.get(&DataSubjectId("dsub_a".into())).await.unwrap().org,
            "org_1"
        );
    }
}
