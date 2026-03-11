use async_trait::async_trait;
#[cfg(feature = "postgres")]
use sqlx::{Postgres, QueryBuilder};
use tirea_contract::storage::{
    paginate_mailbox_entries, Committed, MailboxEntry, MailboxPage, MailboxQuery, MailboxReader,
    MailboxStoreError, MailboxThreadInterrupt, MailboxThreadState, MailboxWriter, MessagePage,
    MessageQuery, MessageWithCursor, RunOrigin, RunPage, RunQuery, RunReader, RunRecord, RunStatus,
    RunStoreError, RunWriter, SortOrder, ThreadHead, ThreadListPage, ThreadListQuery, ThreadReader,
    ThreadStoreError, ThreadWriter, VersionPrecondition,
};
use tirea_contract::{Message, Thread, ThreadChangeSet, Visibility};

pub struct PostgresStore {
    pool: sqlx::PgPool,
    table: String,
    messages_table: String,
    runs_table: String,
    mailbox_table: String,
    mailbox_threads_table: String,
    schema_ready: tokio::sync::Mutex<bool>,
}

#[cfg(feature = "postgres")]
impl PostgresStore {
    /// Create a new PostgreSQL storage using the given connection pool.
    ///
    /// Sessions are stored in the `agent_sessions` table by default,
    /// messages in `agent_messages`.
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            table: "agent_sessions".to_string(),
            messages_table: "agent_messages".to_string(),
            runs_table: "agent_runs".to_string(),
            mailbox_table: "agent_mailbox".to_string(),
            mailbox_threads_table: "agent_mailbox_threads".to_string(),
            schema_ready: tokio::sync::Mutex::new(false),
        }
    }

    /// Create a new PostgreSQL storage with a custom table name.
    ///
    /// The messages table will be named `{table}_messages`.
    pub fn with_table(pool: sqlx::PgPool, table: impl Into<String>) -> Self {
        let table = table.into();
        let messages_table = format!("{}_messages", table);
        let runs_table = format!("{}_runs", table);
        let mailbox_table = format!("{}_mailbox", table);
        let mailbox_threads_table = format!("{}_mailbox_threads", table);
        Self {
            pool,
            table,
            messages_table,
            runs_table,
            mailbox_table,
            mailbox_threads_table,
            schema_ready: tokio::sync::Mutex::new(false),
        }
    }

    fn schema_statements(&self) -> Vec<String> {
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {} (id TEXT PRIMARY KEY, data JSONB NOT NULL, updated_at TIMESTAMPTZ NOT NULL DEFAULT now())",
                self.table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (seq BIGSERIAL PRIMARY KEY, session_id TEXT NOT NULL REFERENCES {}(id) ON DELETE CASCADE, message_id TEXT, run_id TEXT, step_index INTEGER, data JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
                self.messages_table, self.table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_session_seq ON {} (session_id, seq)",
                self.messages_table, self.messages_table
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_message_id ON {} (message_id) WHERE message_id IS NOT NULL",
                self.messages_table, self.messages_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_session_run ON {} (session_id, run_id) WHERE run_id IS NOT NULL",
                self.messages_table, self.messages_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_resource_id ON {} ((data->>'resource_id')) WHERE data ? 'resource_id'",
                self.table, self.table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_parent_thread_id ON {} ((data->>'parent_thread_id')) WHERE data ? 'parent_thread_id'",
                self.table, self.table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, agent_id TEXT NOT NULL DEFAULT '', parent_run_id TEXT, parent_thread_id TEXT, origin TEXT NOT NULL, status TEXT NOT NULL, termination_code TEXT, termination_detail TEXT, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, source_mailbox_entry_id TEXT, metadata JSONB)",
                self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_id ON {} (thread_id)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_active ON {} (thread_id, created_at DESC) WHERE status != 'done'",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_parent_run_id ON {} (parent_run_id) WHERE parent_run_id IS NOT NULL",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_status ON {} (status)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_termination_code ON {} (termination_code) WHERE termination_code IS NOT NULL",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_origin ON {} (origin)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_created_at ON {} (created_at, run_id)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (entry_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, run_id TEXT NOT NULL UNIQUE, agent_id TEXT NOT NULL, generation BIGINT NOT NULL DEFAULT 0, status TEXT NOT NULL, request JSONB NOT NULL, dedupe_key TEXT, kind TEXT, sender_id TEXT, priority SMALLINT NOT NULL DEFAULT 0, available_at BIGINT NOT NULL, attempt_count INTEGER NOT NULL DEFAULT 0, last_error TEXT, claim_token TEXT, claimed_by TEXT, lease_until BIGINT, accepted_run_id TEXT, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL)",
                self.mailbox_table
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {} (thread_id TEXT PRIMARY KEY, current_generation BIGINT NOT NULL DEFAULT 0, updated_at BIGINT NOT NULL)",
                self.mailbox_threads_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_status_available ON {} (status, available_at, created_at)",
                self.mailbox_table, self.mailbox_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_status ON {} (thread_id, status, created_at)",
                self.mailbox_table, self.mailbox_table
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_thread_dedupe ON {} (thread_id, dedupe_key) WHERE dedupe_key IS NOT NULL",
                self.mailbox_table, self.mailbox_table
            ),
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS generation BIGINT NOT NULL DEFAULT 0",
                self.mailbox_table
            ),
            // Migration: add envelope metadata columns to mailbox entries.
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS kind TEXT",
                self.mailbox_table
            ),
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS sender_id TEXT",
                self.mailbox_table
            ),
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS priority SMALLINT NOT NULL DEFAULT 0",
                self.mailbox_table
            ),
            // Migration: add agent_id column to existing tables.
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS agent_id TEXT NOT NULL DEFAULT ''",
                self.runs_table
            ),
            // Enforce at most one non-terminal run per thread.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_{}_thread_active_unique ON {} (thread_id) WHERE status != 'done'",
                self.runs_table, self.runs_table
            ),
        ]
    }

    async fn ensure_schema_ready(&self) -> Result<(), sqlx::Error> {
        let mut schema_ready = self.schema_ready.lock().await;
        if *schema_ready {
            return Ok(());
        }

        // Only flip the flag after all statements succeed so transient failures can retry.
        for sql in self.schema_statements() {
            sqlx::query(&sql).execute(&self.pool).await?;
        }

        *schema_ready = true;
        Ok(())
    }

    async fn ensure_thread_schema_ready(&self) -> Result<(), ThreadStoreError> {
        self.ensure_schema_ready().await.map_err(Self::sql_err)
    }

    async fn ensure_run_schema_ready(&self) -> Result<(), RunStoreError> {
        self.ensure_schema_ready().await.map_err(Self::run_sql_err)
    }

    /// Ensure the storage tables exist (idempotent).
    ///
    /// This is optional for callers; the store also initializes its schema
    /// automatically on first access.
    pub async fn ensure_table(&self) -> Result<(), ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;
        Ok(())
    }

    fn sql_err(e: sqlx::Error) -> ThreadStoreError {
        ThreadStoreError::Io(std::io::Error::other(e.to_string()))
    }

    fn run_sql_err(e: sqlx::Error) -> RunStoreError {
        RunStoreError::Backend(e.to_string())
    }

    fn mailbox_sql_err(e: sqlx::Error) -> MailboxStoreError {
        MailboxStoreError::Backend(e.to_string())
    }

    fn encode_origin(origin: RunOrigin) -> &'static str {
        match origin {
            RunOrigin::User => "user",
            RunOrigin::Subagent => "subagent",
            RunOrigin::AgUi => "ag_ui",
            RunOrigin::AiSdk => "ai_sdk",
            RunOrigin::A2a => "a2a",
            RunOrigin::Internal => "internal",
        }
    }

    fn decode_origin(raw: &str) -> Result<RunOrigin, RunStoreError> {
        match raw {
            "user" => Ok(RunOrigin::User),
            "subagent" => Ok(RunOrigin::Subagent),
            "ag_ui" => Ok(RunOrigin::AgUi),
            "ai_sdk" => Ok(RunOrigin::AiSdk),
            "a2a" => Ok(RunOrigin::A2a),
            "internal" => Ok(RunOrigin::Internal),
            _ => Err(RunStoreError::Backend(format!(
                "invalid run origin value: {raw}"
            ))),
        }
    }

    fn encode_status(status: RunStatus) -> &'static str {
        match status {
            RunStatus::Running => "running",
            RunStatus::Waiting => "waiting",
            RunStatus::Done => "done",
        }
    }

    fn decode_status(raw: &str) -> Result<RunStatus, RunStoreError> {
        match raw {
            "running" => Ok(RunStatus::Running),
            "waiting" => Ok(RunStatus::Waiting),
            "done" => Ok(RunStatus::Done),
            // Backward compatibility for legacy persisted values.
            "submitted" | "working" => Ok(RunStatus::Running),
            "input_required" | "auth_required" => Ok(RunStatus::Waiting),
            "completed" | "failed" | "canceled" | "cancelled" | "rejected" => Ok(RunStatus::Done),
            _ => Err(RunStoreError::Backend(format!(
                "invalid run status value: {raw}"
            ))),
        }
    }

    fn encode_mailbox_status(status: tirea_contract::MailboxEntryStatus) -> &'static str {
        match status {
            tirea_contract::MailboxEntryStatus::Queued => "queued",
            tirea_contract::MailboxEntryStatus::Claimed => "claimed",
            tirea_contract::MailboxEntryStatus::Accepted => "accepted",
            tirea_contract::MailboxEntryStatus::Superseded => "superseded",
            tirea_contract::MailboxEntryStatus::Cancelled => "cancelled",
            tirea_contract::MailboxEntryStatus::DeadLetter => "dead_letter",
        }
    }

    fn decode_mailbox_status(
        raw: &str,
    ) -> Result<tirea_contract::MailboxEntryStatus, MailboxStoreError> {
        match raw {
            "queued" => Ok(tirea_contract::MailboxEntryStatus::Queued),
            "claimed" => Ok(tirea_contract::MailboxEntryStatus::Claimed),
            "accepted" => Ok(tirea_contract::MailboxEntryStatus::Accepted),
            "superseded" => Ok(tirea_contract::MailboxEntryStatus::Superseded),
            "cancelled" | "canceled" => Ok(tirea_contract::MailboxEntryStatus::Cancelled),
            "dead_letter" => Ok(tirea_contract::MailboxEntryStatus::DeadLetter),
            _ => Err(MailboxStoreError::Backend(format!(
                "invalid mailbox status value: {raw}"
            ))),
        }
    }

    fn to_db_timestamp(value: u64, field: &str) -> Result<i64, RunStoreError> {
        i64::try_from(value).map_err(|_| {
            RunStoreError::Backend(format!("{field} is too large for postgres BIGINT: {value}"))
        })
    }

    fn from_db_timestamp(value: i64, field: &str) -> Result<u64, RunStoreError> {
        u64::try_from(value).map_err(|_| {
            RunStoreError::Backend(format!(
                "{field} cannot be negative in postgres BIGINT: {value}"
            ))
        })
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl MailboxReader for PostgresStore {
    async fn load_mailbox_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let sql = format!(
            "SELECT entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at FROM {} WHERE entry_id = $1",
            self.mailbox_table
        );
        let row = sqlx::query_as::<_, MailboxRow>(&sql)
            .bind(entry_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?;
        row.map(Self::mailbox_entry_from_row).transpose()
    }

    async fn load_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let sql = format!(
            "SELECT entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at FROM {} WHERE run_id = $1",
            self.mailbox_table
        );
        let row = sqlx::query_as::<_, MailboxRow>(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?;
        row.map(Self::mailbox_entry_from_row).transpose()
    }

    async fn load_mailbox_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<MailboxThreadState>, MailboxStoreError> {
        let row = sqlx::query_as::<_, (String, i64, i64)>(&format!(
            "SELECT thread_id, current_generation, updated_at FROM {} WHERE thread_id = $1",
            self.mailbox_threads_table
        ))
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;

        row.map(|(thread_id, current_generation, updated_at)| {
            Ok(MailboxThreadState {
                thread_id,
                current_generation: u64::try_from(current_generation).map_err(|_| {
                    MailboxStoreError::Backend(format!(
                        "current_generation cannot be negative in postgres BIGINT: {current_generation}"
                    ))
                })?,
                updated_at: u64::try_from(updated_at).map_err(|_| {
                    MailboxStoreError::Backend(format!(
                        "updated_at cannot be negative in postgres BIGINT: {updated_at}"
                    ))
                })?,
            })
        })
        .transpose()
    }

    async fn list_mailbox_entries(
        &self,
        query: &MailboxQuery,
    ) -> Result<MailboxPage, MailboxStoreError> {
        let status_filter = query.status.map(Self::encode_mailbox_status);
        let sql = format!(
            "SELECT entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at FROM {}",
            self.mailbox_table
        );

        let rows = match (
            query.thread_id.as_deref(),
            query.run_id.as_deref(),
            status_filter,
        ) {
            (Some(thread_id), Some(run_id), Some(status)) => {
                sqlx::query_as::<_, MailboxRow>(&format!(
                    "{sql} WHERE thread_id = $1 AND run_id = $2 AND status = $3 \
                     ORDER BY created_at ASC, entry_id ASC"
                ))
                .bind(thread_id)
                .bind(run_id)
                .bind(status)
                .fetch_all(&self.pool)
                .await
                .map_err(Self::mailbox_sql_err)?
            }
            (Some(thread_id), Some(run_id), None) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE thread_id = $1 AND run_id = $2 \
                     ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(thread_id)
            .bind(run_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (Some(thread_id), None, Some(status)) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE thread_id = $1 AND status = $2 \
                     ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(thread_id)
            .bind(status)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (None, Some(run_id), Some(status)) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE run_id = $1 AND status = $2 \
                     ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(run_id)
            .bind(status)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (Some(thread_id), None, None) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE thread_id = $1 ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (None, Some(run_id), None) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE run_id = $1 ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(run_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (None, None, Some(status)) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} WHERE status = $1 ORDER BY created_at ASC, entry_id ASC"
            ))
            .bind(status)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
            (None, None, None) => sqlx::query_as::<_, MailboxRow>(&format!(
                "{sql} ORDER BY created_at ASC, entry_id ASC"
            ))
            .fetch_all(&self.pool)
            .await
            .map_err(Self::mailbox_sql_err)?,
        };

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(Self::mailbox_entry_from_row(row)?);
        }
        Ok(paginate_mailbox_entries(&entries, query))
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl MailboxWriter for PostgresStore {
    async fn enqueue_mailbox_entry(&self, entry: &MailboxEntry) -> Result<(), MailboxStoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::mailbox_sql_err)?;
        let now_i64 = i64::try_from(entry.updated_at).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?;

        let state_row = sqlx::query_as::<_, (i64,)>(&format!(
            "INSERT INTO {} (thread_id, current_generation, updated_at) VALUES ($1, 0, $2) \
             ON CONFLICT (thread_id) DO UPDATE SET updated_at = EXCLUDED.updated_at \
             RETURNING current_generation",
            self.mailbox_threads_table
        ))
        .bind(&entry.thread_id)
        .bind(now_i64)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;

        let current_generation = u64::try_from(state_row.0).map_err(|_| {
            MailboxStoreError::Backend(format!(
                "current_generation cannot be negative: {}",
                state_row.0
            ))
        })?;
        if current_generation != entry.generation {
            return Err(MailboxStoreError::GenerationMismatch {
                thread_id: entry.thread_id.clone(),
                expected: current_generation,
                actual: entry.generation,
            });
        }

        let request = serde_json::to_value(&entry.request)
            .map_err(|e| MailboxStoreError::Serialization(e.to_string()))?;
        sqlx::query(&format!(
            "INSERT INTO {} (entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
            self.mailbox_table
        ))
        .bind(&entry.entry_id)
        .bind(&entry.thread_id)
        .bind(&entry.run_id)
        .bind(&entry.agent_id)
        .bind(i64::try_from(entry.generation).map_err(|_| {
            MailboxStoreError::Backend("generation too large for postgres BIGINT".to_string())
        })?)
        .bind(Self::encode_mailbox_status(entry.status))
        .bind(request)
        .bind(entry.dedupe_key.as_deref())
        .bind(entry.kind.as_deref())
        .bind(entry.sender_id.as_deref())
        .bind(i16::from(entry.priority))
        .bind(i64::try_from(entry.available_at).map_err(|_| {
            MailboxStoreError::Backend("available_at too large for postgres BIGINT".to_string())
        })?)
        .bind(i32::try_from(entry.attempt_count).map_err(|_| {
            MailboxStoreError::Backend(
                "attempt_count too large for postgres INTEGER".to_string(),
            )
        })?)
        .bind(entry.last_error.as_deref())
        .bind(entry.claim_token.as_deref())
        .bind(entry.claimed_by.as_deref())
        .bind(
            entry
                .lease_until
                .map(i64::try_from)
                .transpose()
                .map_err(|_| {
                    MailboxStoreError::Backend(
                        "lease_until too large for postgres BIGINT".to_string(),
                    )
                })?,
        )
        .bind(entry.accepted_run_id.as_deref())
        .bind(i64::try_from(entry.created_at).map_err(|_| {
            MailboxStoreError::Backend("created_at too large for postgres BIGINT".to_string())
        })?)
        .bind(now_i64)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("duplicate key") || message.contains("unique constraint") {
                MailboxStoreError::AlreadyExists(entry.entry_id.clone())
            } else {
                Self::mailbox_sql_err(error)
            }
        })?;

        tx.commit().await.map_err(Self::mailbox_sql_err)?;
        Ok(())
    }

    async fn ensure_mailbox_thread_state(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxThreadState, MailboxStoreError> {
        let now_i64 = i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?;
        let row = sqlx::query_as::<_, (String, i64, i64)>(&format!(
            "INSERT INTO {} (thread_id, current_generation, updated_at) VALUES ($1, 0, $2) \
             ON CONFLICT (thread_id) DO UPDATE SET updated_at = EXCLUDED.updated_at \
             RETURNING thread_id, current_generation, updated_at",
            self.mailbox_threads_table
        ))
        .bind(thread_id)
        .bind(now_i64)
        .fetch_one(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        Ok(MailboxThreadState {
            thread_id: row.0,
            current_generation: u64::try_from(row.1).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "current_generation cannot be negative in postgres BIGINT: {}",
                    row.1
                ))
            })?,
            updated_at: u64::try_from(row.2).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "updated_at cannot be negative in postgres BIGINT: {}",
                    row.2
                ))
            })?,
        })
    }

    async fn claim_mailbox_entries(
        &self,
        limit: usize,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::mailbox_sql_err)?;
        let limit = i64::try_from(limit)
            .map_err(|_| MailboxStoreError::Backend("claim limit too large".to_string()))?;
        let now_i64 = i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("now too large for postgres BIGINT".to_string())
        })?;
        let rows: Vec<MailboxRow> = sqlx::query_as(&format!(
            "SELECT entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at FROM {} \
             WHERE (status = 'queued' AND available_at <= $1) \
                OR (status = 'claimed' AND lease_until IS NOT NULL AND lease_until <= $1) \
             ORDER BY priority DESC, available_at ASC, created_at ASC, entry_id ASC \
             FOR UPDATE SKIP LOCKED LIMIT $2",
            self.mailbox_table
        ))
        .bind(now_i64)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in rows {
            let mut entry = Self::mailbox_entry_from_row(row)?;
            let claim_token = uuid::Uuid::now_v7().simple().to_string();
            let lease_until = now.saturating_add(lease_duration_ms);
            sqlx::query(&format!(
                "UPDATE {} SET status = $1, claim_token = $2, claimed_by = $3, lease_until = $4, \
                 attempt_count = attempt_count + 1, updated_at = $5 WHERE entry_id = $6",
                self.mailbox_table
            ))
            .bind(Self::encode_mailbox_status(
                tirea_contract::MailboxEntryStatus::Claimed,
            ))
            .bind(&claim_token)
            .bind(consumer_id)
            .bind(i64::try_from(lease_until).map_err(|_| {
                MailboxStoreError::Backend("lease_until too large for postgres BIGINT".to_string())
            })?)
            .bind(now_i64)
            .bind(&entry.entry_id)
            .execute(&mut *tx)
            .await
            .map_err(Self::mailbox_sql_err)?;
            entry.status = tirea_contract::MailboxEntryStatus::Claimed;
            entry.claim_token = Some(claim_token);
            entry.claimed_by = Some(consumer_id.to_string());
            entry.lease_until = Some(lease_until);
            entry.attempt_count = entry.attempt_count.saturating_add(1);
            entry.updated_at = now;
            claimed.push(entry);
        }
        tx.commit().await.map_err(Self::mailbox_sql_err)?;
        Ok(claimed)
    }

    async fn claim_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::mailbox_sql_err)?;
        let now_i64 = i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("now too large for postgres BIGINT".to_string())
        })?;
        let row = sqlx::query_as::<_, MailboxRow>(&format!(
            "SELECT entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at FROM {} \
             WHERE run_id = $1 AND (status = 'queued' OR (status = 'claimed' AND lease_until IS NOT NULL AND lease_until <= $2)) \
             FOR UPDATE SKIP LOCKED",
            self.mailbox_table
        ))
        .bind(run_id)
        .bind(now_i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;

        let Some(row) = row else {
            tx.commit().await.map_err(Self::mailbox_sql_err)?;
            return Ok(None);
        };

        let mut entry = Self::mailbox_entry_from_row(row)?;
        let claim_token = uuid::Uuid::now_v7().simple().to_string();
        let lease_until = now.saturating_add(lease_duration_ms);
        sqlx::query(&format!(
            "UPDATE {} SET status = $1, claim_token = $2, claimed_by = $3, lease_until = $4, \
             attempt_count = attempt_count + 1, updated_at = $5 WHERE entry_id = $6",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(
            tirea_contract::MailboxEntryStatus::Claimed,
        ))
        .bind(&claim_token)
        .bind(consumer_id)
        .bind(i64::try_from(lease_until).map_err(|_| {
            MailboxStoreError::Backend("lease_until too large for postgres BIGINT".to_string())
        })?)
        .bind(now_i64)
        .bind(&entry.entry_id)
        .execute(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;
        tx.commit().await.map_err(Self::mailbox_sql_err)?;

        entry.status = tirea_contract::MailboxEntryStatus::Claimed;
        entry.claim_token = Some(claim_token);
        entry.claimed_by = Some(consumer_id.to_string());
        entry.lease_until = Some(lease_until);
        entry.attempt_count = entry.attempt_count.saturating_add(1);
        entry.updated_at = now;
        Ok(Some(entry))
    }

    async fn ack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        accepted_run_id: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let updated = sqlx::query(&format!(
            "UPDATE {} SET status = $1, accepted_run_id = $2, claim_token = NULL, claimed_by = NULL, \
             lease_until = NULL, updated_at = $3 WHERE entry_id = $4 AND claim_token = $5",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(tirea_contract::MailboxEntryStatus::Accepted))
        .bind(accepted_run_id)
        .bind(i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?)
        .bind(entry_id)
        .bind(claim_token)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        if updated.rows_affected() == 0 {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        Ok(())
    }

    async fn nack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let updated = sqlx::query(&format!(
            "UPDATE {} SET status = $1, available_at = $2, last_error = $3, claim_token = NULL, \
             claimed_by = NULL, lease_until = NULL, updated_at = $4 WHERE entry_id = $5 AND claim_token = $6",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(tirea_contract::MailboxEntryStatus::Queued))
        .bind(i64::try_from(retry_at).map_err(|_| {
            MailboxStoreError::Backend("retry_at too large for postgres BIGINT".to_string())
        })?)
        .bind(error)
        .bind(i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?)
        .bind(entry_id)
        .bind(claim_token)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        if updated.rows_affected() == 0 {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        Ok(())
    }

    async fn dead_letter_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let updated = sqlx::query(&format!(
            "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
             lease_until = NULL, updated_at = $3 WHERE entry_id = $4 AND claim_token = $5",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(
            tirea_contract::MailboxEntryStatus::DeadLetter,
        ))
        .bind(error)
        .bind(i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?)
        .bind(entry_id)
        .bind(claim_token)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        if updated.rows_affected() == 0 {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        Ok(())
    }

    async fn cancel_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
        now: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let updated = sqlx::query(&format!(
            "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
             lease_until = NULL, updated_at = $3 \
             WHERE run_id = $4 AND status NOT IN ('accepted', 'superseded', 'cancelled', 'dead_letter')",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(
            tirea_contract::MailboxEntryStatus::Cancelled,
        ))
        .bind("cancelled")
        .bind(i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        if updated.rows_affected() == 0 {
            return self.load_mailbox_entry_by_run_id(run_id).await;
        }
        self.load_mailbox_entry_by_run_id(run_id).await
    }

    async fn supersede_mailbox_entry(
        &self,
        entry_id: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let updated = sqlx::query(&format!(
            "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
             lease_until = NULL, updated_at = $3 \
             WHERE entry_id = $4 AND status NOT IN ('accepted', 'superseded', 'cancelled', 'dead_letter')",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(
            tirea_contract::MailboxEntryStatus::Superseded,
        ))
        .bind(reason)
        .bind(i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?)
        .bind(entry_id)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        if updated.rows_affected() == 0 {
            return self.load_mailbox_entry(entry_id).await;
        }
        self.load_mailbox_entry(entry_id).await
    }

    async fn cancel_pending_mailbox_for_thread(
        &self,
        thread_id: &str,
        now: u64,
        exclude_run_id: Option<&str>,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError> {
        let now_i64 = i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?;
        let returning_cols = "entry_id, thread_id, run_id, agent_id, generation, status, request, \
             dedupe_key, kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, \
             lease_until, accepted_run_id, created_at, updated_at";

        let rows: Vec<MailboxRow> = match exclude_run_id {
            Some(run_id) => {
                sqlx::query_as(&format!(
                    "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
                     lease_until = NULL, updated_at = $3 WHERE thread_id = $4 AND run_id != $5 \
                     AND status NOT IN ('accepted', 'superseded', 'cancelled', 'dead_letter') \
                     RETURNING {returning_cols}",
                    self.mailbox_table
                ))
                .bind(Self::encode_mailbox_status(tirea_contract::MailboxEntryStatus::Cancelled))
                .bind("cancelled")
                .bind(now_i64)
                .bind(thread_id)
                .bind(run_id)
                .fetch_all(&self.pool)
                .await
                .map_err(Self::mailbox_sql_err)?
            }
            None => {
                sqlx::query_as(&format!(
                    "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
                     lease_until = NULL, updated_at = $3 WHERE thread_id = $4 \
                     AND status NOT IN ('accepted', 'superseded', 'cancelled', 'dead_letter') \
                     RETURNING {returning_cols}",
                    self.mailbox_table
                ))
                .bind(Self::encode_mailbox_status(tirea_contract::MailboxEntryStatus::Cancelled))
                .bind("cancelled")
                .bind(now_i64)
                .bind(thread_id)
                .fetch_all(&self.pool)
                .await
                .map_err(Self::mailbox_sql_err)?
            }
        };

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(Self::mailbox_entry_from_row(row)?);
        }
        Ok(entries)
    }

    async fn interrupt_mailbox_thread(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxThreadInterrupt, MailboxStoreError> {
        let mut tx = self.pool.begin().await.map_err(Self::mailbox_sql_err)?;
        let now_i64 = i64::try_from(now).map_err(|_| {
            MailboxStoreError::Backend("updated_at too large for postgres BIGINT".to_string())
        })?;
        let state_row = sqlx::query_as::<_, (String, i64, i64)>(&format!(
            "INSERT INTO {} (thread_id, current_generation, updated_at) VALUES ($1, 1, $2) \
             ON CONFLICT (thread_id) DO UPDATE SET current_generation = {}.current_generation + 1, updated_at = EXCLUDED.updated_at \
             RETURNING thread_id, current_generation, updated_at",
            self.mailbox_threads_table, self.mailbox_threads_table
        ))
        .bind(thread_id)
        .bind(now_i64)
        .fetch_one(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;

        let thread_state = MailboxThreadState {
            thread_id: state_row.0,
            current_generation: u64::try_from(state_row.1).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "current_generation cannot be negative in postgres BIGINT: {}",
                    state_row.1
                ))
            })?,
            updated_at: u64::try_from(state_row.2).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "updated_at cannot be negative in postgres BIGINT: {}",
                    state_row.2
                ))
            })?,
        };

        let rows = sqlx::query_as::<_, MailboxRow>(&format!(
            "UPDATE {} SET status = $1, last_error = $2, claim_token = NULL, claimed_by = NULL, \
             lease_until = NULL, updated_at = $3 \
             WHERE thread_id = $4 AND generation < $5 \
             AND status NOT IN ('accepted', 'superseded', 'cancelled', 'dead_letter') \
             RETURNING entry_id, thread_id, run_id, agent_id, generation, status, request, dedupe_key, \
             kind, sender_id, priority, \
             available_at, attempt_count, last_error, claim_token, claimed_by, lease_until, \
             accepted_run_id, created_at, updated_at",
            self.mailbox_table
        ))
        .bind(Self::encode_mailbox_status(
            tirea_contract::MailboxEntryStatus::Superseded,
        ))
        .bind("superseded by interrupt")
        .bind(now_i64)
        .bind(thread_id)
        .bind(i64::try_from(thread_state.current_generation).map_err(|_| {
            MailboxStoreError::Backend("generation too large for postgres BIGINT".to_string())
        })?)
        .fetch_all(&mut *tx)
        .await
        .map_err(Self::mailbox_sql_err)?;

        tx.commit().await.map_err(Self::mailbox_sql_err)?;

        let mut superseded_entries = Vec::with_capacity(rows.len());
        for row in rows {
            superseded_entries.push(Self::mailbox_entry_from_row(row)?);
        }

        Ok(MailboxThreadInterrupt {
            thread_state,
            superseded_entries,
        })
    }

    async fn purge_terminal_mailbox_entries(
        &self,
        older_than: u64,
    ) -> Result<usize, MailboxStoreError> {
        let older_than_i64 = i64::try_from(older_than).map_err(|_| {
            MailboxStoreError::Backend("older_than too large for postgres BIGINT".to_string())
        })?;
        let result = sqlx::query(&format!(
            "DELETE FROM {} WHERE status IN ('accepted', 'superseded', 'cancelled', 'dead_letter') \
             AND updated_at < $1",
            self.mailbox_table
        ))
        .bind(older_than_i64)
        .execute(&self.pool)
        .await
        .map_err(Self::mailbox_sql_err)?;
        Ok(result.rows_affected() as usize)
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl ThreadWriter for PostgresStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        let mut v = serde_json::to_value(thread)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("messages".to_string(), serde_json::Value::Array(Vec::new()));
            obj.insert("_version".to_string(), serde_json::Value::Number(0.into()));
        }

        let sql = format!(
            "INSERT INTO {} (id, data, updated_at) VALUES ($1, $2, now())",
            self.table
        );
        sqlx::query(&sql)
            .bind(&thread.id)
            .bind(&v)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if e.to_string().contains("duplicate key")
                    || e.to_string().contains("unique constraint")
                {
                    ThreadStoreError::AlreadyExists
                } else {
                    Self::sql_err(e)
                }
            })?;

        // Insert messages into separate table.
        let insert_sql = format!(
            "INSERT INTO {} (session_id, message_id, run_id, step_index, data) VALUES ($1, $2, $3, $4, $5)",
            self.messages_table,
        );
        for msg in &thread.messages {
            let data = serde_json::to_value(msg.as_ref())
                .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
            let message_id = msg.id.as_deref();
            let (run_id, step_index) = msg
                .metadata
                .as_ref()
                .map(|m| (m.run_id.as_deref(), m.step_index.map(|s| s as i32)))
                .unwrap_or((None, None));
            sqlx::query(&insert_sql)
                .bind(&thread.id)
                .bind(message_id)
                .bind(run_id)
                .bind(step_index)
                .bind(&data)
                .execute(&self.pool)
                .await
                .map_err(Self::sql_err)?;
        }

        Ok(Committed { version: 0 })
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        let mut tx = self.pool.begin().await.map_err(Self::sql_err)?;

        // Lock the row for atomic read-modify-write.
        let sql = format!("SELECT data FROM {} WHERE id = $1 FOR UPDATE", self.table);
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Self::sql_err)?;

        let Some((mut v,)) = row else {
            return Err(ThreadStoreError::NotFound(thread_id.to_string()));
        };

        let current_version = v.get("_version").and_then(|v| v.as_u64()).unwrap_or(0);
        if let VersionPrecondition::Exact(expected) = precondition {
            if current_version != expected {
                return Err(ThreadStoreError::VersionConflict {
                    expected,
                    actual: current_version,
                });
            }
        }
        let new_version = current_version + 1;

        // Apply snapshot or patches to stored data.
        if let Some(ref snapshot) = delta.snapshot {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("state".to_string(), snapshot.clone());
                obj.insert("patches".to_string(), serde_json::Value::Array(Vec::new()));
            }
        } else if !delta.patches.is_empty() {
            let patches_arr = v
                .get("patches")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let mut patches: Vec<serde_json::Value> =
                if let serde_json::Value::Array(arr) = patches_arr {
                    arr
                } else {
                    Vec::new()
                };
            for p in &delta.patches {
                if let Ok(pv) = serde_json::to_value(p) {
                    patches.push(pv);
                }
            }
            if let Some(obj) = v.as_object_mut() {
                obj.insert("patches".to_string(), serde_json::Value::Array(patches));
            }
        }

        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "_version".to_string(),
                serde_json::Value::Number(new_version.into()),
            );
        }

        let update_sql = format!(
            "UPDATE {} SET data = $1, updated_at = now() WHERE id = $2",
            self.table
        );
        sqlx::query(&update_sql)
            .bind(&v)
            .bind(thread_id)
            .execute(&mut *tx)
            .await
            .map_err(Self::sql_err)?;

        // Append new messages.
        if !delta.messages.is_empty() {
            let insert_sql = format!(
                "INSERT INTO {} (session_id, message_id, run_id, step_index, data) VALUES ($1, $2, $3, $4, $5)",
                self.messages_table,
            );
            for msg in &delta.messages {
                let data = serde_json::to_value(msg.as_ref())
                    .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
                let message_id = msg.id.as_deref();
                let (run_id, step_index) = msg
                    .metadata
                    .as_ref()
                    .map(|m| (m.run_id.as_deref(), m.step_index.map(|s| s as i32)))
                    .unwrap_or((None, None));
                sqlx::query(&insert_sql)
                    .bind(thread_id)
                    .bind(message_id)
                    .bind(run_id)
                    .bind(step_index)
                    .bind(&data)
                    .execute(&mut *tx)
                    .await
                    .map_err(Self::sql_err)?;
            }
        }

        tx.commit().await.map_err(Self::sql_err)?;
        Ok(Committed {
            version: new_version,
        })
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        // CASCADE will delete messages automatically.
        let sql = format!("DELETE FROM {} WHERE id = $1", self.table);
        sqlx::query(&sql)
            .bind(thread_id)
            .execute(&self.pool)
            .await
            .map_err(Self::sql_err)?;
        Ok(())
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        // Serialize session skeleton (without messages).
        let mut v = serde_json::to_value(thread)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("messages".to_string(), serde_json::Value::Array(Vec::new()));
        }

        // Use a transaction to keep sessions and messages consistent.
        let mut tx = self.pool.begin().await.map_err(Self::sql_err)?;

        // Lock existing row to preserve save-version semantics (create = 0, update = +1).
        let select_sql = format!("SELECT data FROM {} WHERE id = $1 FOR UPDATE", self.table);
        let existing: Option<(serde_json::Value,)> = sqlx::query_as(&select_sql)
            .bind(&thread.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Self::sql_err)?;

        let next_version = existing
            .as_ref()
            .and_then(|(data,)| data.get("_version").and_then(serde_json::Value::as_u64))
            .map_or(0, |version| version.saturating_add(1));
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "_version".to_string(),
                serde_json::Value::Number(next_version.into()),
            );
        }

        if existing.is_some() {
            let update_sql = format!(
                "UPDATE {} SET data = $1, updated_at = now() WHERE id = $2",
                self.table
            );
            sqlx::query(&update_sql)
                .bind(&v)
                .bind(&thread.id)
                .execute(&mut *tx)
                .await
                .map_err(Self::sql_err)?;
        } else {
            let insert_sql = format!(
                "INSERT INTO {} (id, data, updated_at) VALUES ($1, $2, now())",
                self.table
            );
            sqlx::query(&insert_sql)
                .bind(&thread.id)
                .bind(&v)
                .execute(&mut *tx)
                .await
                .map_err(Self::sql_err)?;
        }

        // `save()` is replace semantics: persist exactly the provided message set.
        let delete_messages_sql =
            format!("DELETE FROM {} WHERE session_id = $1", self.messages_table);
        sqlx::query(&delete_messages_sql)
            .bind(&thread.id)
            .execute(&mut *tx)
            .await
            .map_err(Self::sql_err)?;

        if !thread.messages.is_empty() {
            let mut rows = Vec::with_capacity(thread.messages.len());
            for msg in &thread.messages {
                let data = serde_json::to_value(msg.as_ref())
                    .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
                let message_id = msg.id.clone();
                let (run_id, step_index) = msg
                    .metadata
                    .as_ref()
                    .map(|m| (m.run_id.clone(), m.step_index.map(|s| s as i32)))
                    .unwrap_or((None, None));
                rows.push((message_id, run_id, step_index, data));
            }

            let mut qb = QueryBuilder::<Postgres>::new(format!(
                "INSERT INTO {} (session_id, message_id, run_id, step_index, data) ",
                self.messages_table
            ));
            qb.push_values(
                rows.iter(),
                |mut b, (message_id, run_id, step_index, data)| {
                    b.push_bind(&thread.id)
                        .push_bind(message_id.as_deref())
                        .push_bind(run_id.as_deref())
                        .push_bind(*step_index)
                        .push_bind(data);
                },
            );
            qb.build().execute(&mut *tx).await.map_err(Self::sql_err)?;
        }

        tx.commit().await.map_err(Self::sql_err)?;
        Ok(())
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl ThreadReader for PostgresStore {
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        let sql = format!("SELECT data FROM {} WHERE id = $1", self.table);
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::sql_err)?;

        let Some((mut v,)) = row else {
            return Ok(None);
        };

        let version = v.get("_version").and_then(|v| v.as_u64()).unwrap_or(0);

        let msg_sql = format!(
            "SELECT data FROM {} WHERE session_id = $1 ORDER BY seq",
            self.messages_table
        );
        let msg_rows: Vec<(serde_json::Value,)> = sqlx::query_as(&msg_sql)
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::sql_err)?;

        let messages: Vec<serde_json::Value> = msg_rows.into_iter().map(|(d,)| d).collect();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("messages".to_string(), serde_json::Value::Array(messages));
            obj.remove("_version");
        }

        let thread: Thread = serde_json::from_value(v)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;
        Ok(Some(ThreadHead { thread, version }))
    }

    async fn load_messages(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        // Check session exists.
        let exists_sql = format!("SELECT 1 FROM {} WHERE id = $1", self.table);
        let exists: Option<(i32,)> = sqlx::query_as(&exists_sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::sql_err)?;
        if exists.is_none() {
            return Err(ThreadStoreError::NotFound(thread_id.to_string()));
        }

        let limit = query.limit.clamp(1, 200);
        // Fetch limit+1 rows to determine has_more.
        let fetch_limit = (limit + 1) as i64;

        // Visibility filter on JSONB data.
        let vis_clause = match query.visibility {
            Some(Visibility::All) => {
                " AND COALESCE(data->>'visibility', 'all') = 'all'".to_string()
            }
            Some(Visibility::Internal) => " AND data->>'visibility' = 'internal'".to_string(),
            None => String::new(),
        };

        // Run ID filter on the run_id column.
        let run_clause = if query.run_id.is_some() {
            " AND run_id = $4"
        } else {
            ""
        };

        let extra_param_idx = if query.run_id.is_some() { 5 } else { 4 };

        let (sql, cursor_val) = match query.order {
            SortOrder::Asc => {
                let cursor = query.after.unwrap_or(-1);
                let before_clause = if query.before.is_some() {
                    format!("AND seq < ${extra_param_idx}")
                } else {
                    String::new()
                };
                let sql = format!(
                    "SELECT seq, data FROM {} WHERE session_id = $1 AND seq > $2{}{} {} ORDER BY seq ASC LIMIT $3",
                    self.messages_table, vis_clause, run_clause, before_clause,
                );
                (sql, cursor)
            }
            SortOrder::Desc => {
                let cursor = query.before.unwrap_or(i64::MAX);
                let after_clause = if query.after.is_some() {
                    format!("AND seq > ${extra_param_idx}")
                } else {
                    String::new()
                };
                let sql = format!(
                    "SELECT seq, data FROM {} WHERE session_id = $1 AND seq < $2{}{} {} ORDER BY seq DESC LIMIT $3",
                    self.messages_table, vis_clause, run_clause, after_clause,
                );
                (sql, cursor)
            }
        };

        let rows: Vec<(i64, serde_json::Value)> = match query.order {
            SortOrder::Asc => {
                let mut q = sqlx::query_as(&sql)
                    .bind(thread_id)
                    .bind(cursor_val)
                    .bind(fetch_limit);
                if let Some(ref rid) = query.run_id {
                    q = q.bind(rid);
                }
                if let Some(before) = query.before {
                    q = q.bind(before);
                }
                q.fetch_all(&self.pool).await.map_err(Self::sql_err)?
            }
            SortOrder::Desc => {
                let mut q = sqlx::query_as(&sql)
                    .bind(thread_id)
                    .bind(cursor_val)
                    .bind(fetch_limit);
                if let Some(ref rid) = query.run_id {
                    q = q.bind(rid);
                }
                if let Some(after) = query.after {
                    q = q.bind(after);
                }
                q.fetch_all(&self.pool).await.map_err(Self::sql_err)?
            }
        };

        let has_more = rows.len() > limit;
        let limited: Vec<_> = rows.into_iter().take(limit).collect();

        let messages: Vec<MessageWithCursor> = limited
            .into_iter()
            .map(
                |(seq, data)| -> Result<MessageWithCursor, ThreadStoreError> {
                    let message: Message = serde_json::from_value(data).map_err(|e| {
                        ThreadStoreError::Serialization(format!(
                        "failed to deserialize message row (thread_id={thread_id}, seq={seq}): {e}"
                    ))
                    })?;
                    Ok(MessageWithCursor {
                        cursor: seq,
                        message,
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MessagePage {
            next_cursor: messages.last().map(|m| m.cursor),
            prev_cursor: messages.first().map(|m| m.cursor),
            messages,
            has_more,
        })
    }

    async fn message_count(&self, thread_id: &str) -> Result<usize, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        // Check session exists.
        let exists_sql = format!("SELECT 1 FROM {} WHERE id = $1", self.table);
        let exists: Option<(i32,)> = sqlx::query_as(&exists_sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::sql_err)?;
        if exists.is_none() {
            return Err(ThreadStoreError::NotFound(thread_id.to_string()));
        }

        let sql = format!(
            "SELECT COUNT(*)::bigint FROM {} WHERE session_id = $1",
            self.messages_table
        );
        let row: (i64,) = sqlx::query_as(&sql)
            .bind(thread_id)
            .fetch_one(&self.pool)
            .await
            .map_err(Self::sql_err)?;
        Ok(row.0 as usize)
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.ensure_thread_schema_ready().await?;

        let limit = query.limit.clamp(1, 200);
        let fetch_limit = (limit + 1) as i64;
        let offset = query.offset as i64;

        let mut count_filters = Vec::new();
        let mut data_filters = Vec::new();
        if query.resource_id.is_some() {
            count_filters.push("data->>'resource_id' = $1".to_string());
            data_filters.push("data->>'resource_id' = $3".to_string());
        }
        if query.parent_thread_id.is_some() {
            let idx = if query.resource_id.is_some() { 2 } else { 1 };
            count_filters.push(format!("data->>'parent_thread_id' = ${idx}"));
            let data_idx = if query.resource_id.is_some() { 4 } else { 3 };
            data_filters.push(format!("data->>'parent_thread_id' = ${data_idx}"));
        }

        let where_count = if count_filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", count_filters.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*)::bigint FROM {}{}", self.table, where_count);
        let where_data = if data_filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", data_filters.join(" AND "))
        };
        let data_sql = format!(
            "SELECT id FROM {}{} ORDER BY id LIMIT $1 OFFSET $2",
            self.table, where_data
        );

        let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
        if let Some(ref rid) = query.resource_id {
            count_q = count_q.bind(rid);
        }
        if let Some(ref pid) = query.parent_thread_id {
            count_q = count_q.bind(pid);
        }
        let total = count_q.fetch_one(&self.pool).await.map_err(Self::sql_err)?;

        let mut data_q = sqlx::query_scalar::<_, String>(&data_sql)
            .bind(fetch_limit)
            .bind(offset);
        if let Some(ref rid) = query.resource_id {
            data_q = data_q.bind(rid);
        }
        if let Some(ref pid) = query.parent_thread_id {
            data_q = data_q.bind(pid);
        }
        let rows: Vec<String> = data_q.fetch_all(&self.pool).await.map_err(Self::sql_err)?;

        let has_more = rows.len() > limit;
        let items = rows.into_iter().take(limit).collect();

        Ok(ThreadListPage {
            items,
            total: total as usize,
            has_more,
        })
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        <Self as RunReader>::load_run(self, run_id)
            .await
            .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e.to_string())))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
        <Self as RunReader>::list_runs(self, query)
            .await
            .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e.to_string())))
    }

    async fn active_run_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<RunRecord>, ThreadStoreError> {
        <Self as RunReader>::load_current_run(self, thread_id)
            .await
            .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e.to_string())))
    }
}

