use async_trait::async_trait;
#[cfg(feature = "postgres")]
use sqlx::{Postgres, QueryBuilder};
use std::collections::HashSet;
use tirea_contract::storage::{
    Committed, MessagePage, MessageQuery, MessageWithCursor, SortOrder, ThreadHead, ThreadListPage,
    ThreadListQuery, ThreadReader, ThreadStoreError, ThreadWriter, VersionPrecondition,
};
use tirea_contract::{Message, Thread, ThreadChangeSet, Visibility};

pub struct PostgresStore {
    pool: sqlx::PgPool,
    table: String,
    messages_table: String,
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
        }
    }

    /// Create a new PostgreSQL storage with a custom table name.
    ///
    /// The messages table will be named `{table}_messages`.
    pub fn with_table(pool: sqlx::PgPool, table: impl Into<String>) -> Self {
        let table = table.into();
        let messages_table = format!("{}_messages", table);
        Self {
            pool,
            table,
            messages_table,
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

        // Upsert session skeleton.
        let upsert_sql = format!(
            r#"
            INSERT INTO {} (id, data, updated_at)
            VALUES ($1, $2, now())
            ON CONFLICT (id) DO UPDATE
            SET data = EXCLUDED.data, updated_at = now()
            "#,
            self.table
        );
        sqlx::query(&upsert_sql)
            .bind(&thread.id)
            .bind(&v)
            .execute(&mut *tx)
            .await
            .map_err(Self::sql_err)?;

        // Incremental append: only INSERT messages not yet persisted.
        let existing_sql = format!(
            "SELECT message_id FROM {} WHERE session_id = $1 AND message_id IS NOT NULL",
            self.messages_table,
        );
        let existing_rows: Vec<(String,)> = sqlx::query_as(&existing_sql)
            .bind(&thread.id)
            .fetch_all(&mut *tx)
            .await
            .map_err(Self::sql_err)?;
        let existing_ids: HashSet<String> = existing_rows.into_iter().map(|(id,)| id).collect();

        let new_messages: Vec<&Message> = thread
            .messages
            .iter()
            .filter(|m| m.id.as_ref().is_none_or(|id| !existing_ids.contains(id)))
            .map(|m| m.as_ref())
            .collect();

        if !new_messages.is_empty() {
            let mut rows = Vec::with_capacity(new_messages.len());
            for msg in &new_messages {
                let data = serde_json::to_value(*msg)
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
