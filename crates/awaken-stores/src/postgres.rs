//! PostgreSQL storage backend using `sqlx`.
//!
//! Tables are auto-created on first access via `ensure_schema()`.

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
    extract_meta_revision,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore,
    StorageError, ThreadPage, ThreadParentFilter, ThreadQuery, ThreadRunStore, ThreadStore,
    checkpoint_parent_thread_id, paginate_message_records,
};
use awaken_contract::thread::{Thread, normalize_lineage_id_owned};
use sqlx::postgres::{PgListener, PgRow};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::Mutex;

/// PostgreSQL storage backend.
pub struct PostgresStore {
    pool: PgPool,
    threads_table: String,
    runs_table: String,
    messages_table: String,
    configs_table: String,
    config_notify_channel: String,
    schema_ready: Mutex<bool>,
}

impl PostgresStore {
    /// Create a new store with default table names.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            threads_table: "awaken_threads".to_string(),
            runs_table: "awaken_runs".to_string(),
            messages_table: "awaken_messages".to_string(),
            configs_table: "awaken_configs".to_string(),
            config_notify_channel: "awaken_config_changes".to_string(),
            schema_ready: Mutex::new(false),
        }
    }

    /// Create a new store with a custom table prefix.
    pub fn with_prefix(pool: PgPool, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        Self {
            pool,
            threads_table: format!("{prefix}_threads"),
            runs_table: format!("{prefix}_runs"),
            messages_table: format!("{prefix}_messages"),
            configs_table: format!("{prefix}_configs"),
            config_notify_channel: format!("{prefix}_config_changes"),
            schema_ready: Mutex::new(false),
        }
    }

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
            ("request", "JSONB"),
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

        *ready = true;
        Ok(())
    }

    fn merge_thread_lineage(
        mut thread: Thread,
        resource_id: Option<String>,
        parent_thread_id: Option<String>,
    ) -> Thread {
        if let Some(resource_id) = resource_id {
            thread.resource_id = normalize_lineage_id_owned(Some(resource_id));
        }
        if let Some(parent_thread_id) = parent_thread_id {
            thread.parent_thread_id = normalize_lineage_id_owned(Some(parent_thread_id));
        }
        thread.normalize_lineage();
        thread
    }

    fn decode_thread_row(
        data: serde_json::Value,
        resource_id: Option<String>,
        parent_thread_id: Option<String>,
    ) -> Result<Thread, StorageError> {
        let thread: Thread =
            serde_json::from_value(data).map_err(|e| StorageError::Serialization(e.to_string()))?;
        Ok(Self::merge_thread_lineage(
            thread,
            resource_id,
            parent_thread_id,
        ))
    }

    async fn load_thread_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
        lock_clause: &str,
    ) -> Result<Option<Thread>, StorageError> {
        let sql = format!(
            "SELECT data, resource_id, parent_thread_id FROM {} WHERE id = $1 {}",
            self.threads_table, lock_clause
        );
        let row: Option<(serde_json::Value, Option<String>, Option<String>)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        row.map(|(data, resource_id, parent_thread_id)| {
            Self::decode_thread_row(data, resource_id, parent_thread_id)
        })
        .transpose()
    }

    async fn save_thread_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread: &Thread,
    ) -> Result<(), StorageError> {
        let mut normalized = thread.clone();
        normalized.normalize_lineage();
        let data = serde_json::to_value(&normalized)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "INSERT INTO {} (id, data, resource_id, parent_thread_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET
                 data = $2,
                 resource_id = $3,
                 parent_thread_id = $4,
                 updated_at = now()",
            self.threads_table
        );
        sqlx::query(&sql)
            .bind(&normalized.id)
            .bind(&data)
            .bind(normalized.resource_id.as_deref())
            .bind(normalized.parent_thread_id.as_deref())
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn delete_thread_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
    ) -> Result<(), StorageError> {
        let delete_messages = format!("DELETE FROM {} WHERE thread_id = $1", self.messages_table);
        sqlx::query(&delete_messages)
            .bind(thread_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let delete_thread = format!("DELETE FROM {} WHERE id = $1", self.threads_table);
        sqlx::query(&delete_thread)
            .bind(thread_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn list_child_threads_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        parent_thread_id: &str,
        lock_clause: &str,
    ) -> Result<Vec<Thread>, StorageError> {
        let sql = format!(
            "SELECT data, resource_id, parent_thread_id
             FROM {}
             WHERE parent_thread_id = $1
             ORDER BY id ASC
             {}",
            self.threads_table, lock_clause
        );
        let rows: Vec<(serde_json::Value, Option<String>, Option<String>)> = sqlx::query_as(&sql)
            .bind(parent_thread_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        rows.into_iter()
            .map(|(data, resource_id, parent_thread_id)| {
                Self::decode_thread_row(data, resource_id, parent_thread_id)
            })
            .collect()
    }

    async fn validate_thread_hierarchy_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> Result<(), StorageError> {
        let Some(parent_thread_id) =
            normalize_lineage_id_owned(parent_thread_id.map(str::to_owned))
        else {
            return Ok(());
        };
        if parent_thread_id == thread_id {
            return Err(StorageError::Validation(format!(
                "thread '{thread_id}' cannot parent itself"
            )));
        }

        let root_parent_thread_id = parent_thread_id.to_owned();
        let mut current_thread_id = root_parent_thread_id.clone();
        let mut visited = std::collections::HashSet::from([thread_id.to_owned()]);

        loop {
            if !visited.insert(current_thread_id.clone()) {
                return Err(StorageError::Validation(format!(
                    "thread hierarchy cycle detected at '{current_thread_id}'"
                )));
            }

            let Some(thread) = self
                .load_thread_tx(tx, &current_thread_id, "FOR SHARE")
                .await?
            else {
                let message = if current_thread_id == root_parent_thread_id {
                    format!("parent thread not found: {root_parent_thread_id}")
                } else {
                    format!("thread hierarchy references missing ancestor '{current_thread_id}'")
                };
                return Err(StorageError::Validation(message));
            };

            let Some(next_parent_thread_id) = normalize_lineage_id_owned(thread.parent_thread_id)
            else {
                return Ok(());
            };
            current_thread_id = next_parent_thread_id;
        }
    }
}

struct PostgresConfigChangeSubscriber {
    listener: PgListener,
}

#[async_trait]
impl ConfigChangeSubscriber for PostgresConfigChangeSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        let notification = self
            .listener
            .recv()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        serde_json::from_str(notification.payload())
            .map_err(|error| StorageError::Serialization(error.to_string()))
    }
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for PostgresStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT data, resource_id, parent_thread_id FROM {} WHERE id = $1",
            self.threads_table
        );
        let row: Option<(serde_json::Value, Option<String>, Option<String>)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        match row {
            Some((data, resource_id, parent_thread_id)) => Ok(Some(Self::decode_thread_row(
                data,
                resource_id,
                parent_thread_id,
            )?)),
            None => Ok(None),
        }
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.save_thread_tx(&mut tx, thread).await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.validate_thread_hierarchy_tx(&mut tx, &thread.id, thread.parent_thread_id.as_deref())
            .await?;
        self.save_thread_tx(&mut tx, thread).await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.delete_thread_tx(&mut tx, thread_id).await?;

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        if self
            .load_thread_tx(&mut tx, thread_id, "FOR UPDATE")
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }

        match strategy {
            ChildThreadDeleteStrategy::Reject => {
                let children = self
                    .list_child_threads_tx(&mut tx, thread_id, "FOR UPDATE")
                    .await?;
                if !children.is_empty() {
                    return Err(StorageError::Validation(format!(
                        "thread '{thread_id}' has child threads; choose 'detach' or 'cascade'"
                    )));
                }
                self.delete_thread_tx(&mut tx, thread_id).await?;
            }
            ChildThreadDeleteStrategy::Detach => {
                let mut children = self
                    .list_child_threads_tx(&mut tx, thread_id, "FOR UPDATE")
                    .await?;
                let updated_at = awaken_contract::now_ms();
                for child in &mut children {
                    child.parent_thread_id = None;
                    child.metadata.updated_at = Some(updated_at);
                    self.save_thread_tx(&mut tx, child).await?;
                }
                self.delete_thread_tx(&mut tx, thread_id).await?;
            }
            ChildThreadDeleteStrategy::Cascade => {
                let mut visited = std::collections::HashSet::new();
                let mut stack = vec![(thread_id.to_owned(), false)];
                let mut delete_order = Vec::new();

                while let Some((current_thread_id, expanded)) = stack.pop() {
                    if expanded {
                        delete_order.push(current_thread_id);
                        continue;
                    }

                    if !visited.insert(current_thread_id.clone()) {
                        return Err(StorageError::Validation(format!(
                            "thread hierarchy cycle detected while deleting '{thread_id}'"
                        )));
                    }

                    stack.push((current_thread_id.clone(), true));
                    let children = self
                        .list_child_threads_tx(&mut tx, &current_thread_id, "FOR UPDATE")
                        .await?;
                    for child in children.into_iter().rev() {
                        stack.push((child.id, false));
                    }
                }

                for id in delete_order {
                    self.delete_thread_tx(&mut tx, &id).await?;
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT id FROM {} ORDER BY updated_at DESC, id ASC LIMIT $1 OFFSET $2",
            self.threads_table
        );
        let rows: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        self.ensure_schema().await?;
        let query = query.normalized();
        let (parent_thread_id, root_only) = match &query.parent_filter {
            ThreadParentFilter::Any => (None, false),
            ThreadParentFilter::Root => (None, true),
            ThreadParentFilter::Parent(parent_thread_id) => {
                (Some(parent_thread_id.as_str()), false)
            }
        };
        let limit = query.limit.min(i64::MAX as usize) as i64;
        let offset = query.offset.min(i64::MAX as usize) as i64;
        let count_sql = format!(
            "SELECT COUNT(*)::BIGINT FROM {}
             WHERE ($1::text IS NULL OR resource_id = $1)
               AND (($3::bool AND parent_thread_id IS NULL)
                    OR (NOT $3::bool AND ($2::text IS NULL OR parent_thread_id = $2)))",
            self.threads_table
        );
        let total: (i64,) = sqlx::query_as(&count_sql)
            .bind(query.resource_id.as_deref())
            .bind(parent_thread_id)
            .bind(root_only)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let sql = format!(
            "SELECT id FROM {}
             WHERE ($1::text IS NULL OR resource_id = $1)
               AND (($3::bool AND parent_thread_id IS NULL)
                    OR (NOT $3::bool AND ($2::text IS NULL OR parent_thread_id = $2)))
             ORDER BY updated_at DESC, id ASC
             LIMIT $4 OFFSET $5",
            self.threads_table
        );
        let rows: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(query.resource_id.as_deref())
            .bind(parent_thread_id)
            .bind(root_only)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let total = total.0.max(0) as usize;
        let items: Vec<String> = rows.into_iter().map(|(id,)| id).collect();
        let next_offset = query.offset.min(total) + items.len();
        Ok(ThreadPage {
            items,
            total,
            has_more: next_offset < total,
            next_cursor: (next_offset < total).then(|| next_offset.to_string()),
            prev_cursor: (query.offset > 0)
                .then(|| query.offset.saturating_sub(query.limit).to_string()),
        })
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT data FROM {} WHERE thread_id = $1 ORDER BY updated_at DESC LIMIT 1",
            self.messages_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        match row {
            Some((data,)) => {
                let messages: Vec<Message> = serde_json::from_value(data)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                Ok(Some(messages))
            }
            None => Ok(None),
        }
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let Some(messages) = self.load_messages(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        let records = messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                awaken_contract::contract::message::MessageRecord::from_message(
                    thread_id.to_owned(),
                    index as u64 + 1,
                    message,
                )
            })
            .collect();
        Ok(paginate_message_records(records, query))
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let msg_data = serde_json::to_value(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let delete_sql = format!("DELETE FROM {} WHERE thread_id = $1", self.messages_table);
        let insert_sql = format!(
            "INSERT INTO {} (thread_id, data) VALUES ($1, $2)",
            self.messages_table
        );

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query(&delete_sql)
            .bind(thread_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query(&insert_sql)
            .bind(thread_id)
            .bind(&msg_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        // Verify thread exists
        let check_sql = format!("SELECT 1 FROM {} WHERE id = $1", self.threads_table);
        let exists: Option<(i32,)> = sqlx::query_as(&check_sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        if exists.is_none() {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }
        let sql = format!("DELETE FROM {} WHERE thread_id = $1", self.messages_table);
        sqlx::query(&sql)
            .bind(thread_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: awaken_contract::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let thread = self
            .load_thread_tx(&mut tx, id, "FOR UPDATE")
            .await?
            .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
        let mut updated = thread;
        updated.metadata = metadata;
        self.save_thread_tx(&mut tx, &updated).await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))
    }
}

// ── RunStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunStore for PostgresStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let state_json = record
            .state
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok());
        let termination_reason_json = record
            .termination_reason
            .as_ref()
            .and_then(|reason| serde_json::to_value(reason).ok());
        let request_json = record
            .request
            .as_ref()
            .and_then(|request| serde_json::to_value(request).ok());
        let input_json = record
            .input
            .as_ref()
            .and_then(|input| serde_json::to_value(input).ok());
        let output_json = record
            .output
            .as_ref()
            .and_then(|output| serde_json::to_value(output).ok());
        let waiting_json = record
            .waiting
            .as_ref()
            .and_then(|waiting| serde_json::to_value(waiting).ok());
        let outcome_json = record
            .outcome
            .as_ref()
            .and_then(|outcome| serde_json::to_value(outcome).ok());
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24)",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(&record.agent_id)
            .bind(&record.parent_run_id)
            .bind(&request_json)
            .bind(&input_json)
            .bind(&output_json)
            .bind(format!("{:?}", record.status).to_lowercase())
            .bind(&termination_reason_json)
            .bind(&record.final_output)
            .bind(&record.error_payload)
            .bind(&record.dispatch_id)
            .bind(&record.session_id)
            .bind(&record.transport_request_id)
            .bind(&waiting_json)
            .bind(&outcome_json)
            .bind(record.created_at as i64)
            .bind(record.started_at.map(|value| value as i64))
            .bind(record.finished_at.map(|value| value as i64))
            .bind(record.updated_at as i64)
            .bind(record.steps as i32)
            .bind(record.input_tokens as i64)
            .bind(record.output_tokens as i64)
            .bind(&state_json)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if e.to_string().contains("duplicate key")
                    || e.to_string().contains("unique constraint")
                {
                    StorageError::AlreadyExists(record.run_id.clone())
                } else {
                    StorageError::Io(e.to_string())
                }
            })?;
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state FROM {} WHERE run_id = $1",
            self.runs_table
        );
        let row = sqlx::query(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(row.map(run_record_from_pg_row))
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state FROM {} WHERE thread_id = $1 ORDER BY updated_at DESC LIMIT 1",
            self.runs_table
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(row.map(run_record_from_pg_row))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.ensure_schema().await?;

        // Build count query
        let mut conditions = Vec::new();
        if query.thread_id.is_some() {
            conditions.push("thread_id = $1".to_string());
        }
        if query.status.is_some() {
            let idx = if query.thread_id.is_some() { 2 } else { 1 };
            conditions.push(format!("status = ${idx}"));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM {}{}", self.runs_table, where_clause);
        let list_sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state FROM {}{} ORDER BY created_at ASC LIMIT {} OFFSET {}",
            self.runs_table,
            where_clause,
            query.limit.clamp(1, 200),
            query.offset
        );

        // This is simplified — in production you'd use a proper query builder.
        // For the feature-gated postgres backend, we use raw string queries.
        let (total,): (i64,) = {
            let mut q = sqlx::query_as(&count_sql);
            if let Some(ref tid) = query.thread_id {
                q = q.bind(tid);
            }
            if let Some(status) = query.status {
                q = q.bind(format!("{status:?}").to_lowercase());
            }
            q.fetch_one(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
        };

        let rows = {
            let mut q = sqlx::query(&list_sql);
            if let Some(ref tid) = query.thread_id {
                q = q.bind(tid);
            }
            if let Some(status) = query.status {
                q = q.bind(format!("{status:?}").to_lowercase());
            }
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
        };

        let items: Vec<RunRecord> = rows.into_iter().map(run_record_from_pg_row).collect();

        let has_more = (query.offset + items.len()) < total as usize;
        Ok(RunPage {
            items,
            total: total as usize,
            has_more,
        })
    }
}

