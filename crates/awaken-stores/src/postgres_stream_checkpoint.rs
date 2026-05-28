//! PostgreSQL implementation of `StreamCheckpointStore`.

use async_trait::async_trait;
use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::contract::stream_checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use sqlx::Row;

use crate::postgres::PostgresStore;

const DEFAULT_SCOPE_ID: &str = "default";

struct StreamCheckpointTables {
    checkpoints: String,
}

impl StreamCheckpointTables {
    fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            checkpoints: format!("{prefix}_stream_checkpoints"),
        }
    }
}

pub(crate) async fn ensure_stream_checkpoint_schema(
    store: &PostgresStore,
) -> Result<(), StorageError> {
    let tables = StreamCheckpointTables::from_store(store);
    let statements = vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_id TEXT NOT NULL DEFAULT 'default',
                run_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                upstream_model TEXT NOT NULL,
                checkpoint_json JSONB NOT NULL,
                updated_at_ms BIGINT NOT NULL,
                PRIMARY KEY (scope_id, run_id)
            )",
            tables.checkpoints
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_thread_updated
             ON {} (scope_id, thread_id, updated_at_ms DESC)",
            tables.checkpoints, tables.checkpoints
        ),
    ];

    for stmt in statements {
        sqlx::query(&stmt)
            .execute(&store.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
    }
    Ok(())
}

#[async_trait]
impl StreamCheckpointStore for PostgresStore {
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        self.ensure_schema().await.map_err(to_checkpoint_error)?;
        let tables = StreamCheckpointTables::from_store(self);
        let checkpoint_json = serde_json::to_value(&checkpoint).map_err(to_checkpoint_error)?;
        sqlx::query(&format!(
            "INSERT INTO {} (
                scope_id, run_id, thread_id, upstream_model, checkpoint_json, updated_at_ms
             )
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (scope_id, run_id)
             DO UPDATE SET
                thread_id = EXCLUDED.thread_id,
                upstream_model = EXCLUDED.upstream_model,
                checkpoint_json = EXCLUDED.checkpoint_json,
                updated_at_ms = EXCLUDED.updated_at_ms",
            tables.checkpoints
        ))
        .bind(DEFAULT_SCOPE_ID)
        .bind(&checkpoint.run_id)
        .bind(&checkpoint.thread_id)
        .bind(&checkpoint.upstream_model)
        .bind(checkpoint_json)
        .bind(i64::try_from(checkpoint.updated_at_ms).map_err(to_checkpoint_error)?)
        .execute(&self.pool)
        .await
        .map_err(to_checkpoint_error)?;
        Ok(())
    }

    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, StreamCheckpointError> {
        self.ensure_schema().await.map_err(to_checkpoint_error)?;
        let tables = StreamCheckpointTables::from_store(self);
        let row = sqlx::query(&format!(
            "SELECT checkpoint_json
             FROM {}
             WHERE scope_id = $1 AND run_id = $2",
            tables.checkpoints
        ))
        .bind(DEFAULT_SCOPE_ID)
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(to_checkpoint_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let value = row
            .try_get::<serde_json::Value, _>("checkpoint_json")
            .map_err(to_checkpoint_error)?;
        serde_json::from_value(value)
            .map(Some)
            .map_err(to_checkpoint_error)
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        self.ensure_schema().await.map_err(to_checkpoint_error)?;
        let tables = StreamCheckpointTables::from_store(self);
        sqlx::query(&format!(
            "DELETE FROM {}
             WHERE scope_id = $1 AND run_id = $2",
            tables.checkpoints
        ))
        .bind(DEFAULT_SCOPE_ID)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(to_checkpoint_error)?;
        Ok(())
    }
}

fn to_checkpoint_error(error: impl std::fmt::Display) -> StreamCheckpointError {
    StreamCheckpointError(error.to_string())
}

#[cfg(test)]
mod tests {
    use awaken_server_contract::contract::stream_checkpoint::StreamCheckpointStore;

    use crate::postgres::PostgresStore;

    use super::StreamCheckpointTables;

    #[tokio::test]
    async fn derives_checkpoint_table_from_thread_prefix() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/awaken_test").unwrap();
        let store = PostgresStore::with_prefix(pool, "demo");

        let tables = StreamCheckpointTables::from_store(&store);
        assert_eq!(tables.checkpoints, "demo_stream_checkpoints");
    }

    #[test]
    fn postgres_store_implements_stream_checkpoint_store() {
        fn assert_store<T: StreamCheckpointStore>() {}
        assert_store::<PostgresStore>();
    }
}
