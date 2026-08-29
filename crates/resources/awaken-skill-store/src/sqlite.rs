//! SQLite [`SkillStore`] over the crate's `skill_store` migration scope.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{BUNDLE_ID, converged_skill_store_bundle, selected_skill_store_bundle};
#[cfg(test)]
use crate::schema::{expanded_skill_store_bundle, skill_store_bundle};
use crate::{
    SkillAggregate, SkillDefinition, SkillStore, SkillStoreError, SkillVersion, append_to,
    decode_aggregate, remove_version_from, validate_create,
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
    let ledger_exists = guard
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [format!("{NS}_schema_migrations")],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let v1_checksum = if ledger_exists {
        guard
            .query_row(
                &format!(
                    "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
                ),
                [BUNDLE_ID],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| StoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    let published = selected_skill_store_bundle(v1_checksum.as_deref())
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let converged =
        converged_skill_store_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
    let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&guard, &published)
        .and_then(|_| runner.run_bundle(&guard, &converged))
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
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

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
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

    async fn workspace_snapshot(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillAggregate>, SkillStoreError> {
        let ws = workspace_id.to_string();
        with_conn(&self.conn, move |conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT id, data FROM {NS}_aggregate WHERE workspace_id = ?1 ORDER BY id"
                ))
                .map_err(storage)?;
            let rows = statement
                .query_map(params![ws], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage)?;
            let mut aggregates = Vec::new();
            for row in rows {
                let (id, data) = row.map_err(storage)?;
                let aggregate = decode_aggregate(data.as_bytes(), &ws, &id)?;
                if !aggregate.deleted {
                    aggregates.push(aggregate);
                }
            }
            Ok(aggregates)
        })
        .await
    }
}

#[async_trait::async_trait]
impl SkillStore for SqliteSkillStore {
    async fn workspace_ids(&self) -> Result<Vec<String>, SkillStoreError> {
        with_conn(&self.conn, move |conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT DISTINCT workspace_id FROM {NS}_aggregate ORDER BY workspace_id"
                ))
                .map_err(storage)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)
        })
        .await
    }

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
            deleted: false,
        };
        let ws = aggregate.definition.workspace_id.to_string();
        let id = aggregate.definition.id.to_string();
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
            let mut aggregate = decode_aggregate(data.as_bytes(), &ws, &id)?;
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
                decode_aggregate(data.as_bytes(), &ws, &id)
                    .map(|value| (!value.deleted).then_some(value.definition))
            })
            .transpose()
            .map(Option::flatten)
        })
        .await
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        Ok(self
            .workspace_snapshot(workspace_id)
            .await?
            .into_iter()
            .map(|aggregate| aggregate.definition)
            .collect())
    }

    async fn snapshot_latest_versions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        self.workspace_snapshot(workspace_id)
            .await?
            .into_iter()
            .map(|aggregate| {
                aggregate
                    .versions
                    .get(&aggregate.definition.latest_version)
                    .cloned()
                    .ok_or_else(|| storage("Skill latest version is missing"))
            })
            .collect()
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
            .filter(|aggregate| !aggregate.deleted)
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
            let mut aggregate = decode_aggregate(data.as_bytes(), &ws, &id)?;
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
            let Some(data) = conn
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
            let mut aggregate = decode_aggregate(data.as_bytes(), &ws, &id)?;
            if aggregate.deleted {
                return Ok(false);
            }
            aggregate.deleted = true;
            let data = serde_json::to_string(&aggregate).map_err(storage)?;
            conn.execute(
                &format!("UPDATE {NS}_aggregate SET data = ?3 WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, id, data],
            )
            .map_err(storage)?;
            Ok(true)
        })
        .await
    }

    async fn purge_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<u64, SkillStoreError> {
        let ws = workspace_id.to_string();
        let id = skill_id.to_string();
        with_conn(&self.conn, move |conn| {
            let Some(data) = conn
                .query_row(
                    &format!("SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                    params![ws, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(storage)?
            else {
                return Ok(0);
            };
            let aggregate = decode_aggregate(data.as_bytes(), &ws, &id)?;
            if !aggregate.deleted {
                return Err(SkillStoreError::Invalid(
                    "an active Skill cannot be physically reclaimed".into(),
                ));
            }
            conn.execute(
                &format!("DELETE FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, id],
            )
            .map_err(storage)?;
            u64::try_from(aggregate.versions.len()).map_err(storage)
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
            .map(|data| decode_aggregate(data.as_bytes(), &ws, &id))
            .transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_baseline_exposes_only_the_skill_aggregate() {
        // Causes: C1 empty ledger; C2 exact V1 replay. Effects: E1 create the
        // aggregate table without the retired current-body projection; E2 apply
        // once; E3 replay no SQL. Decision rules S1=C1=>E1+E2; S2=C2=>E3.
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = skill_store_bundle().expect("bundle builds");
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS).expect("runner");
        let applied = runner.run_bundle(&conn, &full).expect("S1");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            vec![1],
            "S1/E2"
        );
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_store_aggregate'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let retired: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_store_skill'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((active, retired), (1, 0), "S1/E1");
        assert!(runner.run_bundle(&conn, &full).expect("S2").is_empty());
    }

    #[tokio::test]
    async fn expanded_history_converges_without_reviving_retired_projection() {
        // Causes: H1 exact expanded V1/V2 receipts; H2 one legacy projection
        // row; H3 a new aggregate command after convergence. Effects: E1 add
        // only the common receipt; E2 preserve but never read/write V1 data;
        // E3 persist and read the aggregate through the sole SkillStore API.
        // Decision rule X1=H1+H2+H3 => E1+E2+E3.
        let conn = Connection::open_in_memory().expect("open sqlite");
        let expanded = expanded_skill_store_bundle().expect("expanded");
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .expect("runner")
            .run_bundle(&conn, &expanded)
            .expect("H1");
        conn.execute(
            "INSERT INTO skill_store_skill(workspace_id,id,content) VALUES('ws','retired','legacy')",
            [],
        )
        .expect("H2");

        let store = SqliteSkillStore::over(conn);
        store.ensure_schema().expect("E1");
        let (definition, version) = aggregate();
        store.create(definition, version).await.expect("H3/E3");
        assert!(store.definition("ws", "greet").await.unwrap().is_some());

        let guard = store.conn.lock().unwrap();
        let counts: (i64, i64, i64) = guard
            .query_row(
                "SELECT \
                   (SELECT COUNT(*) FROM skill_store_schema_migrations WHERE bundle_id='awaken.skill_store'), \
                   (SELECT COUNT(*) FROM skill_store_schema_migrations WHERE bundle_id='awaken.skill_store.converged'), \
                   (SELECT COUNT(*) FROM skill_store_skill)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (2, 1, 1), "X1 -> E1+E2");
    }

    fn aggregate() -> (SkillDefinition, SkillVersion) {
        let files = vec![crate::SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\ndescription: test\n---\nbody".to_vec(),
            executable: false,
        }];
        (
            SkillDefinition {
                id: "greet".into(),
                workspace_id: "ws".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
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
                created_unix_nanos: 0,
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
}