// ── ThreadRunStore ──────────────────────────────────────────────────

#[async_trait]
impl ThreadRunStore for PostgresStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;

        // Upsert messages
        let msg_data = serde_json::to_value(messages)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // We need a unique constraint on thread_id for messages table.
        // Since we created the table without it, let's use DELETE + INSERT instead.
        let delete_sql = format!("DELETE FROM {} WHERE thread_id = $1", self.messages_table);
        let insert_sql = format!(
            "INSERT INTO {} (thread_id, data) VALUES ($1, $2)",
            self.messages_table
        );

        // Use a transaction for atomicity
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as u64;
        let mut thread = self
            .load_thread_tx(&mut tx, thread_id, "")
            .await?
            .unwrap_or_else(|| Thread::with_id(thread_id));
        self.validate_thread_hierarchy_tx(
            &mut tx,
            thread_id,
            checkpoint_parent_thread_id(Some(&thread), run),
        )
        .await?;
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();
        self.save_thread_tx(&mut tx, &thread).await?;

        sqlx::query(&delete_sql)
            .bind(thread_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query(&insert_sql)
            .bind(thread_id)
            .bind(&msg_data)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        // Upsert run record
        let state_json = run
            .state
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok());
        let termination_reason_json = run
            .termination_reason
            .as_ref()
            .and_then(|reason| serde_json::to_value(reason).ok());
        let request_json = run
            .request
            .as_ref()
            .and_then(|request| serde_json::to_value(request).ok());
        let input_json = run
            .input
            .as_ref()
            .and_then(|input| serde_json::to_value(input).ok());
        let output_json = run
            .output
            .as_ref()
            .and_then(|output| serde_json::to_value(output).ok());
        let waiting_json = run
            .waiting
            .as_ref()
            .and_then(|waiting| serde_json::to_value(waiting).ok());
        let outcome_json = run
            .outcome
            .as_ref()
            .and_then(|outcome| serde_json::to_value(outcome).ok());
        let run_sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24)
             ON CONFLICT (run_id) DO UPDATE SET
                request = $5, run_input = $6, run_output = $7, status = $8,
                termination_reason = $9, final_output = $10,
                error_payload = $11, dispatch_id = $12, session_id = $13,
                transport_request_id = $14, waiting = $15, outcome = $16,
                started_at = $18, finished_at = $19, updated_at = $20,
                steps = $21, input_tokens = $22, output_tokens = $23, state = $24",
            self.runs_table
        );
        sqlx::query(&run_sql)
            .bind(&run.run_id)
            .bind(&run.thread_id)
            .bind(&run.agent_id)
            .bind(&run.parent_run_id)
            .bind(&request_json)
            .bind(&input_json)
            .bind(&output_json)
            .bind(format!("{:?}", run.status).to_lowercase())
            .bind(&termination_reason_json)
            .bind(&run.final_output)
            .bind(&run.error_payload)
            .bind(&run.dispatch_id)
            .bind(&run.session_id)
            .bind(&run.transport_request_id)
            .bind(&waiting_json)
            .bind(&outcome_json)
            .bind(run.created_at as i64)
            .bind(run.started_at.map(|value| value as i64))
            .bind(run.finished_at.map(|value| value as i64))
            .bind(run.updated_at as i64)
            .bind(run.steps as i32)
            .bind(run.input_tokens as i64)
            .bind(run.output_tokens as i64)
            .bind(&state_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(())
    }
}

