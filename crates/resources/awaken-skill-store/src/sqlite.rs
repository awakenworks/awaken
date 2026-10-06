// @formal evidence skill.pinned_version_survives_retirement
//! SQLite [`SkillStore`] over the crate's `skill_store` migration scope.

use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::SqliteConnection;

use crate::schema::{BUNDLE_ID, converged_skill_store_bundle, selected_skill_store_bundle};
#[cfg(test)]
use crate::schema::{expanded_skill_store_bundle, skill_store_bundle};
use crate::{
    SkillAggregate, SkillDefinition, SkillStore, SkillStoreError, SkillVersion, append_to,
    decode_aggregate, encode_aggregate, remove_version_from,
};

const NS: &str = "skill_store";

/// SQLite extended result codes that identify a uniqueness violation.
const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1555;
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `skill_store` scoped migration bundle to the store's pool.
async fn migrate(pool: &EmbeddedPool) -> Result<(), StoreError> {
    let ledger_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
    )
    .bind(format!("{NS}_schema_migrations"))
    .fetch_one(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let v1_checksum = if ledger_exists {
        sqlx::query_scalar::<_, String>(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    let published = selected_skill_store_bundle(v1_checksum.as_deref())
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let converged =
        converged_skill_store_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&published)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&converged)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    Ok(())
}

fn storage(err: impl std::fmt::Display) -> SkillStoreError {
    SkillStoreError::Indeterminate {
        detail: err.to_string(),
    }
}

fn create_error(error: sqlx::Error, id: String) -> SkillStoreError {
    let duplicate = match &error {
        sqlx::Error::Database(database) => database
            .code()
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| {
                matches!(
                    code,
                    SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY
                )
            }),
        _ => false,
    };
    if duplicate {
        SkillStoreError::AlreadyExists(id)
    } else {
        storage(error)
    }
}

async fn aggregate_data(
    conn: &mut SqliteConnection,
    workspace_id: &str,
    skill_id: &str,
) -> Result<Option<String>, SkillStoreError> {
    sqlx::query_scalar::<_, String>(&format!(
        "SELECT data FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"
    ))
    .bind(workspace_id)
    .bind(skill_id)
    .fetch_optional(conn)
    .await
    .map_err(storage)
}

async fn update_aggregate_data(
    conn: &mut SqliteConnection,
    workspace_id: &str,
    skill_id: &str,
    data: &str,
) -> Result<(), SkillStoreError> {
    sqlx::query(&format!(
        "UPDATE {NS}_aggregate SET data = ?3 WHERE workspace_id = ?1 AND id = ?2"
    ))
    .bind(workspace_id)
    .bind(skill_id)
    .bind(data)
    .execute(conn)
    .await
    .map_err(storage)?;
    Ok(())
}

/// A SQLite-backed [`SkillStore`].
pub struct SqliteSkillStore {
    pool: EmbeddedPool,
}

