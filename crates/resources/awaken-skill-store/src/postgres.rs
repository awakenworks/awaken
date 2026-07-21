//! Postgres [`SkillStore`] over the crate's `skill_store` migration scope — the
//! multi-node sibling of the sqlite backend, the same portable bundle.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::schema::skill_store_bundle;
use crate::{
    SkillAggregate, SkillDefinition, SkillStore, SkillStoreError, SkillVersion, append_to,
    legacy_aggregate, remove_version_from, validate_create,
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

    /// Wrap an existing pool **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `skill_store` scope so this
    /// store reuses the caller's single database instead of a parallel schema.
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the `skill_store` scoped migration bundle (idempotent). Optional:
    /// skip it when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), PgStoreError> {
        run_migrations(&self.pool).await?;
        let rows = sqlx::query(&format!("SELECT workspace_id, id, content FROM {NS}_skill"))
            .fetch_all(&self.pool)
            .await
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
        for row in rows {
            let workspace = row
                .try_get::<String, _>("workspace_id")
                .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
            let id = row
                .try_get::<String, _>("id")
                .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
            let content = row
                .try_get::<String, _>("content")
                .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
            let data =
                serde_json::to_string(&legacy_aggregate(&workspace, &id, content.as_bytes()))
                    .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
            sqlx::query(&format!(
                "INSERT INTO {NS}_aggregate(workspace_id, id, data) VALUES ($1, $2, $3) ON CONFLICT (workspace_id, id) DO NOTHING"
            ))
            .bind(workspace)
            .bind(id)
            .bind(data)
            .execute(&self.pool)
            .await
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
        }
        Ok(())
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
        };
        let data = serde_json::to_string(&aggregate).map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_aggregate (workspace_id, id, data) VALUES ($1, $2, $3)"
        ))
        .bind(&aggregate.definition.workspace_id)
        .bind(&aggregate.definition.id)
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
                SkillStoreError::AlreadyExists(aggregate.definition.id)
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
        let mut aggregate: SkillAggregate = serde_json::from_str(&data).map_err(storage)?;
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
            .map(|aggregate| aggregate.definition))
    }

    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_aggregate WHERE workspace_id = $1 ORDER BY id COLLATE \"C\""
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|r| {
                let data = r.try_get::<String, _>("data").map_err(storage)?;
                serde_json::from_str::<SkillAggregate>(&data)
                    .map(|aggregate| aggregate.definition)
                    .map_err(storage)
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
        let mut aggregate: SkillAggregate = serde_json::from_str(&data).map_err(storage)?;
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
        let r = sqlx::query(&format!(
            "DELETE FROM {NS}_aggregate WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(skill_id)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(r.rows_affected() > 0)
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
            serde_json::from_str(&data).map_err(storage)
        })
        .transpose()
    }
}
