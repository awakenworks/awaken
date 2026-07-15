//! Postgres [`SkillStore`] over the crate's `skill_store` migration scope — the
//! multi-node sibling of the sqlite backend, the same portable bundle.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::schema::skill_store_bundle;
use crate::{SkillStore, SkillStoreError, sanitize_stem};

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
        run_migrations(&self.pool).await
    }
}

#[async_trait::async_trait]
impl SkillStore for PgSkillStore {
    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        content: &str,
    ) -> Result<String, SkillStoreError> {
        let stem = sanitize_stem(id);
        sqlx::query(&format!(
            "INSERT INTO {NS}_skill (workspace_id, id, content) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, id) DO UPDATE SET content = excluded.content"
        ))
        .bind(workspace_id)
        .bind(&stem)
        .bind(content)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(stem)
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<String>, SkillStoreError> {
        let row = sqlx::query(&format!(
            "SELECT content FROM {NS}_skill WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(sanitize_stem(id))
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|r| r.try_get::<String, _>("content").map_err(storage))
            .transpose()
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<(String, String)>, SkillStoreError> {
        let rows = sqlx::query(&format!(
            // COLLATE "C" = raw byte order, matching the fs/in-mem/sqlite backends.
            // Without it Postgres sorts by the DB's default collation (e.g. en_US),
            // which reorders mixed-case sanitized stems and breaks the SkillStore
            // contract that every backend's `list` is byte-for-byte comparable.
            "SELECT id, content FROM {NS}_skill WHERE workspace_id = $1 ORDER BY id COLLATE \"C\""
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("id").map_err(storage)?,
                    r.try_get::<String, _>("content").map_err(storage)?,
                ))
            })
            .collect()
    }

    async fn delete(&self, workspace_id: &str, id: &str) -> Result<bool, SkillStoreError> {
        let r = sqlx::query(&format!(
            "DELETE FROM {NS}_skill WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(sanitize_stem(id))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(r.rows_affected() > 0)
    }
}
