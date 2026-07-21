//! SQLite [`SkillStore`] over the crate's `skill_store` migration scope.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::skill_store_bundle;
use crate::{
    SkillAggregate, SkillDefinition, SkillStore, SkillStoreError, SkillVersion, append_to,
    legacy_aggregate, remove_version_from, validate_create,
};

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
        migrate_guarded(&self.conn)?;
        let connection = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("skill_store connection poisoned".into()))?;
        let mut statement = connection
            .prepare(&format!("SELECT workspace_id, id, content FROM {NS}_skill"))
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| StoreError::Migrate(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        drop(statement);
        for (workspace, id, content) in rows {
            let data =
                serde_json::to_string(&legacy_aggregate(&workspace, &id, content.as_bytes()))
                    .map_err(|error| StoreError::Migrate(error.to_string()))?;
            connection
                .execute(
                    &format!("INSERT OR IGNORE INTO {NS}_aggregate(workspace_id, id, data) VALUES (?1, ?2, ?3)"),
                    params![workspace, id, data],
                )
                .map_err(|error| StoreError::Migrate(error.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SkillStore for SqliteSkillStore {
    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        validate_create(&definition, &initial_version)?;
        let aggregate = SkillAggregate {
            definition,
            versions: std::collections::BTreeMap::from([(
                initial_version.version,
                initial_version,
            )]),
            retired_versions: Default::default(),
        };
        let ws = aggregate.definition.workspace_id.clone();
        let id = aggregate.definition.id.clone();
        let data = serde_json::to_string(&aggregate).map_err(storage)?;
        with_conn(&self.conn, move |conn| {
            conn.execute(
                &format!("INSERT INTO {NS}_aggregate (workspace_id, id, data) VALUES (?1, ?2, ?3)"),
                params![ws, id, data],
            )
            .map(|_| ())
            .map_err(|error| {
                if error.to_string().contains("UNIQUE constraint failed") {
                    SkillStoreError::AlreadyExists(id)
                } else {
                    storage(error)
                }
            })
        })
        .await
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            let transaction = conn.unchecked_transaction().map_err(storage)?;
            let data = transaction
                .query_row(
                    &format!("SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                    params![ws, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
                .ok_or_else(|| SkillStoreError::NotFound(id.clone()))?;
            let mut aggregate: SkillAggregate = serde_json::from_str(&data).map_err(storage)?;
            append_to(&mut aggregate, version)?;
            let data = serde_json::to_string(&aggregate).map_err(storage)?;
            transaction
                .execute(
                    &format!(
                        "UPDATE {NS}_aggregate SET data = ?3 WHERE workspace_id = ?1 AND id = ?2"
                    ),
                    params![ws, id, data],
                )
                .map_err(storage)?;
            transaction.commit().map_err(storage)
        })
        .await
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|data| {
                serde_json::from_str::<SkillAggregate>(&data)
                    .map(|value| value.definition)
                    .map_err(storage)
            })
            .transpose()
        })
        .await
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        let ws = workspace_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 ORDER BY id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map(params![ws], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| {
                let data = row.map_err(storage)?;
                serde_json::from_str::<SkillAggregate>(&data)
                    .map(|aggregate| aggregate.definition)
                    .map_err(storage)
            })
            .collect()
        })
        .await
    }

    async fn version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<Option<SkillVersion>, SkillStoreError> {
        Ok(self
            .load(workspace_id, skill_id)
            .await?
            .and_then(|aggregate| aggregate.versions.get(&version).cloned()))
    }

    async fn list_versions(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        Ok(self
            .load(workspace_id, skill_id)
            .await?
            .map(|aggregate| {
                aggregate
                    .versions
                    .into_iter()
                    .filter(|(version, _)| !aggregate.retired_versions.contains(version))
                    .map(|(_, version)| version)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn delete_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<bool, SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            let transaction = conn.unchecked_transaction().map_err(storage)?;
            let Some(data) = transaction
                .query_row(
                    &format!("SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                    params![ws, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
            else {
                return Ok(false);
            };
            let mut aggregate: SkillAggregate = serde_json::from_str(&data).map_err(storage)?;
            let removed = remove_version_from(&mut aggregate, version)?;
            if removed {
                let data = serde_json::to_string(&aggregate).map_err(storage)?;
                transaction
                    .execute(
                        &format!("UPDATE {NS}_aggregate SET data = ?3 WHERE workspace_id = ?1 AND id = ?2"),
                        params![ws, id, data],
                    )
                    .map_err(storage)?;
            }
            transaction.commit().map_err(storage)?;
            Ok(removed)
        })
        .await
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    &format!("DELETE FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                    params![ws, id],
                )
                .map_err(storage)?;
            Ok(n > 0)
        })
        .await
    }
}

impl SqliteSkillStore {
    async fn load(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillAggregate>, SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate() -> (SkillDefinition, SkillVersion) {
        let files = vec![crate::SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\ndescription: test\n---\nbody".to_vec(),
        }];
        (
            SkillDefinition {
                id: "greet".into(),
                workspace_id: "ws".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
            },
            SkillVersion {
                id: "skver-greet-1".into(),
                skill_id: "greet".into(),
                version: 1,
                name: "greet".into(),
                description: "test".into(),
                directory: "/skills/greet".into(),
                bundle_sha256: crate::bundle_sha256(&files),
                files,
            },
        )
    }

    /// `over` wraps a connection without migrating; the store only works once the
    /// caller opts into the `skill_store` scope via `ensure_schema`.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let store = SqliteSkillStore::over(Connection::open_in_memory().unwrap());
        let (definition, version) = aggregate();
        assert!(store.create(definition, version).await.is_err());

        store.ensure_schema().unwrap();
        let (definition, version) = aggregate();
        store.create(definition, version).await.unwrap();
        assert!(store.definition("ws", "greet").await.unwrap().is_some());

        store.ensure_schema().unwrap(); // idempotent
    }

    #[tokio::test]
    async fn ensure_schema_imports_v1_current_content_idempotently() {
        let store = SqliteSkillStore::open_in_memory().unwrap();
        {
            let connection = store.conn.lock().unwrap();
            connection
                .execute(
                    "INSERT INTO skill_store_skill(workspace_id, id, content) VALUES (?1, ?2, ?3)",
                    params!["ws-old", "legacy", "# legacy"],
                )
                .unwrap();
        }
        store.ensure_schema().unwrap();
        store.ensure_schema().unwrap();
        assert_eq!(
            store.list_versions("ws-old", "legacy").await.unwrap().len(),
            1
        );
    }
}
