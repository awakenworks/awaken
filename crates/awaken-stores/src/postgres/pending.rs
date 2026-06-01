use async_trait::async_trait;
use awaken_server_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
    pending_queue_revision, select_pending_for_freeze, select_pending_for_freeze_for_run,
};
use awaken_server_contract::contract::storage::{RunRecord, StorageError, message_append};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use std::collections::HashSet;

use crate::PendingMessageStore;
use crate::pending_message_store::validate_pending_message_record;

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

fn pending_row_decode_error(column: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError::Serialization(format!(
        "failed to decode pending message column '{column}': {error}"
    ))
}

fn optional_i64(row: &PgRow, column: &str) -> Result<Option<i64>, StorageError> {
    row.try_get::<Option<i64>, _>(column)
        .map_err(|error| pending_row_decode_error(column, error))
}

fn optional_string(row: &PgRow, column: &str) -> Result<Option<String>, StorageError> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|error| pending_row_decode_error(column, error))
}

fn optional_json(row: &PgRow, column: &str) -> Result<Option<serde_json::Value>, StorageError> {
    row.try_get::<Option<serde_json::Value>, _>(column)
        .map_err(|error| pending_row_decode_error(column, error))
}

fn required_nonnegative_u64(row: &PgRow, column: &str, label: &str) -> Result<u64, StorageError> {
    let value = optional_i64(row, column)?.ok_or_else(|| {
        StorageError::Serialization(format!("pending message row missing {label}"))
    })?;
    u64::try_from(value).map_err(|_| {
        StorageError::Serialization(format!(
            "pending message {label} must be non-negative, got {value}"
        ))
    })
}

fn optional_epoch_u64(row: &PgRow, column: &str, unit: &str) -> Result<Option<u64>, StorageError> {
    optional_i64(row, column)?
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                StorageError::Serialization(format!(
                    "pending message {column} {unit} must be non-negative, got {value}"
                ))
            })
        })
        .transpose()
}

fn decode_delivery_mode(row: &PgRow) -> Result<DeliveryMode, StorageError> {
    optional_json(row, "delivery_mode")?
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| StorageError::Serialization(e.to_string()))
        .map(|mode| mode.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_row_decode_error_includes_column_name() {
        let error = pending_row_decode_error("position", "wrong type");
        assert!(matches!(error, StorageError::Serialization(_)));
        assert!(error.to_string().contains("position"));
    }
}

