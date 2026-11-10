// @formal entrypoint checkpoint.crash_preserves_durable_truth
//! PostgreSQL persistence for interrupted inference stream checkpoints.

use async_trait::async_trait;
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::postgres::{StoreError, migrate, verify_schema};

/// PostgreSQL storage for an interrupted inference stream.
///
/// The dispatch authority holds the claim epoch lock while this adapter is
/// called. Keeping the mutable checkpoint in the same scoped runtime schema
/// makes it survive coordinator and worker replacement without adding another
/// database contract.
pub struct PostgresStreamCheckpointStore {
    pool: PgPool,
}

impl PostgresStreamCheckpointStore {
    /// Connect and apply the shared runtime-dispatch migration bundle.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Connect to the already-migrated shared runtime-dispatch schema.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_existing_pool(pool).await
    }

    /// Build from an existing pool after applying the shared runtime schema.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Build from an existing pool after verifying the externally-owned ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, StoreError> {
        verify_schema(&pool).await?;
        Ok(Self { pool })
    }

    pub(crate) fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl StreamCheckpointStore for PostgresStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        let row =
            match sqlx::query("SELECT checkpoint FROM runtime_stream_checkpoint WHERE run_id = $1")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await
            {
                // Only a query that ran and returned no row is evidence of absence.
                Ok(None) => return Observed::Absent,
                Ok(Some(row)) => row,
                Err(error) => return Observed::Unknown(UnknownCause::Storage(error.to_string())),
            };
        // The row exists; a column that will not decode is corruption, not
        // emptiness, so it must not read as "no checkpoint".
        match row.try_get::<Json<StreamCheckpoint>, _>("checkpoint") {
            Ok(value) => Observed::Present(value.0),
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }

    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        let run_id = checkpoint.run_id.clone();
        sqlx::query(
            "INSERT INTO runtime_stream_checkpoint (run_id, checkpoint) VALUES ($1, $2) \
             ON CONFLICT (run_id) DO UPDATE SET checkpoint = EXCLUDED.checkpoint",
        )
        .bind(run_id)
        .bind(Json(checkpoint))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        sqlx::query("DELETE FROM runtime_stream_checkpoint WHERE run_id = $1")
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }
}