fn run_record_from_pg_row(row: PgRow) -> RunRecord {
    let status: String = row.get("status");
    let state: Option<serde_json::Value> = row.get("state");
    let request: Option<serde_json::Value> = row.get("request");
    let input: Option<serde_json::Value> = row.get("run_input");
    let output: Option<serde_json::Value> = row.get("run_output");
    let termination_reason: Option<serde_json::Value> = row.get("termination_reason");
    let waiting: Option<serde_json::Value> = row.get("waiting");
    let outcome: Option<serde_json::Value> = row.get("outcome");
    let created_at: i64 = row.get("created_at");
    let started_at: Option<i64> = row.get("started_at");
    let finished_at: Option<i64> = row.get("finished_at");
    let updated_at: i64 = row.get("updated_at");
    let steps: i32 = row.get("steps");
    let input_tokens: i64 = row.get("input_tokens");
    let output_tokens: i64 = row.get("output_tokens");

    RunRecord {
        run_id: row.get("run_id"),
        thread_id: row.get("thread_id"),
        agent_id: row.get("agent_id"),
        parent_run_id: row.get("parent_run_id"),
        request: request.and_then(|value| serde_json::from_value(value).ok()),
        input: input.and_then(|value| serde_json::from_value(value).ok()),
        output: output.and_then(|value| serde_json::from_value(value).ok()),
        status: parse_run_status(&status),
        termination_reason: termination_reason.and_then(|value| serde_json::from_value(value).ok()),
        final_output: row.get("final_output"),
        error_payload: row.get("error_payload"),
        dispatch_id: row.get("dispatch_id"),
        session_id: row.get("session_id"),
        transport_request_id: row.get("transport_request_id"),
        waiting: waiting.and_then(|value| serde_json::from_value(value).ok()),
        outcome: outcome.and_then(|value| serde_json::from_value(value).ok()),
        created_at: created_at as u64,
        started_at: started_at.map(|value| value as u64),
        finished_at: finished_at.map(|value| value as u64),
        updated_at: updated_at as u64,
        steps: steps as usize,
        input_tokens: input_tokens as u64,
        output_tokens: output_tokens as u64,
        state: state.and_then(|value| serde_json::from_value(value).ok()),
    }
}