impl PostgresStore {
    async fn load_pending_message_records_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        let sql = format!(
            "SELECT message_id, position, data, pending_revision, delivery_mode, created_at_ms, EXTRACT(EPOCH FROM updated_at)::BIGINT AS updated_at_s
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
                let delivery_mode = decode_delivery_mode(&row)?;
                let position = required_nonnegative_u64(&row, "position", "position")?;
                let pending_id = optional_string(&row, "message_id")?
                    .or_else(|| message.id.clone())
                    .ok_or_else(|| {
                        StorageError::Serialization(
                            "pending message row has no message_id or message.id".to_string(),
                        )
                    })?;
                let created_at =
                    optional_epoch_u64(&row, "created_at_ms", "milliseconds")?.map(|ms| ms / 1000);
                let updated_at = optional_epoch_u64(&row, "updated_at_s", "seconds")?;
                let record = PendingMessageRecord {
                    pending_id,
                    thread_id: thread_id.to_owned(),
                    position,
                    message,
                    revision: required_nonnegative_u64(
                        &row,
                        "pending_revision",
                        "pending_revision",
                    )?,
                    delivery_mode,
                    created_at,
                    updated_at,
                };
                validate_pending_message_record(&record)?;
                Ok(record)
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
        validate_pending_message_record(record)?;
        let data = serde_json::to_value(&record.message)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let delivery_mode = serde_json::to_value(record.delivery_mode.clone())
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "INSERT INTO {} (thread_id, message_id, state, position, data, pending_revision, delivery_mode, created_at_ms)
             VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7)",
            self.messages_table
        );
        sqlx::query(&sql)
            .bind(&record.thread_id)
            .bind(&record.pending_id)
            .bind(record.position as i64)
            .bind(data)
            .bind(record.revision as i64)
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

    async fn list_threads_with_pending_messages(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<String>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT DISTINCT thread_id FROM {} \
             WHERE state = 'pending' AND ($2::text IS NULL OR thread_id > $2) \
             ORDER BY thread_id LIMIT $1",
            self.messages_table
        );
        let bound = if limit == 0 { i64::MAX } else { limit as i64 };
        let rows = sqlx::query_scalar::<_, String>(&sql)
            .bind(bound)
            .bind(after)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(rows)
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
                    delivery_mode.clone(),
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
            validate_pending_message_record(record)?;
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

    async fn update_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
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
        let pending_message = PendingMessageRecord {
            pending_id: pending_id.to_owned(),
            thread_id: thread_id.to_owned(),
            position: 1,
            message: message.clone(),
            revision: 1,
            delivery_mode: DeliveryMode::default(),
            created_at: None,
            updated_at: None,
        };
        validate_pending_message_record(&pending_message)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.lock_thread_messages_tx(&mut tx, thread_id).await?;
        let data = serde_json::to_value(&message)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let sql = format!(
            "UPDATE {} SET data = $3, pending_revision = pending_revision + 1, updated_at = now()
             WHERE thread_id = $1 AND message_id = $2 AND state = 'pending' AND ($4::BIGINT IS NULL OR pending_revision = $4)
             RETURNING position, pending_revision, delivery_mode, created_at_ms, EXTRACT(EPOCH FROM updated_at)::BIGINT AS updated_at_s",
            self.messages_table
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(pending_id)
            .bind(data)
            .bind(expected_revision.map(|revision| revision as i64))
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let Some(row) = row else {
            if let Some(expected) = expected_revision
                && let Some(actual) = self
                    .load_pending_message_records_tx(&mut tx, thread_id)
                    .await?
                    .into_iter()
                    .find(|record| record.pending_id == pending_id)
                    .map(|record| record.revision)
            {
                return Err(StorageError::VersionConflict { expected, actual });
            }
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
            position: required_nonnegative_u64(&row, "position", "position")?,
            revision: required_nonnegative_u64(&row, "pending_revision", "pending_revision")?,
            message,
            delivery_mode: decode_delivery_mode(&row)?,
            created_at: optional_epoch_u64(&row, "created_at_ms", "milliseconds")?
                .map(|ms| ms / 1000),
            updated_at: optional_epoch_u64(&row, "updated_at_s", "seconds")?,
        };
        validate_pending_message_record(&record)?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(record)
    }

    async fn retract_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
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
        if let Some(expected) = expected_revision
            && pending[index].revision != expected
        {
            return Err(StorageError::VersionConflict {
                expected,
                actual: pending[index].revision,
            });
        }
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
                "UPDATE {} SET position = $3, pending_revision = pending_revision + 1, updated_at = now()
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

    async fn reorder_pending_message_records_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
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
        let actual_queue_revision = pending_queue_revision(&pending);
        if let Some(expected) = expected_queue_revision
            && expected != actual_queue_revision
        {
            return Err(StorageError::VersionConflict {
                expected,
                actual: actual_queue_revision,
            });
        }
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
            record.revision += 1;
            record.updated_at = Some(now);
            let update_sql = format!(
                "UPDATE {} SET position = $3, pending_revision = pending_revision + 1, updated_at = now()
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
            None,
        )
        .await
    }

    async fn append_and_freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        new_messages: &[Message],
        append_delivery_mode: DeliveryMode,
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
            Some((new_messages, append_delivery_mode)),
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
        append: Option<(&[Message], DeliveryMode)>,
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
        // Fold an append into the same transaction as the freeze (ADR-0042 D7):
        // the new messages are inserted as pending, then the selection runs over
        // existing + appended, all committed atomically.
        if let Some((new_messages, append_delivery_mode)) = append {
            let now = crate::current_millis() / 1000;
            let start_position = pending.len() as u64 + 1;
            let mut seen = pending
                .iter()
                .map(|record| record.pending_id.clone())
                .collect::<HashSet<_>>();
            let mut appended_pending = Vec::with_capacity(new_messages.len());
            for (index, message) in new_messages.iter().cloned().enumerate() {
                let mut record = PendingMessageRecord::from_message(
                    thread_id.to_owned(),
                    start_position + index as u64,
                    message,
                    append_delivery_mode.clone(),
                );
                record.created_at = Some(now);
                record.updated_at = Some(now);
                validate_pending_message_record(&record)?;
                if !seen.insert(record.pending_id.clone()) {
                    return Err(duplicate_pending_id(&record.pending_id));
                }
                if self
                    .committed_message_exists_tx(&mut tx, thread_id, &record.pending_id)
                    .await?
                {
                    return Err(already_consumed(&record.pending_id));
                }
                appended_pending.push(record);
            }
            for record in &appended_pending {
                self.insert_pending_message_tx(&mut tx, record).await?;
            }
            pending.extend(appended_pending);
        }
        let selected_indexes = if let Some(run) = run {
            select_pending_for_freeze_for_run(&pending, boundary, Some(&run.run_id))
        } else {
            select_pending_for_freeze(&pending, boundary)
        };
        let selected_ids = selected_indexes
            .iter()
            .map(|index| pending[*index].pending_id.clone())
            .collect::<Vec<_>>();
        if let Some(expected_pending_ids) = expected_pending_ids
            && selected_ids != expected_pending_ids
        {
            return Err(StorageError::PendingSelectionConflict {
                expected_ids: expected_pending_ids.to_vec(),
                actual_ids: selected_ids,
            });
        }
        if selected_indexes.is_empty() {
            return Ok(Vec::new());
        }
        let committed_messages = committed
            .iter()
            .map(|record| record.message.clone())
            .collect::<Vec<_>>();
        let selected_messages = selected_indexes
            .iter()
            .map(|index| pending[*index].message.clone())
            .collect::<Vec<_>>();
        message_append::validate_append_only_delta(&committed_messages, &selected_messages)?;

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
