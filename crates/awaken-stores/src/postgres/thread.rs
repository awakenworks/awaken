use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessagePage, MessageQuery, StorageError, ThreadPage,
    ThreadParentFilter, ThreadQuery, ThreadStore, paginate_message_records,
};
use awaken_contract::thread::{Thread, normalize_lineage_id_owned};
use sqlx::{Postgres, Transaction};

use super::PostgresStore;

impl PostgresStore {
    pub(super) fn merge_thread_lineage(
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

    pub(super) fn decode_thread_row(
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

    pub(super) async fn load_thread_tx(
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

    pub(super) async fn save_thread_tx(
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

    pub(super) async fn delete_thread_tx(
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

    pub(super) async fn list_child_threads_tx(
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

    pub(super) async fn validate_thread_hierarchy_tx(
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
