//! PostgreSQL persistence for interrupted inference stream checkpoints.

use async_trait::async_trait;
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::postgres::{NS, StoreError, migrate, verify_schema};

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
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|err| StoreError::Connect(err.to_string()))?;
        Self::with_pool(pool).await
    }

    /// Connect to the already-migrated shared runtime-dispatch schema.
    pub async fn connect_existing(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
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
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        let row = sqlx::query(&format!(
            "SELECT checkpoint FROM {NS}_stream_checkpoint WHERE run_id = $1"
        ))
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| StreamCheckpointError::Storage(error.to_string()))?;
        row.map(|row| {
            row.try_get::<Json<StreamCheckpoint>, _>("checkpoint")
                .map(|value| value.0)
                .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
        })
        .transpose()
    }

    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        let run_id = checkpoint.run_id.clone();
        sqlx::query(&format!(
            "INSERT INTO {NS}_stream_checkpoint (run_id, checkpoint) VALUES ($1, $2) \
             ON CONFLICT (run_id) DO UPDATE SET checkpoint = EXCLUDED.checkpoint"
        ))
        .bind(run_id)
        .bind(Json(checkpoint))
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        sqlx::query(&format!(
            "DELETE FROM {NS}_stream_checkpoint WHERE run_id = $1"
        ))
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }
}
