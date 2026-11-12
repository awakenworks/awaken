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

impl PostgresStreamCheckpointStore {
    /// Observe a checkpoint through the caller's existing connection. Query or
    /// decode failure remains Unknown, never successful absence. This operation
    /// does not check out a connection or complete the caller's transaction.
    pub async fn get_on(
        connection: &mut sqlx::PgConnection,
        run_id: &str,
    ) -> Observed<StreamCheckpoint> {
        let row =
            match sqlx::query("SELECT checkpoint FROM runtime_stream_checkpoint WHERE run_id = $1")
                .bind(run_id)
                .fetch_optional(&mut *connection)
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

    /// Write the exact checkpoint in the caller's existing transaction. Only
    /// that caller decides whether the write commits or rolls back.
    pub async fn put_on(
        connection: &mut sqlx::PgConnection,
        checkpoint: StreamCheckpoint,
    ) -> Result<(), StreamCheckpointError> {
        let run_id = checkpoint.run_id.clone();
        sqlx::query(
            "INSERT INTO runtime_stream_checkpoint (run_id, checkpoint) VALUES ($1, $2) \
             ON CONFLICT (run_id) DO UPDATE SET checkpoint = EXCLUDED.checkpoint",
        )
        .bind(run_id)
        .bind(Json(checkpoint))
        .execute(&mut *connection)
        .await
        .map(|_| ())
        .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }

    /// Delete a checkpoint in the caller's existing transaction; absence is the
    /// original idempotent success. No connection or transaction is created.
    pub async fn delete_on(
        connection: &mut sqlx::PgConnection,
        run_id: &str,
    ) -> Result<(), StreamCheckpointError> {
        sqlx::query("DELETE FROM runtime_stream_checkpoint WHERE run_id = $1")
            .bind(run_id)
            .execute(&mut *connection)
            .await
            .map(|_| ())
            .map_err(|error| StreamCheckpointError::Storage(error.to_string()))
    }
}

#[async_trait]
impl StreamCheckpointStore for PostgresStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        let mut connection = match self.pool.acquire().await {
            Ok(connection) => connection,
            Err(error) => return Observed::Unknown(UnknownCause::Storage(error.to_string())),
        };
        Self::get_on(&mut connection, run_id).await
    }

    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| StreamCheckpointError::Storage(error.to_string()))?;
        Self::put_on(&mut connection, checkpoint).await
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| StreamCheckpointError::Storage(error.to_string()))?;
        Self::delete_on(&mut connection, run_id).await
    }
}

#[cfg(test)]
mod borrowed_tests {
    use super::*;

    #[tokio::test]
    async fn postgres_borrowed_checkpoints_keep_caller_rollback_and_unknown_observations() {
        // Cause/effect table: B1 caller-owned RW transaction + put/get/delete
        // => its same txid sees every uncommitted effect; B2 rollback => original
        // checkpoint and sentinel are unchanged. B3 malformed row or aborted
        // query => Unknown, never absence or a partial successful checkpoint.
        // The sentinel and original value are independent ownership oracles:
        // an inner commit or a second pooled connection cannot satisfy B2.
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
        let store = PostgresStreamCheckpointStore::with_pool(pool.clone())
            .await
            .unwrap();
        let original = awaken_store_conformance::stream_checkpoint::sample("borrowed-original");
        store.put(original.clone()).await.unwrap();
        sqlx::query("CREATE TABLE checkpoint_borrowed_sentinel (value INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let txid: i64 = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO checkpoint_borrowed_sentinel VALUES (1)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let replacement = awaken_store_conformance::stream_checkpoint::sample("borrowed-pending");
        PostgresStreamCheckpointStore::put_on(&mut tx, replacement.clone())
            .await
            .unwrap();
        assert_eq!(
            PostgresStreamCheckpointStore::get_on(&mut tx, &replacement.run_id).await,
            Observed::Present(replacement.clone()),
            "B1 caller sees its own uncommitted checkpoint"
        );
        PostgresStreamCheckpointStore::delete_on(&mut tx, &original.run_id)
            .await
            .unwrap();
        assert_eq!(
            PostgresStreamCheckpointStore::get_on(&mut tx, &original.run_id).await,
            Observed::Absent,
            "B1 exact delete is visible only in the caller transaction"
        );
        let same_txid: i64 = sqlx::query_scalar("SELECT txid_current()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(same_txid, txid, "B1 no inner transaction completion");
        sqlx::query(
            "UPDATE runtime_stream_checkpoint SET checkpoint = '{}'::jsonb WHERE run_id = $1",
        )
        .bind(&replacement.run_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert!(
            matches!(
                PostgresStreamCheckpointStore::get_on(&mut tx, &replacement.run_id).await,
                Observed::Unknown(_)
            ),
            "B3 malformed data is not absence"
        );
        assert!(
            sqlx::query("SELECT 1 / 0").execute(&mut *tx).await.is_err(),
            "B3 the caller's transaction is actually aborted"
        );
        assert!(
            matches!(
                PostgresStreamCheckpointStore::get_on(&mut tx, &original.run_id).await,
                Observed::Unknown(_)
            ),
            "B3 an aborted query is not absence"
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            store.get(&original.run_id).await,
            Observed::Present(original)
        );
        assert_eq!(store.get(&replacement.run_id).await, Observed::Absent);
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checkpoint_borrowed_sentinel")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(retained, 0, "B2 only the caller owns rollback");
        pool.close().await;
    }
}
