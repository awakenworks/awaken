//! PostgreSQL storage backend using `sqlx`.
//!
//! Tables are auto-created on first access via `ensure_schema()`.

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MailboxEntry, MailboxStore, RunPage, RunQuery, RunRecord, RunStore, StorageError,
    ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use sqlx::PgPool;
use tokio::sync::Mutex;

/// PostgreSQL storage backend.
pub struct PostgresStore {
    pool: PgPool,
    threads_table: String,
    runs_table: String,
    messages_table: String,
    mailbox_table: String,
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
            mailbox_table: "awaken_mailbox".to_string(),
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
            mailbox_table: format!("{prefix}_mailbox"),
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
                    status TEXT NOT NULL,
                    termination_code TEXT,
                    created_at BIGINT NOT NULL,
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
            format!(
                "CREATE TABLE IF NOT EXISTS {} (
                    entry_id TEXT PRIMARY KEY,
                    mailbox_id TEXT NOT NULL,
                    payload JSONB NOT NULL,
                    created_at BIGINT NOT NULL
                )",
                self.mailbox_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_mailbox_id ON {} (mailbox_id, created_at)",
                self.mailbox_table, self.mailbox_table
            ),
        ];

        for stmt in statements {
            sqlx::query(&stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        *ready = true;
        Ok(())
    }
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for PostgresStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!("SELECT data FROM {} WHERE id = $1", self.threads_table);
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        match row {
            Some((data,)) => {
                let thread: Thread = serde_json::from_value(data)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                Ok(Some(thread))
            }
            None => Ok(None),
        }
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let data =
            serde_json::to_value(thread).map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "INSERT INTO {} (id, data) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET data = $2, updated_at = now()",
            self.threads_table
        );
        sqlx::query(&sql)
            .bind(&thread.id)
            .bind(&data)
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT id FROM {} ORDER BY id LIMIT $1 OFFSET $2",
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
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, status, termination_code, created_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(&record.agent_id)
            .bind(&record.parent_run_id)
            .bind(format!("{:?}", record.status).to_lowercase())
            .bind(&record.termination_code)
            .bind(record.created_at as i64)
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
            "SELECT run_id, thread_id, agent_id, parent_run_id, status, termination_code, created_at, updated_at, steps, input_tokens, output_tokens, state FROM {} WHERE run_id = $1",
            self.runs_table
        );
        let row: Option<(
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            i64,
            i64,
            i32,
            i64,
            i64,
            Option<serde_json::Value>,
        )> = sqlx::query_as(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        match row {
            Some((
                run_id,
                thread_id,
                agent_id,
                parent_run_id,
                status,
                termination_code,
                created_at,
                updated_at,
                steps,
                input_tokens,
                output_tokens,
                state,
            )) => {
                let status = parse_run_status(&status);
                let state = state.and_then(|v| serde_json::from_value(v).ok());
                Ok(Some(RunRecord {
                    run_id,
                    thread_id,
                    agent_id,
                    parent_run_id,
                    status,
                    termination_code,
                    created_at: created_at as u64,
                    updated_at: updated_at as u64,
                    steps: steps as usize,
                    input_tokens: input_tokens as u64,
                    output_tokens: output_tokens as u64,
                    state,
                }))
            }
            None => Ok(None),
        }
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, status, termination_code, created_at, updated_at, steps, input_tokens, output_tokens, state FROM {} WHERE thread_id = $1 ORDER BY updated_at DESC LIMIT 1",
            self.runs_table
        );
        let row: Option<(
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            i64,
            i64,
            i32,
            i64,
            i64,
            Option<serde_json::Value>,
        )> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        match row {
            Some((
                run_id,
                thread_id,
                agent_id,
                parent_run_id,
                status,
                termination_code,
                created_at,
                updated_at,
                steps,
                input_tokens,
                output_tokens,
                state,
            )) => {
                let status = parse_run_status(&status);
                let state = state.and_then(|v| serde_json::from_value(v).ok());
                Ok(Some(RunRecord {
                    run_id,
                    thread_id,
                    agent_id,
                    parent_run_id,
                    status,
                    termination_code,
                    created_at: created_at as u64,
                    updated_at: updated_at as u64,
                    steps: steps as usize,
                    input_tokens: input_tokens as u64,
                    output_tokens: output_tokens as u64,
                    state,
                }))
            }
            None => Ok(None),
        }
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
            "SELECT run_id, thread_id, agent_id, parent_run_id, status, termination_code, created_at, updated_at, steps, input_tokens, output_tokens, state FROM {}{} ORDER BY created_at ASC LIMIT {} OFFSET {}",
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

        let rows: Vec<(
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            i64,
            i64,
            i32,
            i64,
            i64,
            Option<serde_json::Value>,
        )> = {
            let mut q = sqlx::query_as(&list_sql);
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

        let items: Vec<RunRecord> = rows
            .into_iter()
            .map(
                |(
                    run_id,
                    thread_id,
                    agent_id,
                    parent_run_id,
                    status,
                    termination_code,
                    created_at,
                    updated_at,
                    steps,
                    input_tokens,
                    output_tokens,
                    state,
                )| {
                    let status = parse_run_status(&status);
                    let state = state.and_then(|v| serde_json::from_value(v).ok());
                    RunRecord {
                        run_id,
                        thread_id,
                        agent_id,
                        parent_run_id,
                        status,
                        termination_code,
                        created_at: created_at as u64,
                        updated_at: updated_at as u64,
                        steps: steps as usize,
                        input_tokens: input_tokens as u64,
                        output_tokens: output_tokens as u64,
                        state,
                    }
                },
            )
            .collect();

        let has_more = (query.offset + items.len()) < total as usize;
        Ok(RunPage {
            items,
            total: total as usize,
            has_more,
        })
    }
}

