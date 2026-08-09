//! Postgres adapter for the data-subject application (feature `postgres`, ADR-0050),
//! the network-DB sibling of [`SqliteDataSubjectRepo`](crate::SqliteDataSubjectRepo)
//! over the Control-owned `control_data_subject` migration scope. The subject aggregate
//! serializes into the `data {json}` (jsonb) column; `id`/`org` are keyed columns,
//! so `list`/erasure stay **Org-partitioned** (D7). The *same* portable bundle
//! renders here as on sqlite — the schema is written once.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::schema::{CONTROL_PREFIX, control_data_subject_bundle};
use awaken_data_subject_application::{
    DataSubject, DataSubjectError, DataSubjectId, DataSubjectRepo, ErasureJobRepo, ErasureProgress,
};

/// The component's table namespace (its bundle prefix).
const NS: &str = CONTROL_PREFIX;

/// Errors from connecting or migrating the Postgres store.
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
    #[error("schema: {0}")]
    Schema(String),
}

async fn connect_migrated(url: &str) -> Result<PgPool, PgStoreError> {
    let pool = PgPool::connect(url)
        .await
        .map_err(|e| PgStoreError::Connect(e.to_string()))?;
    pool_migrated(pool).await
}

async fn pool_migrated(pool: PgPool) -> Result<PgPool, PgStoreError> {
    let bundle =
        control_data_subject_bundle().map_err(|error| PgStoreError::Migrate(error.to_string()))?;
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
    /// Connect and apply the Control data-subject migrations under the
    /// `control_data_subject` namespace.
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: connect_migrated(url).await?,
        })
    }

    /// Build from an existing pool: apply the Control data-subject migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }

    /// Connect to a schema owned by the deployment migration command.
    pub async fn connect_existing(url: &str) -> Result<Self, PgStoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| PgStoreError::Connect(error.to_string()))?;
        let bundle = control_data_subject_bundle()
            .map_err(|error| PgStoreError::Schema(error.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| PgStoreError::Schema(error.to_string()))?
            .verify_bundle(&bundle)
            .await
            .map_err(|error| PgStoreError::Schema(error.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait::async_trait]
impl DataSubjectRepo for PgDataSubjectRepo {
    async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let p = NS;
        let stored_revision = i64::try_from(subject.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let result = sqlx::query(&format!(
            "INSERT INTO {p}_subject (id, org, data, revision) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(&subject.id.0)
        .bind(&subject.org)
        .bind(Json(&subject))
        .bind(stored_revision)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(DataSubjectError::AlreadyExists(subject.id.0))
        }
    }

    async fn compare_and_swap(
        &self,
        expected_revision: u64,
        subject: DataSubject,
    ) -> Result<(), DataSubjectError> {
        if expected_revision.checked_add(1) != Some(subject.revision) {
            return Err(DataSubjectError::Conflict(subject.id.0));
        }
        let stored_revision = i64::try_from(subject.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let stored_expected = i64::try_from(expected_revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let p = NS;
        let result = sqlx::query(&format!(
            "UPDATE {p}_subject SET org = $1, data = $2, revision = $3 \
             WHERE id = $4 AND revision = $5"
        ))
        .bind(&subject.org)
        .bind(Json(&subject))
        .bind(stored_revision)
        .bind(&subject.id.0)
        .bind(stored_expected)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(DataSubjectError::Conflict(subject.id.0))
        }
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
}

#[async_trait::async_trait]
impl ErasureJobRepo for PgDataSubjectRepo {
    async fn load(&self, id: &DataSubjectId) -> Result<Option<ErasureProgress>, DataSubjectError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT data FROM {p}_erasure_job WHERE subject_id = $1"
        ))
        .bind(&id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            let Json(progress): Json<ErasureProgress> = row.try_get("data").map_err(storage)?;
            Ok(progress)
        })
        .transpose()
    }

    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError> {
        if !progress.follows(expected_revision) {
            return Err(DataSubjectError::Conflict(id.0.clone()));
        }
        let revision = i64::try_from(progress.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(id.0.clone()))?;
        let expected = expected_revision
            .map(i64::try_from)
            .transpose()
            .map_err(|_| DataSubjectError::RevisionExhausted(id.0.clone()))?;
        let p = NS;
        let result =
            match expected {
                None => sqlx::query(&format!(
                    "INSERT INTO {p}_erasure_job (subject_id, data, revision) VALUES ($1, $2, $3) \
                 ON CONFLICT (subject_id) DO NOTHING"
                ))
                .bind(&id.0)
                .bind(Json(progress))
                .bind(revision)
                .execute(&self.pool)
                .await,
                Some(expected) => {
                    sqlx::query(&format!(
                        "UPDATE {p}_erasure_job SET data = $2, revision = $3, updated_at = now() \
                 WHERE subject_id = $1 AND revision = $4"
                    ))
                    .bind(&id.0)
                    .bind(Json(progress))
                    .bind(revision)
                    .bind(expected)
                    .execute(&self.pool)
                    .await
                }
            }
            .map_err(storage)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(DataSubjectError::Conflict(id.0.clone()))
        }
    }
}