#[cfg(feature = "postgres")]
type RunRowTuple = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<String>,
    Option<serde_json::Value>,
);

#[cfg(feature = "postgres")]
struct MailboxRow {
    entry_id: String,
    thread_id: String,
    run_id: String,
    agent_id: String,
    generation: i64,
    status: String,
    request: serde_json::Value,
    dedupe_key: Option<String>,
    kind: Option<String>,
    sender_id: Option<String>,
    priority: i16,
    available_at: i64,
    attempt_count: i32,
    last_error: Option<String>,
    claim_token: Option<String>,
    claimed_by: Option<String>,
    lease_until: Option<i64>,
    accepted_run_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[cfg(feature = "postgres")]
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for MailboxRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;

        Ok(Self {
            entry_id: row.try_get("entry_id")?,
            thread_id: row.try_get("thread_id")?,
            run_id: row.try_get("run_id")?,
            agent_id: row.try_get("agent_id")?,
            generation: row.try_get("generation")?,
            status: row.try_get("status")?,
            request: row.try_get("request")?,
            dedupe_key: row.try_get("dedupe_key")?,
            kind: row.try_get("kind")?,
            sender_id: row.try_get("sender_id")?,
            priority: row.try_get("priority")?,
            available_at: row.try_get("available_at")?,
            attempt_count: row.try_get("attempt_count")?,
            last_error: row.try_get("last_error")?,
            claim_token: row.try_get("claim_token")?,
            claimed_by: row.try_get("claimed_by")?,
            lease_until: row.try_get("lease_until")?,
            accepted_run_id: row.try_get("accepted_run_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[cfg(feature = "postgres")]
impl PostgresStore {
    fn run_from_row(row: RunRowTuple) -> Result<RunRecord, RunStoreError> {
        let (
            run_id,
            thread_id,
            agent_id,
            parent_run_id,
            parent_thread_id,
            origin,
            status,
            termination_code,
            termination_detail,
            created_at,
            updated_at,
            source_mailbox_entry_id,
            metadata,
        ) = row;
        Ok(RunRecord {
            run_id,
            thread_id,
            agent_id,
            parent_run_id,
            parent_thread_id,
            origin: Self::decode_origin(&origin)?,
            status: Self::decode_status(&status)?,
            termination_code,
            termination_detail,
            created_at: Self::from_db_timestamp(created_at, "created_at")?,
            updated_at: Self::from_db_timestamp(updated_at, "updated_at")?,
            source_mailbox_entry_id,
            metadata,
        })
    }

    fn mailbox_entry_from_row(row: MailboxRow) -> Result<MailboxEntry, MailboxStoreError> {
        let request: tirea_contract::RunRequest = serde_json::from_value(row.request)
            .map_err(|e| MailboxStoreError::Serialization(e.to_string()))?;
        Ok(MailboxEntry {
            entry_id: row.entry_id,
            thread_id: row.thread_id,
            run_id: row.run_id,
            agent_id: row.agent_id,
            generation: u64::try_from(row.generation).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "generation cannot be negative in postgres BIGINT: {}",
                    row.generation
                ))
            })?,
            status: Self::decode_mailbox_status(&row.status)?,
            request,
            dedupe_key: row.dedupe_key,
            kind: row.kind,
            sender_id: row.sender_id,
            priority: u8::try_from(row.priority).unwrap_or(0),
            available_at: Self::from_db_timestamp(row.available_at, "available_at")
                .map_err(|e| MailboxStoreError::Backend(e.to_string()))?,
            attempt_count: u32::try_from(row.attempt_count).map_err(|_| {
                MailboxStoreError::Backend(format!(
                    "attempt_count cannot be negative in postgres INTEGER: {}",
                    row.attempt_count
                ))
            })?,
            last_error: row.last_error,
            claim_token: row.claim_token,
            claimed_by: row.claimed_by,
            lease_until: row
                .lease_until
                .map(|value| {
                    Self::from_db_timestamp(value, "lease_until")
                        .map_err(|e| MailboxStoreError::Backend(e.to_string()))
                })
                .transpose()?,
            accepted_run_id: row.accepted_run_id,
            created_at: Self::from_db_timestamp(row.created_at, "created_at")
                .map_err(|e| MailboxStoreError::Backend(e.to_string()))?,
            updated_at: Self::from_db_timestamp(row.updated_at, "updated_at")
                .map_err(|e| MailboxStoreError::Backend(e.to_string()))?,
        })
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl RunReader for PostgresStore {
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, parent_thread_id, origin, status, termination_code, termination_detail, created_at, updated_at, source_mailbox_entry_id, metadata FROM {} WHERE run_id = $1",
            self.runs_table
        );
        let row = sqlx::query_as::<_, RunRowTuple>(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        row.map(Self::run_from_row).transpose()
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let limit = query.limit.clamp(1, 200);
        let fetch_limit = (limit + 1) as i64;
        let offset = i64::try_from(query.offset)
            .map_err(|_| RunStoreError::Backend("offset is too large".to_string()))?;

        let mut count_qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT COUNT(*)::bigint FROM {}",
            self.runs_table
        ));
        let mut has_where = false;
        if let Some(thread_id) = query.thread_id.as_deref() {
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb.push("thread_id = ").push_bind(thread_id);
        }
        if let Some(parent_run_id) = query.parent_run_id.as_deref() {
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb.push("parent_run_id = ").push_bind(parent_run_id);
        }
        if let Some(status) = query.status {
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb
                .push("status = ")
                .push_bind(Self::encode_status(status));
        }
        if let Some(termination_code) = query.termination_code.as_deref() {
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb
                .push("termination_code = ")
                .push_bind(termination_code);
        }
        if let Some(origin) = query.origin {
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb
                .push("origin = ")
                .push_bind(Self::encode_origin(origin));
        }
        if let Some(created_at_from) = query.created_at_from {
            let created_at_from = Self::to_db_timestamp(created_at_from, "created_at_from")?;
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb.push("created_at >= ").push_bind(created_at_from);
        }
        if let Some(created_at_to) = query.created_at_to {
            let created_at_to = Self::to_db_timestamp(created_at_to, "created_at_to")?;
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb.push("created_at <= ").push_bind(created_at_to);
        }
        if let Some(updated_at_from) = query.updated_at_from {
            let updated_at_from = Self::to_db_timestamp(updated_at_from, "updated_at_from")?;
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            count_qb.push("updated_at >= ").push_bind(updated_at_from);
        }
        if let Some(updated_at_to) = query.updated_at_to {
            let updated_at_to = Self::to_db_timestamp(updated_at_to, "updated_at_to")?;
            count_qb.push(if has_where { " AND " } else { " WHERE " });
            count_qb.push("updated_at <= ").push_bind(updated_at_to);
        }
        let total: i64 = count_qb
            .build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;