// ── MailboxStore ────────────────────────────────────────────────────

#[async_trait]
impl MailboxStore for PostgresStore {
    async fn push_message(&self, entry: &MailboxEntry) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "INSERT INTO {} (entry_id, mailbox_id, payload, created_at) VALUES ($1, $2, $3, $4)",
            self.mailbox_table
        );
        sqlx::query(&sql)
            .bind(&entry.entry_id)
            .bind(&entry.mailbox_id)
            .bind(&entry.payload)
            .bind(entry.created_at as i64)
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn pop_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "DELETE FROM {} WHERE entry_id IN (
                SELECT entry_id FROM {} WHERE mailbox_id = $1 ORDER BY created_at ASC LIMIT $2
            ) RETURNING entry_id, mailbox_id, payload, created_at",
            self.mailbox_table, self.mailbox_table
        );
        let rows: Vec<(String, String, serde_json::Value, i64)> = sqlx::query_as(&sql)
            .bind(mailbox_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(entry_id, mailbox_id, payload, created_at)| MailboxEntry {
                entry_id,
                mailbox_id,
                payload,
                created_at: created_at as u64,
            })
            .collect())
    }

    async fn peek_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT entry_id, mailbox_id, payload, created_at FROM {} WHERE mailbox_id = $1 ORDER BY created_at ASC LIMIT $2",
            self.mailbox_table
        );
        let rows: Vec<(String, String, serde_json::Value, i64)> = sqlx::query_as(&sql)
            .bind(mailbox_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(entry_id, mailbox_id, payload, created_at)| MailboxEntry {
                entry_id,
                mailbox_id,
                payload,
                created_at: created_at as u64,
            })
            .collect())
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
        let run_sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, status, termination_code, created_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (run_id) DO UPDATE SET
                status = $5, termination_code = $6, updated_at = $8,
                steps = $9, input_tokens = $10, output_tokens = $11, state = $12",
            self.runs_table
        );
        sqlx::query(&run_sql)
            .bind(&run.run_id)
            .bind(&run.thread_id)
            .bind(&run.agent_id)
            .bind(&run.parent_run_id)
            .bind(format!("{:?}", run.status).to_lowercase())
            .bind(&run.termination_code)
            .bind(run.created_at as i64)
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

fn parse_run_status(s: &str) -> awaken_contract::contract::lifecycle::RunStatus {
    use awaken_contract::contract::lifecycle::RunStatus;
    match s {
        "running" => RunStatus::Running,
        "waiting" => RunStatus::Waiting,
        "done" => RunStatus::Done,
        _ => RunStatus::Running,
    }
}
