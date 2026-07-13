//! SQLite [`SkillStore`] over the crate's `skill_store` migration scope.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::skill_store_bundle;
use crate::{SkillStore, SkillStoreError, sanitize_stem};

const NS: &str = "skill_store";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `skill_store` scoped migration bundle to the guarded connection.
fn migrate_guarded(conn: &Arc<Mutex<Connection>>) -> Result<(), StoreError> {
    let guard = conn
        .lock()
        .map_err(|_| StoreError::Migrate("skill_store connection poisoned".into()))?;
    let bundle = skill_store_bundle().map_err(|e| StoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|e| StoreError::Migrate(e.to_string()))?
        .run_bundle(&guard, &bundle)
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(())
}

fn storage(err: impl std::fmt::Display) -> SkillStoreError {
    SkillStoreError::Storage(err.to_string())
}

async fn with_conn<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, SkillStoreError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, SkillStoreError> + Send + 'static,
{
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| storage("skill_store poisoned"))?;
        f(&guard)
    })
    .await
    .map_err(storage)?
}

/// A SQLite-backed [`SkillStore`].
pub struct SqliteSkillStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSkillStore {
    /// Open (or create) a database file and apply the skill-store migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call
    /// [`Self::ensure_schema`], or let a unified migration pipeline own the
    /// `skill_store` scope so this store shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `skill_store` scoped migration bundle (idempotent). Optional.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        migrate_guarded(&self.conn)
    }
}

#[async_trait::async_trait]
impl SkillStore for SqliteSkillStore {
    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        content: &str,
    ) -> Result<String, SkillStoreError> {
        let ws = workspace_id.to_string();
        let stem = sanitize_stem(id);
        let content = content.to_string();
        let out = stem.clone();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {NS}_skill (workspace_id, id, content) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(workspace_id, id) DO UPDATE SET content = excluded.content"
                ),
                params![ws, stem, content],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await?;
        Ok(out)
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<String>, SkillStoreError> {
        let ws = workspace_id.to_string();
        let stem = sanitize_stem(id);
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT content FROM {NS}_skill WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, stem],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)
        })
        .await
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<(String, String)>, SkillStoreError> {
        let ws = workspace_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT id, content FROM {NS}_skill WHERE workspace_id = ?1 ORDER BY id"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![ws], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(storage)?;
            rows.map(|row| row.map_err(storage)).collect()
        })
        .await
    }

    async fn delete(&self, workspace_id: &str, id: &str) -> Result<bool, SkillStoreError> {
        let ws = workspace_id.to_string();
        let stem = sanitize_stem(id);
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    &format!("DELETE FROM {NS}_skill WHERE workspace_id = ?1 AND id = ?2"),
                    params![ws, stem],
                )
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `over` wraps a connection without migrating; the store only works once the
    /// caller opts into the `skill_store` scope via `ensure_schema`.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let store = SqliteSkillStore::over(Connection::open_in_memory().unwrap());
        assert!(store.put("ws", "greet", "hi").await.is_err());

        store.ensure_schema().unwrap();
        let id = store.put("ws", "greet", "hi").await.unwrap();
        assert_eq!(store.get("ws", &id).await.unwrap().as_deref(), Some("hi"));

        store.ensure_schema().unwrap(); // idempotent
    }
}