impl SqliteSkillStore {
    /// Open (or create) a database file and apply the skill-store migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::file(path)).await
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_at(EmbeddedSqlite::memory()).await
    }

    async fn open_at(location: EmbeddedSqlite) -> Result<Self, StoreError> {
        let pool = location
            .open()
            .await
            .map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(pool);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**. Call
    /// [`Self::ensure_schema`], or let a unified migration pipeline own the
    /// `skill_store` scope so this store shares the caller's database.
    pub fn over(pool: EmbeddedPool) -> Self {
        Self { pool }
    }

    /// Apply the `skill_store` scoped migration bundle (idempotent). Optional.
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        migrate(&self.pool).await
    }

    async fn workspace_snapshot(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillAggregate>, SkillStoreError> {
        let rows = sqlx::query_as::<_, (String, String)>(&format!(
            "SELECT id, data FROM {NS}_aggregate WHERE workspace_id = ?1 ORDER BY id"
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut aggregates = Vec::new();
        for (id, data) in rows {
            let aggregate = decode_aggregate(data.as_bytes(), workspace_id, &id)?;
            if !aggregate.deleted {
                aggregates.push(aggregate);
            }
        }
        Ok(aggregates)
    }
}

#[async_trait::async_trait]
impl SkillStore for SqliteSkillStore {
    async fn workspace_ids(&self) -> Result<Vec<String>, SkillStoreError> {
        sqlx::query_scalar::<_, String>(&format!(
            "SELECT DISTINCT workspace_id FROM {NS}_aggregate ORDER BY workspace_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)
    }

    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        definition.validate_initial_version(&initial_version)?;
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
        let data = encode_aggregate(&aggregate)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_aggregate (workspace_id, id, data) VALUES (?1, ?2, ?3)"
        ))
        .bind(&ws)
        .bind(&id)
        .bind(&data)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| create_error(error, id))
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let data = aggregate_data(&mut transaction, workspace_id, skill_id)
            .await?
            .ok_or_else(|| SkillStoreError::NotFound(skill_id.to_string()))?;
        let mut aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        append_to(&mut aggregate, version)?;
        let data = encode_aggregate(&aggregate)?;
        update_aggregate_data(&mut transaction, workspace_id, skill_id, &data).await?;
        transaction.commit().await.map_err(storage)
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        aggregate_data(&mut conn, workspace_id, skill_id)
            .await?
            .map(|data| {
                decode_aggregate(data.as_bytes(), workspace_id, skill_id)
                    .map(|value| (!value.deleted).then_some(value.definition))
            })
            .transpose()
            .map(Option::flatten)
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
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let Some(data) = aggregate_data(&mut transaction, workspace_id, skill_id).await? else {
            return Ok(false);
        };
        let mut aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        let removed = remove_version_from(
            &mut aggregate.definition,
            &aggregate.versions,
            &mut aggregate.retired_versions,
            aggregate.deleted,
            version,
        )?;
        if removed {
            let data = encode_aggregate(&aggregate)?;
            update_aggregate_data(&mut transaction, workspace_id, skill_id, &data).await?;
        }
        transaction.commit().await.map_err(storage)?;
        Ok(removed)
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        let Some(data) = aggregate_data(&mut conn, workspace_id, skill_id).await? else {
            return Ok(false);
        };
        let mut aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        if aggregate.deleted {
            return Ok(false);
        }
        aggregate.deleted = true;
        let data = encode_aggregate(&aggregate)?;
        update_aggregate_data(&mut conn, workspace_id, skill_id, &data).await?;
        Ok(true)
    }

    async fn purge_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<u64, SkillStoreError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        let Some(data) = aggregate_data(&mut conn, workspace_id, skill_id).await? else {
            return Ok(0);
        };
        let aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        if !aggregate.deleted {
            return Err(SkillStoreError::LifecycleConflict {
                detail: "an active Skill cannot be physically reclaimed".into(),
            });
        }
        sqlx::query(&format!(
            "DELETE FROM {NS}_aggregate WHERE workspace_id = ?1 AND id = ?2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
        u64::try_from(aggregate.versions.len()).map_err(storage)
    }
}

impl SqliteSkillStore {
    async fn load(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillAggregate>, SkillStoreError> {
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        aggregate_data(&mut conn, workspace_id, skill_id)
            .await?
            .map(|data| decode_aggregate(data.as_bytes(), workspace_id, skill_id))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver error carrying only an extended result code and a message, so
    /// the mapping can be exercised for codes a test cannot easily provoke.
    #[derive(Debug)]
    struct CodedError {
        code: i32,
        message: &'static str,
    }

    impl std::fmt::Display for CodedError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl std::error::Error for CodedError {}

    impl sqlx::error::DatabaseError for CodedError {
        fn message(&self) -> &str {
            self.message
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.code.to_string().into())
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn create_failure_uses_constraint_identity_not_diagnostic_text() {
        // C1 UNIQUE/PRIMARYKEY => AlreadyExists; C2 BUSY/IO => unknown outcome.
        // R1/R2 cross both innocuous and misleading diagnostic strings; text
        // cannot manufacture a uniqueness receipt or erase the true conflict.
        const SQLITE_BUSY: i32 = 5;
        const SQLITE_IOERR: i32 = 10;
        for (code, duplicate) in [
            (SQLITE_CONSTRAINT_UNIQUE, true),
            (SQLITE_CONSTRAINT_PRIMARYKEY, true),
            (SQLITE_BUSY, false),
            (SQLITE_IOERR, false),
        ] {
            for message in ["different language", "UNIQUE constraint failed"] {
                let error = sqlx::Error::Database(Box::new(CodedError { code, message }));
                let mapped = create_error(error, "skill".into());
                assert_eq!(
                    matches!(mapped, SkillStoreError::AlreadyExists(_)),
                    duplicate
                );
                assert_eq!(
                    matches!(mapped, SkillStoreError::Indeterminate { .. }),
                    !duplicate
                );
            }
        }
    }

    #[tokio::test]
    async fn create_failure_maps_a_real_duplicate_to_already_exists() {
        // The driver reports a real duplicate key with the extended code the
        // mapping above keys on.
        let store = SqliteSkillStore::open_in_memory().await.unwrap();
        let (definition, version) = aggregate();
        store.create(definition, version).await.unwrap();
        let (definition, version) = aggregate();
        assert!(matches!(
            store.create(definition, version).await,
            Err(SkillStoreError::AlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn current_baseline_exposes_only_the_skill_aggregate() {
        // Causes: C1 empty ledger; C2 exact V1 replay. Effects: E1 create the
        // aggregate table without the retired current-body projection; E2 apply
        // once; E3 replay no SQL. Decision rules S1=C1=>E1+E2; S2=C2=>E3.
        let pool = EmbeddedSqlite::memory().open().await.expect("open sqlite");
        let full = skill_store_bundle().expect("bundle builds");
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).expect("runner");
        let applied = runner.run_bundle(&full).await.expect("S1");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            vec![1],
            "S1/E2"
        );
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_store_aggregate'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let retired: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_store_skill'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((active, retired), (1, 0), "S1/E1");
        assert!(runner.run_bundle(&full).await.expect("S2").is_empty());
    }

    #[tokio::test]
    async fn expanded_history_converges_without_reviving_retired_projection() {
        // Causes: H1 exact expanded V1/V2 receipts; H2 one legacy projection
        // row; H3 a new aggregate command after convergence. Effects: E1 add
        // only the common receipt; E2 preserve but never read/write V1 data;
        // E3 persist and read the aggregate through the sole SkillStore API.
        // Decision rule X1=H1+H2+H3 => E1+E2+E3.
        let pool = EmbeddedSqlite::memory().open().await.expect("open sqlite");
        let expanded = expanded_skill_store_bundle().expect("expanded");
        SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .expect("runner")
            .run_bundle(&expanded)
            .await
            .expect("H1");
        sqlx::query(
            "INSERT INTO skill_store_skill(workspace_id,id,content) VALUES('ws','retired','legacy')",
        )
        .execute(&pool)
        .await
        .expect("H2");

        let store = SqliteSkillStore::over(pool);
        store.ensure_schema().await.expect("E1");
        let (definition, version) = aggregate();
        store.create(definition, version).await.expect("H3/E3");
        assert!(store.definition("ws", "greet").await.unwrap().is_some());

        let counts: (i64, i64, i64) = sqlx::query_as(
            "SELECT \
               (SELECT COUNT(*) FROM skill_store_schema_migrations WHERE bundle_id='awaken.skill_store'), \
               (SELECT COUNT(*) FROM skill_store_schema_migrations WHERE bundle_id='awaken.skill_store.converged'), \
               (SELECT COUNT(*) FROM skill_store_skill)",
        )
        .fetch_one(&store.pool)
        .await
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
        let store = SqliteSkillStore::over(EmbeddedSqlite::memory().open().await.unwrap());
        let (definition, version) = aggregate();
        assert!(store.create(definition, version).await.is_err());

        store.ensure_schema().await.unwrap();
        let (definition, version) = aggregate();
        store.create(definition, version).await.unwrap();
        assert!(store.definition("ws", "greet").await.unwrap().is_some());

        store.ensure_schema().await.unwrap(); // idempotent
    }
}