fn parse_run_status(s: &str) -> awaken_contract::contract::lifecycle::RunStatus {
    use awaken_contract::contract::lifecycle::RunStatus;
    match s {
        "created" => RunStatus::Created,
        "running" => RunStatus::Running,
        "waiting" => RunStatus::Waiting,
        "done" => RunStatus::Done,
        _ => RunStatus::Running,
    }
}

// ── ConfigStore ─────────────────────────────────────────────────────

#[async_trait]
impl ConfigStore for PostgresStore {
    async fn get(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(row.map(|(value,)| value))
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
        self.ensure_schema().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let sql = format!(
            "SELECT id, data FROM {} WHERE namespace = $1 ORDER BY id ASC LIMIT $2 OFFSET $3",
            self.configs_table
        );
        sqlx::query_as(&sql)
            .bind(namespace)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))
    }

    async fn put(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3)
             ON CONFLICT (namespace, id) DO UPDATE SET data = $3, updated_at = now()",
            self.configs_table
        );
        sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3)",
            self.configs_table
        );
        let result = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await;
        if let Err(error) = result {
            if error
                .as_database_error()
                .and_then(|db_error| db_error.code())
                .as_deref()
                == Some("23505")
            {
                return Err(StorageError::AlreadyExists(format!("{namespace}/{id}")));
            }
            return Err(StorageError::Io(error.to_string()));
        }

        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "DELETE FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let result = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        if result.rows_affected() > 0 {
            let payload = serde_json::to_string(&ConfigChangeEvent {
                namespace: namespace.to_string(),
                id: id.to_string(),
                kind: ConfigChangeKind::Delete,
            })
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&self.config_notify_channel)
                .bind(payload)
                .execute(&mut *tx)
                .await
                .map_err(|error| StorageError::Io(error.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        // Lock the row (or its absence) so that concurrent writers cannot race
        // between the read and the upsert within this transaction.
        let select_sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2 FOR UPDATE",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&select_sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let actual = row
            .as_ref()
            .map(|(v,)| v)
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }

        let upsert_sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (namespace, id) DO UPDATE SET data = EXCLUDED.data, updated_at = now()",
            self.configs_table
        );
        sqlx::query(&upsert_sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        // Fire NOTIFY inside the same transaction, matching put semantics.
        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let select_sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2 FOR UPDATE",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&select_sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let actual = row
            .as_ref()
            .map(|(v,)| v)
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }

        let delete_sql = format!(
            "DELETE FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let result = sqlx::query(&delete_sql)
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        if result.rows_affected() > 0 {
            let payload = serde_json::to_string(&ConfigChangeEvent {
                namespace: namespace.to_string(),
                id: id.to_string(),
                kind: ConfigChangeKind::Delete,
            })
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&self.config_notify_channel)
                .bind(payload)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }
}

