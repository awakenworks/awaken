//! Postgres adapter for the data-subject domain (feature `postgres`, ADR-0050),
//! the network-DB sibling of [`SqliteDataSubjectRepo`](crate::SqliteDataSubjectRepo)
//! over the crate's own `data_subject` migration scope
//! ([`data_subject_bundle`](crate::data_subject_bundle)). The subject aggregate
//! serializes into the `data {json}` (jsonb) column; `id`/`org` are keyed columns,
//! so `list`/erasure stay **Org-partitioned** (D7). The *same* portable bundle
//! renders here as on sqlite — the schema is written once.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::schema::data_subject_bundle;
use crate::{DataSubject, DataSubjectError, DataSubjectId, DataSubjectRepo};

/// The component's table namespace (its bundle prefix).
const NS: &str = "data_subject";

/// Errors from connecting or migrating the Postgres store.
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

pub(crate) async fn connect_migrated(url: &str) -> Result<PgPool, PgStoreError> {
    let pool = PgPool::connect(url)
        .await
        .map_err(|e| PgStoreError::Connect(e.to_string()))?;
    pool_migrated(pool).await
}

pub(crate) async fn pool_migrated(pool: PgPool) -> Result<PgPool, PgStoreError> {
    let bundle = data_subject_bundle().map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    Ok(pool)
}

fn storage(err: impl std::fmt::Display) -> DataSubjectError {
    DataSubjectError::Storage(err.to_string())
}

/// A Postgres-backed [`DataSubjectRepo`].
pub struct PgDataSubjectRepo {
    pool: PgPool,
}

impl PgDataSubjectRepo {
    /// Connect and apply the data-subject migrations under the `data_subject` namespace.
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: connect_migrated(url).await?,
        })
    }

    /// Build from an existing pool: apply the data-subject migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }
}

#[async_trait::async_trait]
impl DataSubjectRepo for PgDataSubjectRepo {
    async fn put(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_subject (id, org, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET org = excluded.org, data = excluded.data"
        ))
        .bind(&subject.id.0)
        .bind(&subject.org)
        .bind(Json(&subject))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        let p = NS;
        let row = sqlx::query(&format!("SELECT data FROM {p}_subject WHERE id = $1"))
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| DataSubjectError::NotFound(id.0.clone()))?;
        let Json(subject): Json<DataSubject> = row.try_get("data").map_err(storage)?;
        Ok(subject)
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        let p = NS;
        // Org-partitioned (ADR-0050 D7): a workspace/org only ever sees its own
        // subjects. `created_at` orders insertion, matching the sqlite `rowid` order.
        let rows = sqlx::query(&format!(
            "SELECT data FROM {p}_subject WHERE org = $1 ORDER BY created_at, id"
        ))
        .bind(org)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                let Json(subject): Json<DataSubject> = row.try_get("data").map_err(storage)?;
                Ok(subject)
            })
            .collect()
    }

    async fn delete(&self, id: &DataSubjectId) -> Result<(), DataSubjectError> {
        let p = NS;
        sqlx::query(&format!("DELETE FROM {p}_subject WHERE id = $1"))
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
}
