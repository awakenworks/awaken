use async_trait::async_trait;
#[cfg(feature = "postgres")]
use sqlx::{Postgres, QueryBuilder};
use tirea_contract::storage::{
    Committed, MessagePage, MessageQuery, MessageWithCursor, RunOrigin, RunPage, RunQuery,
    RunReader, RunRecord, RunRecordStatus, RunStoreError, RunWriter, SortOrder, ThreadHead,
    ThreadListPage, ThreadListQuery, ThreadReader, ThreadStoreError, ThreadWriter,
    VersionPrecondition,
};
use tirea_contract::{Message, Thread, ThreadChangeSet, Visibility};

pub struct PostgresStore {
    pool: sqlx::PgPool,
    table: String,
    messages_table: String,
    runs_table: String,
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
        }
    }

    /// Create a new PostgreSQL storage with a custom table name.
    ///
    /// The messages table will be named `{table}_messages`.
    pub fn with_table(pool: sqlx::PgPool, table: impl Into<String>) -> Self {
        let table = table.into();
        let messages_table = format!("{}_messages", table);
        let runs_table = format!("{}_runs", table);
        Self {
            pool,
            table,
            messages_table,
            runs_table,
        }
    }

    /// Ensure the storage tables exist (idempotent).
    pub async fn ensure_table(&self) -> Result<(), ThreadStoreError> {
        let statements = vec![
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
                "CREATE TABLE IF NOT EXISTS {} (run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, parent_run_id TEXT, parent_thread_id TEXT, origin TEXT NOT NULL, status TEXT NOT NULL, termination_code TEXT, termination_detail TEXT, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, metadata JSONB)",
                self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_thread_id ON {} (thread_id)",
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
                "CREATE INDEX IF NOT EXISTS idx_{}_origin ON {} (origin)",
                self.runs_table, self.runs_table
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS idx_{}_created_at ON {} (created_at, run_id)",
                self.runs_table, self.runs_table
            ),
        ];

        for sql in statements {
            sqlx::query(&sql)
                .execute(&self.pool)
                .await
                .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e.to_string())))?;
        }
        Ok(())
    }

    fn sql_err(e: sqlx::Error) -> ThreadStoreError {
        ThreadStoreError::Io(std::io::Error::other(e.to_string()))
    }

    fn run_sql_err(e: sqlx::Error) -> RunStoreError {
        RunStoreError::Io(std::io::Error::other(e.to_string()))
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
            _ => Err(RunStoreError::Serialization(format!(
                "invalid run origin value: {raw}"
            ))),
        }
    }

    fn encode_status(status: RunRecordStatus) -> &'static str {
        match status {
            RunRecordStatus::Submitted => "submitted",
            RunRecordStatus::Working => "working",
            RunRecordStatus::InputRequired => "input_required",
            RunRecordStatus::AuthRequired => "auth_required",
            RunRecordStatus::Completed => "completed",
            RunRecordStatus::Failed => "failed",
            RunRecordStatus::Canceled => "canceled",
            RunRecordStatus::Rejected => "rejected",
        }
    }

    fn decode_status(raw: &str) -> Result<RunRecordStatus, RunStoreError> {
        match raw {
            "submitted" => Ok(RunRecordStatus::Submitted),
            "working" => Ok(RunRecordStatus::Working),
            "input_required" => Ok(RunRecordStatus::InputRequired),
            "auth_required" => Ok(RunRecordStatus::AuthRequired),
            "completed" => Ok(RunRecordStatus::Completed),
            "failed" => Ok(RunRecordStatus::Failed),
            "canceled" | "cancelled" => Ok(RunRecordStatus::Canceled),
            "rejected" => Ok(RunRecordStatus::Rejected),
            _ => Err(RunStoreError::Serialization(format!(
                "invalid run status value: {raw}"
            ))),
        }
    }

    fn to_db_timestamp(value: u64, field: &str) -> Result<i64, RunStoreError> {
        i64::try_from(value).map_err(|_| {
            RunStoreError::Serialization(format!(
                "{field} is too large for postgres BIGINT: {value}"
            ))
        })
    }

    fn from_db_timestamp(value: i64, field: &str) -> Result<u64, RunStoreError> {
        u64::try_from(value).map_err(|_| {
            RunStoreError::Serialization(format!(
                "{field} cannot be negative in postgres BIGINT: {value}"
            ))
        })
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl ThreadWriter for PostgresStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
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
}

#[cfg(feature = "postgres")]
type RunRowTuple = (
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
    Option<serde_json::Value>,
);

#[cfg(feature = "postgres")]
impl PostgresStore {
    fn run_from_row(row: RunRowTuple) -> Result<RunRecord, RunStoreError> {
        let (
            run_id,
            thread_id,
            parent_run_id,
            parent_thread_id,
            origin,
            status,
            termination_code,
            termination_detail,
            created_at,
            updated_at,
            metadata,
        ) = row;
        Ok(RunRecord {
            run_id,
            thread_id,
            parent_run_id,
            parent_thread_id,
            origin: Self::decode_origin(&origin)?,
            status: Self::decode_status(&status)?,
            termination_code,
            termination_detail,
            created_at: Self::from_db_timestamp(created_at, "created_at")?,
            updated_at: Self::from_db_timestamp(updated_at, "updated_at")?,
            metadata,
        })
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl RunReader for PostgresStore {
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        let sql = format!(
            "SELECT run_id, thread_id, parent_run_id, parent_thread_id, origin, status, termination_code, termination_detail, created_at, updated_at, metadata FROM {} WHERE run_id = $1",
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
        let limit = query.limit.clamp(1, 200);
        let fetch_limit = (limit + 1) as i64;
        let offset = i64::try_from(query.offset)
            .map_err(|_| RunStoreError::Serialization("offset is too large".to_string()))?;

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
            "SELECT run_id, thread_id, parent_run_id, parent_thread_id, origin, status, termination_code, termination_detail, created_at, updated_at, metadata FROM {}",
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
                .map_err(|_| RunStoreError::Serialization("total is negative".to_string()))?,
            has_more,
        })
    }

    async fn resolve_thread_id(&self, run_id: &str) -> Result<Option<String>, RunStoreError> {
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
}

#[cfg(feature = "postgres")]
#[async_trait]
impl RunWriter for PostgresStore {
    async fn upsert_run(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        let created_at = Self::to_db_timestamp(record.created_at, "created_at")?;
        let updated_at = Self::to_db_timestamp(record.updated_at, "updated_at")?;
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, parent_run_id, parent_thread_id, origin, status, termination_code, termination_detail, created_at, updated_at, metadata) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (run_id) DO UPDATE SET thread_id = EXCLUDED.thread_id, parent_run_id = EXCLUDED.parent_run_id, parent_thread_id = EXCLUDED.parent_thread_id, origin = EXCLUDED.origin, status = EXCLUDED.status, termination_code = EXCLUDED.termination_code, termination_detail = EXCLUDED.termination_detail, created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at, metadata = EXCLUDED.metadata",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(record.parent_run_id.as_deref())
            .bind(record.parent_thread_id.as_deref())
            .bind(Self::encode_origin(record.origin))
            .bind(Self::encode_status(record.status))
            .bind(record.termination_code.as_deref())
            .bind(record.termination_detail.as_deref())
            .bind(created_at)
            .bind(updated_at)
            .bind(&record.metadata)
            .execute(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        Ok(())
    }

    async fn delete_run(&self, run_id: &str) -> Result<(), RunStoreError> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.runs_table);
        sqlx::query(&sql)
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map_err(Self::run_sql_err)?;
        Ok(())
    }
}
