use awaken_contract::contract::storage::StorageError;

use super::PostgresStore;

impl PostgresStore {
    /// Ensure all tables exist. Called lazily on first access.
    pub async fn ensure_schema(&self) -> Result<(), StorageError> {
        let mut ready = self.schema_ready.lock().await;
        if *ready {
            return Ok(());
        }

        let statements = vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    id TEXT PRIMARY KEY,
                    data JSONB NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
                )",
                self.threads_table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    thread_id TEXT NOT NULL,
                    data JSONB NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
                )",
                self.messages_table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    run_id TEXT PRIMARY KEY,
                    thread_id TEXT NOT NULL,
                    agent_id TEXT NOT NULL DEFAULT '',
                    parent_run_id TEXT,
                    registry_manifest JSONB,
                    activation JSONB,
                    request JSONB,
                    run_input JSONB,
                    run_output JSONB,
                    status TEXT NOT NULL,
                    termination_reason JSONB,
                    final_output TEXT,
                    error_payload JSONB,
                    dispatch_id TEXT,
                    session_id TEXT,
                    transport_request_id TEXT,
                    waiting JSONB,
                    outcome JSONB,
                    created_at BIGINT NOT NULL,
                    started_at BIGINT,
                    finished_at BIGINT,
                    updated_at BIGINT NOT NULL,
                    steps INTEGER NOT NULL DEFAULT 0,
                    input_tokens BIGINT NOT NULL DEFAULT 0,
                    output_tokens BIGINT NOT NULL DEFAULT 0,
                    state JSONB
                )",
                self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_id ON {} (thread_id)",
                self.runs_table, self.runs_table
            ),
            // Additional performance indices
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_created ON {} (thread_id, created_at DESC)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_id ON {} (thread_id)",
                self.messages_table, self.messages_table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    namespace TEXT NOT NULL,
                    id TEXT NOT NULL,
                    data JSONB NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                    PRIMARY KEY (namespace, id)
                )",
                self.configs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_namespace_id ON {} (namespace, id)",
                self.configs_table, self.configs_table
            ),
        ];

        for stmt in statements {
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        let run_migrations = [
            ("activation", "JSONB"),
            ("request", "JSONB"),
            ("registry_manifest", "JSONB"),
            ("run_input", "JSONB"),
            ("run_output", "JSONB"),
            ("termination_reason", "JSONB"),
            ("final_output", "TEXT"),
            ("error_payload", "JSONB"),
            ("dispatch_id", "TEXT"),
            ("session_id", "TEXT"),
            ("transport_request_id", "TEXT"),
            ("waiting", "JSONB"),
            ("outcome", "JSONB"),
            ("started_at", "BIGINT"),
            ("finished_at", "BIGINT"),
        ];
        for (column, ty) in run_migrations {
            let stmt = format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}",
                self.runs_table, column, ty
            );
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        let thread_migrations = [("resource_id", "TEXT"), ("parent_thread_id", "TEXT")];
        for (column, ty) in thread_migrations {
            let stmt = format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}",
                self.threads_table, column, ty
            );
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        let thread_backfills = [
            format!(
                "UPDATE {}
                 SET resource_id = NULLIF(BTRIM(COALESCE(resource_id, data ->> 'resource_id')), '')",
                self.threads_table
            ),
            format!(
                "UPDATE {}
                 SET parent_thread_id = NULLIF(BTRIM(COALESCE(parent_thread_id, data ->> 'parent_thread_id')), '')",
                self.threads_table
            ),
        ];
        for stmt in thread_backfills {
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        let thread_indexes = [
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_resource_id ON {} (resource_id)",
                self.threads_table, self.threads_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_parent_thread_id ON {} (parent_thread_id)",
                self.threads_table, self.threads_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_resource_parent_updated
                 ON {} (resource_id, parent_thread_id, updated_at DESC, id ASC)",
                self.threads_table, self.threads_table
            ),
        ];
        for stmt in thread_indexes {
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        crate::postgres_event::ensure_event_schema(self).await?;
        crate::postgres_protocol_replay::ensure_protocol_replay_schema(self).await?;
        crate::postgres_outbox::ensure_outbox_schema(self).await?;
        crate::postgres_stream_checkpoint::ensure_stream_checkpoint_schema(self).await?;
        crate::postgres_versioned_registry_schema::ensure_versioned_registry_schema(self).await?;
        *ready = true;
        Ok(())
    }
}