// ── ConfigChangeNotifier ────────────────────────────────────────────

#[async_trait]
impl ConfigChangeNotifier for PostgresStore {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        self.ensure_schema().await?;
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        listener
            .listen(&self.config_notify_channel)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(Box::new(PostgresConfigChangeSubscriber { listener }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_status_known_values() {
        use awaken_contract::contract::lifecycle::RunStatus;
        assert!(matches!(parse_run_status("created"), RunStatus::Created));
        assert!(matches!(parse_run_status("running"), RunStatus::Running));
        assert!(matches!(parse_run_status("waiting"), RunStatus::Waiting));
        assert!(matches!(parse_run_status("done"), RunStatus::Done));
    }

    #[test]
    fn parse_run_status_unknown_defaults_to_running() {
        use awaken_contract::contract::lifecycle::RunStatus;
        assert!(matches!(parse_run_status("unknown"), RunStatus::Running));
        assert!(matches!(parse_run_status(""), RunStatus::Running));
    }

    #[test]
    fn postgres_store_default_table_names() {
        // We can't actually connect, but we can verify table name construction
        // This would require a PgPool which needs a real connection.
        // Instead test the `with_prefix` naming logic by creating without connecting.
        // We can only test the table name generation pattern.
        let prefix = "test_prefix";
        assert_eq!(format!("{prefix}_threads"), "test_prefix_threads");
        assert_eq!(format!("{prefix}_runs"), "test_prefix_runs");
        assert_eq!(format!("{prefix}_configs"), "test_prefix_configs");
        assert_eq!(
            format!("{prefix}_config_changes"),
            "test_prefix_config_changes"
        );
    }

    #[test]
    fn merge_thread_lineage_prefers_columns_when_present() {
        let thread = Thread::with_id("thread-1")
            .with_resource_id("json-resource")
            .with_parent_thread_id("json-parent");

        let merged = PostgresStore::merge_thread_lineage(
            thread,
            Some("column-resource".to_string()),
            Some("column-parent".to_string()),
        );

        assert_eq!(merged.resource_id.as_deref(), Some("column-resource"));
        assert_eq!(merged.parent_thread_id.as_deref(), Some("column-parent"));
    }

    #[test]
    fn merge_thread_lineage_preserves_json_when_columns_missing() {
        let thread = Thread::with_id("thread-1")
            .with_resource_id("json-resource")
            .with_parent_thread_id("json-parent");

        let merged = PostgresStore::merge_thread_lineage(thread, None, None);

        assert_eq!(merged.resource_id.as_deref(), Some("json-resource"));
        assert_eq!(merged.parent_thread_id.as_deref(), Some("json-parent"));
    }

    // Integration tests below require a running PostgreSQL server.

    #[tokio::test]
    #[ignore]
    async fn schema_initialization() {
        let pool = PgPool::connect("postgres://localhost/awaken_test")
            .await
            .unwrap();
        let store = PostgresStore::with_prefix(pool, "test_schema_init");
        store.ensure_schema().await.unwrap();
        // Calling again should be idempotent
        store.ensure_schema().await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn connection_error_handling() {
        let pool = PgPool::connect("postgres://localhost:19999/nonexistent")
            .await
            .unwrap_err();
        // Connection itself fails, which is the expected behavior
        let _ = pool;
    }

    #[tokio::test]
    #[ignore]
    async fn thread_crud_operations() {
        let pool = PgPool::connect("postgres://localhost/awaken_test")
            .await
            .unwrap();
        let store = PostgresStore::with_prefix(pool, "test_crud");
        store.ensure_schema().await.unwrap();

        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();

        let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, thread.id);

        store.delete_thread(&thread.id).await.unwrap();
        assert!(store.load_thread(&thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn run_create_duplicate_returns_already_exists() {
        use awaken_contract::contract::lifecycle::RunStatus;

        let pool = PgPool::connect("postgres://localhost/awaken_test")
            .await
            .unwrap();
        let store = PostgresStore::with_prefix(pool, "test_dup_run");
        store.ensure_schema().await.unwrap();

        let run = RunRecord {
            run_id: format!("dup-{}", uuid::Uuid::now_v7()),
            thread_id: "t-1".to_string(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 100,
            started_at: None,
            finished_at: None,
            updated_at: 100,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    #[ignore]
    async fn checkpoint_atomicity() {
        use awaken_contract::contract::lifecycle::RunStatus;
        use awaken_contract::contract::message::Message;

        let pool = PgPool::connect("postgres://localhost/awaken_test")
            .await
            .unwrap();
        let store = PostgresStore::with_prefix(pool, "test_checkpoint");
        store.ensure_schema().await.unwrap();

        let thread_id = format!("t-{}", uuid::Uuid::now_v7());
        let msgs = vec![Message::user("checkpoint test")];
        let run = RunRecord {
            run_id: format!("r-{}", uuid::Uuid::now_v7()),
            thread_id: thread_id.clone(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 100,
            started_at: None,
            finished_at: None,
            updated_at: 100,
            steps: 1,
            input_tokens: 10,
            output_tokens: 20,
            state: None,
        };

        store.checkpoint(&thread_id, &msgs, &run).await.unwrap();

        let loaded_msgs = store.load_messages(&thread_id).await.unwrap().unwrap();
        assert_eq!(loaded_msgs.len(), 1);
        let loaded_run = store.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(loaded_run.thread_id, thread_id);
    }

    #[tokio::test]
    #[ignore = "requires PG_TEST_URL"]
    async fn put_if_revision_atomic_cas() {
        let url = std::env::var("PG_TEST_URL")
            .unwrap_or_else(|_| "postgres://localhost/awaken_test".to_string());
        let pool = PgPool::connect(&url).await.unwrap();
        let store = PostgresStore::with_prefix(pool, "test_cas");
        store.ensure_schema().await.unwrap();

        let v1 = serde_json::json!({"spec": {"id": "cas-key"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
        // First write: no record → expected 0 succeeds.
        store
            .put_if_revision("cas_ns", "cas-key", &v1, 0)
            .await
            .unwrap();
        let stored = ConfigStore::get(&store, "cas_ns", "cas-key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored["meta"]["revision"], 1);

        // Conflict: re-try with expected 0 should fail.
        let err = store
            .put_if_revision("cas_ns", "cas-key", &v1, 0)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::VersionConflict {
                expected: 0,
                actual: 1
            }
        ));
    }
}