        let mut data_qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, parent_thread_id, origin, status, termination_code, termination_detail, created_at, updated_at, source_mailbox_entry_id, metadata FROM {}",
            self.runs_table
        ));
        let mut has_where = false;
        if let Some(thread_id) = query.thread_id.as_deref() {
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb.push("thread_id = ").push_bind(thread_id);
        }
        if let Some(parent_run_id) = query.parent_run_id.as_deref() {
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb.push("parent_run_id = ").push_bind(parent_run_id);
        }
        if let Some(status) = query.status {
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb
                .push("status = ")
                .push_bind(Self::encode_status(status));
        }
        if let Some(termination_code) = query.termination_code.as_deref() {
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb
                .push("termination_code = ")
                .push_bind(termination_code);
        }
        if let Some(origin) = query.origin {
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb
                .push("origin = ")
                .push_bind(Self::encode_origin(origin));
        }
        if let Some(created_at_from) = query.created_at_from {
            let created_at_from = Self::to_db_timestamp(created_at_from, "created_at_from")?;
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb.push("created_at >= ").push_bind(created_at_from);
        }
        if let Some(created_at_to) = query.created_at_to {
            let created_at_to = Self::to_db_timestamp(created_at_to, "created_at_to")?;
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb.push("created_at <= ").push_bind(created_at_to);
        }
        if let Some(updated_at_from) = query.updated_at_from {
            let updated_at_from = Self::to_db_timestamp(updated_at_from, "updated_at_from")?;
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            has_where = true;
            data_qb.push("updated_at >= ").push_bind(updated_at_from);
        }
        if let Some(updated_at_to) = query.updated_at_to {
            let updated_at_to = Self::to_db_timestamp(updated_at_to, "updated_at_to")?;
            data_qb.push(if has_where { " AND " } else { " WHERE " });
            data_qb.push("updated_at <= ").push_bind(updated_at_to);
        }
        data_qb
            .push(" ORDER BY created_at ASC, run_id ASC LIMIT ")
            .push_bind(fetch_limit)
            .push(" OFFSET ")
            .push_bind(offset);

        let rows: Vec<RunRowTuple> = data_qb
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        let has_more = rows.len() > limit;
        let items = rows
            .into_iter()
            .take(limit)
            .map(Self::run_from_row)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RunPage {
            items,
            total: usize::try_from(total)
                .map_err(|_| RunStoreError::Backend("total is negative".to_string()))?,
            has_more,
        })
    }

    async fn resolve_thread_id(&self, run_id: &str) -> Result<Option<String>, RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let sql = format!(
            "SELECT thread_id FROM {} WHERE run_id = $1",
            self.runs_table
        );
        sqlx::query_scalar::<_, String>(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::run_sql_err)
    }

    async fn load_current_run(&self, thread_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let sql = format!(
            "SELECT run_id, thread_id, agent_id, parent_run_id, parent_thread_id, origin, status, \
             termination_code, termination_detail, created_at, updated_at, source_mailbox_entry_id, metadata \
             FROM {} WHERE thread_id = $1 AND status != $2 \
             ORDER BY created_at DESC, updated_at DESC, run_id DESC LIMIT 1",
            self.runs_table
        );
        let row = sqlx::query_as::<_, RunRowTuple>(&sql)
            .bind(thread_id)
            .bind(Self::encode_status(RunStatus::Done))
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        row.map(Self::run_from_row).transpose()
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl RunWriter for PostgresStore {
    async fn upsert_run(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let created_at = Self::to_db_timestamp(record.created_at, "created_at")?;
        let updated_at = Self::to_db_timestamp(record.updated_at, "updated_at")?;
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, parent_thread_id, origin, status, \
             termination_code, termination_detail, created_at, updated_at, source_mailbox_entry_id, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (run_id) DO UPDATE SET thread_id = EXCLUDED.thread_id, agent_id = EXCLUDED.agent_id, \
             parent_run_id = EXCLUDED.parent_run_id, parent_thread_id = EXCLUDED.parent_thread_id, \
             origin = EXCLUDED.origin, status = EXCLUDED.status, termination_code = EXCLUDED.termination_code, \
             termination_detail = EXCLUDED.termination_detail, created_at = EXCLUDED.created_at, \
             updated_at = EXCLUDED.updated_at, source_mailbox_entry_id = EXCLUDED.source_mailbox_entry_id, \
             metadata = EXCLUDED.metadata",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(&record.agent_id)
            .bind(record.parent_run_id.as_deref())
            .bind(record.parent_thread_id.as_deref())
            .bind(Self::encode_origin(record.origin))
            .bind(Self::encode_status(record.status))
            .bind(record.termination_code.as_deref())
            .bind(record.termination_detail.as_deref())
            .bind(created_at)
            .bind(updated_at)
            .bind(record.source_mailbox_entry_id.as_deref())
            .bind(&record.metadata)
            .execute(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        Ok(())
    }

    async fn delete_run(&self, run_id: &str) -> Result<(), RunStoreError> {
        self.ensure_run_schema_ready().await?;

        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.runs_table);
        sqlx::query(&sql)
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        Ok(())
    }
}
