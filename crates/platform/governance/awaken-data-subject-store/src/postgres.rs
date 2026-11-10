// @formal entrypoint data_subject.revision_is_monotonic
// @formal entrypoint data_subject.stale_writer_never_applies
// @formal entrypoint erasure.checkpoint_never_forgets_completed_targets
// @formal entrypoint erasure.concurrent_replicas_have_one_checkpoint_winner
//! Postgres adapter for the data-subject application (feature `postgres`, ADR-0050),
//! the network-DB sibling of [`SqliteDataSubjectRepo`](crate::SqliteDataSubjectRepo)
//! over the Control-owned `control_data_subject` migration scope. The subject aggregate
//! serializes into the `data {json}` (jsonb) column; `id`/`org` are keyed columns,
//! so `list`/erasure stay **Org-partitioned** (D7). The *same* portable bundle
//! renders here as on sqlite — the schema is written once.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

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
    let pool = awaken_sql_storage::postgres_pool_options()
        .connect(url)
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|error| PgStoreError::Connect(error.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Verify the scoped ledger on the composition-owned pool.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, PgStoreError> {
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
        let stored_revision = i64::try_from(subject.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let result = sqlx::query("INSERT INTO control_data_subject_subject (id, org, data, revision) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (id) DO NOTHING")
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
        if !subject.is_successor_of(expected_revision) {
            return Err(DataSubjectError::Conflict(subject.id.0));
        }
        let stored_revision = i64::try_from(subject.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let stored_expected = i64::try_from(expected_revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current = sqlx::query(
            "SELECT data, revision FROM control_data_subject_subject WHERE id = $1 FOR UPDATE",
        )
        .bind(&subject.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(current) = current else {
            return Err(DataSubjectError::Conflict(subject.id.0));
        };
        let Json(current_subject): Json<DataSubject> = current.try_get("data").map_err(storage)?;
        let current_revision: i64 = current.try_get("revision").map_err(storage)?;
        if i64::try_from(current_subject.revision).ok() != Some(current_revision) {
            return Err(storage("data subject revision mismatch"));
        }
        if !subject.follows_revision(expected_revision, current_subject.revision) {
            return Err(DataSubjectError::Conflict(subject.id.0));
        }
        let result = sqlx::query(
            "UPDATE control_data_subject_subject SET org = $1, data = $2, revision = $3 \
             WHERE id = $4 AND revision = $5",
        )
        .bind(&subject.org)
        .bind(Json(&subject))
        .bind(stored_revision)
        .bind(&subject.id.0)
        .bind(stored_expected)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if result.rows_affected() == 1 {
            tx.commit().await.map_err(storage)?;
            Ok(())
        } else {
            Err(DataSubjectError::Conflict(subject.id.0))
        }
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        let row = sqlx::query("SELECT data FROM control_data_subject_subject WHERE id = $1")
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| DataSubjectError::NotFound(id.0.clone()))?;
        let Json(subject): Json<DataSubject> = row.try_get("data").map_err(storage)?;
        Ok(subject)
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        // Org-partitioned (ADR-0050 D7): a workspace/org only ever sees its own
        // subjects. `created_at` orders insertion, matching the sqlite `rowid` order.
        let rows = sqlx::query(
            "SELECT data FROM control_data_subject_subject WHERE org = $1 ORDER BY created_at, id",
        )
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
        let row =
            sqlx::query("SELECT data FROM control_data_subject_erasure_job WHERE subject_id = $1")
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Lock an existing row until commit. Absence cannot be row-locked:
        // competing creators are still fenced by the subject primary key and
        // insert-only statement below, on this same transaction connection.
        let current = sqlx::query("SELECT data, revision FROM control_data_subject_erasure_job WHERE subject_id = $1 FOR UPDATE")
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let current = current
            .map(|row| {
                let Json(current): Json<ErasureProgress> = row.try_get("data").map_err(storage)?;
                let revision: i64 = row.try_get("revision").map_err(storage)?;
                if i64::try_from(current.revision).ok() != Some(revision) {
                    return Err(storage("erasure checkpoint revision mismatch"));
                }
                Ok(current)
            })
            .transpose()?;
        if !progress.follows(expected_revision, current.as_ref()) {
            return Err(DataSubjectError::Conflict(id.0.clone()));
        }
        let revision = i64::try_from(progress.revision)
            .map_err(|_| DataSubjectError::RevisionExhausted(id.0.clone()))?;
        let expected = expected_revision
            .map(i64::try_from)
            .transpose()
            .map_err(|_| DataSubjectError::RevisionExhausted(id.0.clone()))?;
        let result =
            match expected {
                None => sqlx::query("INSERT INTO control_data_subject_erasure_job (subject_id, data, revision) VALUES ($1, $2, $3) \
                 ON CONFLICT (subject_id) DO NOTHING")
                .bind(&id.0)
                .bind(Json(progress))
                .bind(revision)
                .execute(&mut *tx)
                .await,
                Some(expected) => {
                    sqlx::query("UPDATE control_data_subject_erasure_job SET data = $2, revision = $3, updated_at = now() \
                 WHERE subject_id = $1 AND revision = $4")
                    .bind(&id.0)
                    .bind(Json(progress))
                    .bind(revision)
                    .bind(expected)
                    .execute(&mut *tx)
                    .await
                }
            }
            .map_err(storage)?;
        if result.rows_affected() == 1 {
            tx.commit().await.map_err(storage)?;
            Ok(())
        } else {
            Err(DataSubjectError::Conflict(id.0.clone()))
        }
    }
}
