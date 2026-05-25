use async_trait::async_trait;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
    select_pending_for_freeze,
};
use awaken_contract::contract::storage::{RunRecord, StorageError};
use sqlx::{Postgres, Row, Transaction};
use std::collections::HashSet;

use crate::PendingMessageStore;

use super::PostgresStore;

fn pending_not_found(thread_id: &str, pending_id: &str) -> StorageError {
    StorageError::NotFound(format!(
        "pending message '{pending_id}' in thread '{thread_id}'"
    ))
}

fn already_consumed(pending_id: &str) -> StorageError {
    StorageError::Validation(format!(
        "pending message '{pending_id}' is already consumed"
    ))
}

fn duplicate_pending_id(pending_id: &str) -> StorageError {
    StorageError::Validation(format!("pending message '{pending_id}' already exists"))
}

impl PostgresStore {
    async fn load_pending_message_records_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        let sql = format!(
            "SELECT message_id, position, data, delivery_mode, created_at_ms, EXTRACT(EPOCH FROM updated_at)::BIGINT AS updated_at_s
             FROM {}
             WHERE thread_id = $1 AND state = 'pending'
             ORDER BY position ASC, updated_at ASC",
            self.messages_table
        );
        let rows = sqlx::query(&sql)
            .bind(thread_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let message: Message = serde_json::from_value(row.get("data"))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                let delivery_mode = row
                    .try_get::<Option<serde_json::Value>, _>("delivery_mode")
                    .ok()
                    .flatten()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| StorageError::Serialization(e.to_string()))?
                    .unwrap_or_default();
                let position = row
                    .try_get::<Option<i64>, _>("position")
                    .ok()
                    .flatten()
                    .unwrap_or(0) as u64;
                let pending_id = row
                    .try_get::<Option<String>, _>("message_id")
                    .ok()
                    .flatten()
                    .or_else(|| message.id.clone())
                    .ok_or_else(|| {
                        StorageError::Serialization(
                            "pending message row has no message_id or message.id".to_string(),
                        )
                    })?;
                let created_at = row
                    .try_get::<Option<i64>, _>("created_at_ms")
                    .ok()
                    .flatten()
                    .map(|ms| (ms as u64) / 1000);
                let updated_at = row
                    .try_get::<Option<i64>, _>("updated_at_s")
                    .ok()
                    .flatten()
                    .map(|seconds| seconds as u64);
                Ok(PendingMessageRecord {
                    pending_id,
                    thread_id: thread_id.to_owned(),
                    position,
                    message,
                    delivery_mode,
                    created_at,
                    updated_at,
                })
            })
            .collect()
    }

    async fn committed_message_exists_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
        message_id: &str,
    ) -> Result<bool, StorageError> {
        let sql = format!(
            "SELECT 1 FROM {} WHERE thread_id = $1 AND message_id = $2 AND COALESCE(state, 'committed') = 'committed' LIMIT 1",
            self.messages_table
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(message_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn insert_pending_message_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        record: &PendingMessageRecord,
    ) -> Result<(), StorageError> {
        let data = serde_json::to_value(&record.message)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let delivery_mode = serde_json::to_value(record.delivery_mode)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "INSERT INTO {} (thread_id, message_id, state, position, data, delivery_mode, created_at_ms)
             VALUES ($1, $2, 'pending', $3, $4, $5, $6)",
            self.messages_table
        );
        sqlx::query(&sql)
            .bind(&record.thread_id)
            .bind(&record.pending_id)
            .bind(record.position as i64)
            .bind(data)
            .bind(delivery_mode)
            .bind(record.created_at.map(|s| (s * 1000) as i64))
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl PendingMessageStore for PostgresStore {
    async fn load_pending_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let records = self
            .load_pending_message_records_tx(&mut tx, thread_id)
            .await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(records)
    }

    async fn append_pending_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let pending = self
            .load_pending_message_records_tx(&mut tx, thread_id)
            .await?;
        let now = crate::current_millis() / 1000;
        let start_position = pending.len() as u64 + 1;
        let records = messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| {
                let mut record = PendingMessageRecord::from_message(
                    thread_id.to_owned(),
                    start_position + index as u64,
                    message,
                    delivery_mode,
                );
                record.created_at = Some(now);
                record.updated_at = Some(now);
                record
            })
            .collect::<Vec<_>>();
        let mut seen = pending
            .iter()
            .map(|record| record.pending_id.as_str())
            .collect::<HashSet<_>>();
        for record in &records {
            if !seen.insert(record.pending_id.as_str()) {
                return Err(duplicate_pending_id(&record.pending_id));
            }
            if self
                .committed_message_exists_tx(&mut tx, thread_id, &record.pending_id)
                .await?
            {
                return Err(already_consumed(&record.pending_id));
            }
        }
        for record in &records {
            self.insert_pending_message_tx(&mut tx, record).await?;
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(records)
    }

    async fn update_pending_message_record(
        &self,
        thread_id: &str,
        pending_id: &str,
        mut message: Message,
    ) -> Result<PendingMessageRecord, StorageError> {
        self.ensure_schema().await?;
        match message.id.as_deref() {
            Some(message_id) if message_id != pending_id => {
                return Err(StorageError::Validation(format!(
                    "pending message '{pending_id}' cannot change message id to '{message_id}'"
                )));
            }
            Some(_) => {}
            None => message.id = Some(pending_id.to_owned()),
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let data = serde_json::to_value(&message)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "UPDATE {} SET data = $3, updated_at = now()
             WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'
             RETURNING position, delivery_mode, created_at_ms, EXTRACT(EPOCH FROM updated_at)::BIGINT AS updated_at_s",
            self.messages_table
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(pending_id)
            .bind(data)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let Some(row) = row else {
            if self
                .committed_message_exists_tx(&mut tx, thread_id, pending_id)
                .await?
            {
                return Err(already_consumed(pending_id));
            }
            return Err(pending_not_found(thread_id, pending_id));
        };
        let record = PendingMessageRecord {
            pending_id: pending_id.to_owned(),
            thread_id: thread_id.to_owned(),
            position: row
                .try_get::<Option<i64>, _>("position")
                .ok()
                .flatten()
                .unwrap_or(0) as u64,
            message,
            delivery_mode: row
                .try_get::<Option<serde_json::Value>, _>("delivery_mode")
                .ok()
                .flatten()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| StorageError::Serialization(e.to_string()))?
                .unwrap_or_default(),
            created_at: row
                .try_get::<Option<i64>, _>("created_at_ms")
                .ok()
                .flatten()
                .map(|ms| (ms as u64) / 1000),
            updated_at: row
                .try_get::<Option<i64>, _>("updated_at_s")
                .ok()
                .flatten()
                .map(|seconds| seconds as u64),
        };
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(record)
    }

    async fn retract_pending_message_record(
        &self,
        thread_id: &str,
        pending_id: &str,
    ) -> Result<PendingMessageRecord, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let mut pending = self
            .load_pending_message_records_tx(&mut tx, thread_id)
            .await?;
        let Some(index) = pending
            .iter()
            .position(|record| record.pending_id == pending_id)
        else {
            if self
                .committed_message_exists_tx(&mut tx, thread_id, pending_id)
                .await?
            {
                return Err(already_consumed(pending_id));
            }
            return Err(pending_not_found(thread_id, pending_id));
        };
        let removed = pending.remove(index);
        let delete_sql = format!(
            "DELETE FROM {} WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'",
            self.messages_table
        );
        sqlx::query(&delete_sql)
            .bind(thread_id)
            .bind(pending_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        for (index, record) in pending.iter().enumerate() {
            let update_sql = format!(
                "UPDATE {} SET position = $3, updated_at = now()
                 WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'",
                self.messages_table
            );
            sqlx::query(&update_sql)
                .bind(thread_id)
                .bind(&record.pending_id)
                .bind(index as i64 + 1)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(removed)
    }

    async fn reorder_pending_message_records(
        &self,
        thread_id: &str,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let pending = self
            .load_pending_message_records_tx(&mut tx, thread_id)
            .await?;
        let pending_ids = pending
            .iter()
            .map(|record| record.pending_id.as_str())
            .collect::<HashSet<_>>();
        for pending_id in ordered_pending_ids {
            if !pending_ids.contains(pending_id.as_str())
                && self
                    .committed_message_exists_tx(&mut tx, thread_id, pending_id)
                    .await?
            {
                return Err(already_consumed(pending_id));
            }
        }
        if pending.len() != ordered_pending_ids.len() {
            return Err(StorageError::VersionConflict {
                expected: ordered_pending_ids.len() as u64,
                actual: pending.len() as u64,
            });
        }
        let mut by_id = pending
            .iter()
            .cloned()
            .map(|record| (record.pending_id.clone(), record))
            .collect::<std::collections::HashMap<_, _>>();
        let mut reordered = Vec::with_capacity(ordered_pending_ids.len());
        for pending_id in ordered_pending_ids {
            let record = by_id
                .remove(pending_id)
                .ok_or_else(|| StorageError::NotFound(pending_id.clone()))?;
            reordered.push(record);
        }
        if !by_id.is_empty() {
            return Err(StorageError::Validation(format!(
                "reorder for thread '{thread_id}' omitted pending ids"
            )));
        }
        let now = crate::current_millis() / 1000;
        for (index, record) in reordered.iter_mut().enumerate() {
            record.position = index as u64 + 1;
            record.updated_at = Some(now);
            let update_sql = format!(
                "UPDATE {} SET position = $3, updated_at = now()
                 WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'",
                self.messages_table
            );
            sqlx::query(&update_sql)
                .bind(thread_id)
                .bind(&record.pending_id)
                .bind(record.position as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(reordered)
    }

    async fn freeze_pending_message_records(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        self.freeze_pending_message_records_with_run_inner(
            thread_id,
            boundary,
            expected_message_version,
            None,
            None,
        )
        .await
    }

    async fn freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: &[String],
        run: &RunRecord,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        self.freeze_pending_message_records_with_run_inner(
            thread_id,
            boundary,
            expected_message_version,
            Some(expected_pending_ids),
            Some(run),
        )
        .await
    }
}

impl PostgresStore {
    async fn freeze_pending_message_records_with_run_inner(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: Option<&[String]>,
        run: Option<&RunRecord>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let committed = self
            .load_committed_message_records_tx(&mut tx, thread_id)
            .await?;
        let actual = committed.len() as u64;
        if let Some(expected) = expected_message_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut pending = self
            .load_pending_message_records_tx(&mut tx, thread_id)
            .await?;
        let selected_indexes = select_pending_for_freeze(&pending, boundary);
        let selected_ids = selected_indexes
            .iter()
            .map(|index| pending[*index].pending_id.clone())
            .collect::<Vec<_>>();
        if let Some(expected_pending_ids) = expected_pending_ids
            && selected_ids != expected_pending_ids
        {
            return Err(StorageError::VersionConflict {
                expected: expected_pending_ids.len() as u64,
                actual: selected_ids.len() as u64,
            });
        }
        if selected_indexes.is_empty() {
            return Ok(Vec::new());
        }

        let mut selected = Vec::with_capacity(selected_indexes.len());
        for index in selected_indexes.iter().rev() {
            selected.push(pending.remove(*index));
        }
        selected.reverse();
        let delete_sql = format!(
            "DELETE FROM {} WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'",
            self.messages_table
        );
        let mut appended = Vec::with_capacity(selected.len());
        let mut next_seq = actual + 1;
        for record in selected {
            sqlx::query(&delete_sql)
                .bind(thread_id)
                .bind(&record.pending_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
            self.insert_committed_message_tx(&mut tx, thread_id, next_seq, &record.message)
                .await?;
            appended.push(MessageRecord::from_message(
                thread_id.to_owned(),
                next_seq,
                record.message,
            ));
            next_seq += 1;
        }
        for (index, record) in pending.iter().enumerate() {
            let update_sql = format!(
                "UPDATE {} SET position = $3, updated_at = now()
                 WHERE thread_id = $1 AND message_id = $2 AND state = 'pending'",
                self.messages_table
            );
            sqlx::query(&update_sql)
                .bind(thread_id)
                .bind(&record.pending_id)
                .bind(index as i64 + 1)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        if let Some(run) = run {
            self.upsert_thread_and_run_in_tx(&mut tx, thread_id, run)
                .await?;
        }
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(appended)
    }
}
