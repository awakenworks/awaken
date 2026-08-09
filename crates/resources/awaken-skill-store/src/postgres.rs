//! Postgres [`SkillStore`] over the crate's `skill_store` migration scope — the
//! multi-node sibling of the sqlite backend, the same portable bundle.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::schema::skill_store_bundle;
use crate::{
    SkillAggregate, SkillDefinition, SkillStore, SkillStoreError, SkillVersion, append_to,
    decode_aggregate, remove_version_from, validate_create,
};

const NS: &str = "skill_store";

/// Errors from connecting or migrating the Postgres store.
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `skill_store` scoped migration bundle to `pool` (idempotent).
async fn run_migrations(pool: &PgPool) -> Result<(), PgStoreError> {
    let bundle = skill_store_bundle().map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    Ok(())
}

/// Verify the externally-owned `skill_store` bundle without executing DDL.
async fn verify_migrations(pool: &PgPool) -> Result<(), PgStoreError> {
    let bundle = skill_store_bundle().map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?
        .verify_bundle(&bundle)
        .await
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    Ok(())
}

fn storage(err: impl std::fmt::Display) -> SkillStoreError {
    SkillStoreError::Storage(err.to_string())
}

/// A Postgres-backed [`SkillStore`].
pub struct PgSkillStore {
    pool: PgPool,
}

impl PgSkillStore {
    /// Connect and apply the skill-store migrations under the `skill_store`
    /// namespace (one-step convenience for a store-owned database).
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| PgStoreError::Connect(e.to_string()))?;
        let store = Self::with_pool(pool);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Connect to an already-migrated schema without executing DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, PgStoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| PgStoreError::Connect(error.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Wrap an existing pool after verifying its scoped migration ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, PgStoreError> {
        verify_migrations(&pool).await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `skill_store` scope so this
    /// store reuses the caller's single database instead of a parallel schema.
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the `skill_store` scoped migration bundle (idempotent). Optional:
    /// skip it when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), PgStoreError> {
        run_migrations(&self.pool).await
    }

    async fn workspace_snapshot(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillAggregate>, SkillStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT id, data FROM {NS}_aggregate WHERE workspace_id = $1 ORDER BY id COLLATE \"C\""
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut aggregates = Vec::new();
        for row in rows {
            let id = row.try_get::<String, _>("id").map_err(storage)?;
            let data = row.try_get::<String, _>("data").map_err(storage)?;
            let aggregate = decode_aggregate(data.as_bytes(), workspace_id, &id)?;
            if !aggregate.deleted {
                aggregates.push(aggregate);
            }
        }
        Ok(aggregates)
    }
}

#[async_trait::async_trait]
impl SkillStore for PgSkillStore {
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
        let data = serde_json::to_string(&aggregate).map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_aggregate (workspace_id, id, data) VALUES ($1, $2, $3)"
        ))
        .bind(aggregate.definition.workspace_id.as_str())
        .bind(aggregate.definition.id.as_str())
        .bind(data)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| {
            if error
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref()
                == Some("23505")
            {
                SkillStoreError::AlreadyExists(aggregate.definition.id.to_string())
            } else {
                storage(error)
            }
        })
    }

    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError> {
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_aggregate WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage)?
        .ok_or_else(|| SkillStoreError::NotFound(skill_id.into()))?;
        let data = row.try_get::<String, _>("data").map_err(storage)?;
        let mut aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        append_to(&mut aggregate, version)?;
        let data = serde_json::to_string(&aggregate).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {NS}_aggregate SET data = $3 WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .bind(data)
        .execute(&mut *transaction)
        .await
        .map_err(storage)?;
        transaction.commit().await.map_err(storage)
    }

    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError> {
        Ok(self
            .load(workspace_id, skill_id)
            .await?
            .filter(|aggregate| !aggregate.deleted)
            .map(|aggregate| aggregate.definition))
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
        let Some(row) = sqlx::query(&format!(
            "SELECT data FROM {NS}_aggregate WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage)?
        else {
            return Ok(false);
        };
        let data = row.try_get::<String, _>("data").map_err(storage)?;
        let mut aggregate = decode_aggregate(data.as_bytes(), workspace_id, skill_id)?;
        let removed = remove_version_from(&mut aggregate, version)?;
        if removed {
            let data = serde_json::to_string(&aggregate).map_err(storage)?;
            sqlx::query(&format!(
                "UPDATE {NS}_aggregate SET data = $3 WHERE workspace_id = $1 AND id = $2"
            ))
            .bind(workspace_id)
            .bind(skill_id)
            .bind(data)
            .execute(&mut *transaction)
            .await
            .map_err(storage)?;
        }
        transaction.commit().await.map_err(storage)?;
        Ok(removed)
    }

    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError> {
        let Some(mut aggregate) = self.load(workspace_id, skill_id).await? else {
            return Ok(false);
        };
        if aggregate.deleted {
            return Ok(false);
        }
        aggregate.deleted = true;
        let data = serde_json::to_string(&aggregate).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {NS}_aggregate SET data = $3 WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(true)
    }

    async fn purge_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<u64, SkillStoreError> {
        let Some(aggregate) = self.load(workspace_id, skill_id).await? else {
            return Ok(0);
        };
        if !aggregate.deleted {
            return Err(SkillStoreError::Invalid(
                "an active Skill cannot be physically reclaimed".into(),
            ));
        }
        sqlx::query(&format!(
            "DELETE FROM {NS}_aggregate WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(aggregate.versions.len() as u64)
    }
}

impl PgSkillStore {
    async fn load(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillAggregate>, SkillStoreError> {
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_aggregate WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            let data = row.try_get::<String, _>("data").map_err(storage)?;
            decode_aggregate(data.as_bytes(), workspace_id, skill_id)
        })
        .transpose()
    }
}
