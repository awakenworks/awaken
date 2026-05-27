use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
    pending_queue_revision, select_pending_for_freeze, select_pending_for_freeze_for_run,
};
use awaken_contract::contract::storage::{
    RunRecord, StorageError, ThreadStore, checkpoint_parent_thread_id,
};
use awaken_contract::thread::Thread;

use crate::PendingMessageStore;

use super::{
    FileStore, StagedFileOp, cleanup_staged_file_ops, commit_staged_file_ops, commit_staged_writes,
    current_millis, stage_write, validate_id,
};

#[async_trait]
impl PendingMessageStore for FileStore {
    async fn load_pending_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        validate_id(thread_id, "thread id")?;
        self.read_pending_messages_locked(thread_id).await
    }

    async fn append_pending_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let mut pending = self.read_pending_messages_locked(thread_id).await?;
        let now = current_millis() / 1000;
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
            if !seen.insert(record.pending_id.as_str()) {
                return Err(Self::duplicate_pending_id(&record.pending_id));
            }
            if self
                .committed_message_exists(thread_id, &record.pending_id)
                .await?
            {
                return Err(Self::already_consumed(&record.pending_id));
            }
        }
        pending.extend(records.iter().cloned());
        let write = self
            .write_pending_messages_locked(thread_id, &pending)
            .await?;
        commit_staged_writes(&self.base_path, &[write]).await?;
        Ok(records)
    }

    async fn update_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
        mut message: Message,
    ) -> Result<PendingMessageRecord, StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let mut pending = self.read_pending_messages_locked(thread_id).await?;
        if let Some(record) = pending
            .iter_mut()
            .find(|record| record.pending_id == pending_id)
        {
            if let Some(expected) = expected_revision
                && record.revision != expected
            {
                return Err(StorageError::VersionConflict {
                    expected,
                    actual: record.revision,
                });
            }
            match message.id.as_deref() {
                Some(message_id) if message_id != pending_id => {
                    return Err(StorageError::Validation(format!(
                        "pending message '{pending_id}' cannot change message id to '{message_id}'"
                    )));
                }
                Some(_) => {}
                None => message.id = Some(pending_id.to_owned()),
            }
            record.message = message;
            record.revision += 1;
            record.updated_at = Some(current_millis() / 1000);
            let updated = record.clone();
            let write = self
                .write_pending_messages_locked(thread_id, &pending)
                .await?;
            commit_staged_writes(&self.base_path, &[write]).await?;
            return Ok(updated);
        }
        if self.committed_message_exists(thread_id, pending_id).await? {
            return Err(Self::already_consumed(pending_id));
        }
        Err(Self::pending_not_found(thread_id, pending_id))
    }

    async fn retract_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<PendingMessageRecord, StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let mut pending = self.read_pending_messages_locked(thread_id).await?;
        if let Some(index) = pending
            .iter()
            .position(|record| record.pending_id == pending_id)
        {
            if let Some(expected) = expected_revision
                && pending[index].revision != expected
            {
                return Err(StorageError::VersionConflict {
                    expected,
                    actual: pending[index].revision,
                });
            }
            let removed = pending.remove(index);
            Self::normalize_pending_positions(&mut pending);
            let write = self
                .write_pending_messages_locked(thread_id, &pending)
                .await?;
            commit_staged_writes(&self.base_path, &[write]).await?;
            return Ok(removed);
        }
        if self.committed_message_exists(thread_id, pending_id).await? {
            return Err(Self::already_consumed(pending_id));
        }
        Err(Self::pending_not_found(thread_id, pending_id))
    }

    async fn reorder_pending_message_records_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let pending = self.read_pending_messages_locked(thread_id).await?;
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
                && self.committed_message_exists(thread_id, pending_id).await?
            {
                return Err(Self::already_consumed(pending_id));
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
            .collect::<HashMap<_, _>>();
        let mut reordered = Vec::with_capacity(ordered_pending_ids.len());
        for pending_id in ordered_pending_ids {
            let Some(record) = by_id.remove(pending_id) else {
                if self.committed_message_exists(thread_id, pending_id).await? {
                    return Err(Self::already_consumed(pending_id));
                }
                return Err(StorageError::NotFound(pending_id.clone()));
            };
            reordered.push(record);
        }
        if !by_id.is_empty() {
            return Err(StorageError::Validation(format!(
                "reorder for thread '{thread_id}' omitted pending ids"
            )));
        }
        let now = current_millis() / 1000;
        Self::normalize_pending_positions(&mut reordered);
        for record in &mut reordered {
            record.revision += 1;
            record.updated_at = Some(now);
        }
        let write = self
            .write_pending_messages_locked(thread_id, &reordered)
            .await?;
        commit_staged_writes(&self.base_path, &[write]).await?;
        Ok(reordered)
    }

    async fn freeze_pending_message_records(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        validate_id(thread_id, "thread id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let committed = self
            .load_committed_message_records_locked(thread_id)
            .await?
            .unwrap_or_default();
        let actual = committed.len() as u64;
        if let Some(expected) = expected_message_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut pending = self.read_pending_messages_locked(thread_id).await?;
        let selected_indexes = select_pending_for_freeze(&pending, boundary);
        if selected_indexes.is_empty() {
            return Ok(Vec::new());
        }
        let mut selected = Vec::with_capacity(selected_indexes.len());
        for index in selected_indexes.iter().rev() {
            selected.push(pending.remove(*index));
        }
        selected.reverse();
        Self::normalize_pending_positions(&mut pending);
        let selected_messages = selected
            .into_iter()
            .map(|record| record.message)
            .collect::<Vec<_>>();
        awaken_contract::contract::storage::message_append::validate_append_only_delta(
            &committed
                .iter()
                .map(|record| record.message.clone())
                .collect::<Vec<_>>(),
            &selected_messages,
        )?;
        let mut ops = Vec::new();
        let appended = match self
            .stage_append_message_records(thread_id, actual + 1, selected_messages, &mut ops)
            .await
        {
            Ok(appended) => appended,
            Err(error) => {
                cleanup_staged_file_ops(&ops).await;
                return Err(error);
            }
        };
        let pending_write = self
            .write_pending_messages_locked(thread_id, &pending)
            .await?;
        ops.push(StagedFileOp::Write(pending_write));
        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }
        Ok(appended)
    }

    async fn freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: &[String],
        run: &RunRecord,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        validate_id(thread_id, "thread id")?;
        validate_id(&run.run_id, "run id")?;
        let _guard = self.hierarchy_lock.lock().await;
        let committed = self
            .load_committed_message_records_locked(thread_id)
            .await?
            .unwrap_or_default();
        let actual = committed.len() as u64;
        if let Some(expected) = expected_message_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut pending = self.read_pending_messages_locked(thread_id).await?;
        let selected_indexes =
            select_pending_for_freeze_for_run(&pending, boundary, Some(&run.run_id));
        let selected_ids = selected_indexes
            .iter()
            .map(|index| pending[*index].pending_id.clone())
            .collect::<Vec<_>>();
        if selected_ids != expected_pending_ids {
            return Err(StorageError::PendingSelectionConflict {
                expected_ids: expected_pending_ids.to_vec(),
                actual_ids: selected_ids,
            });
        }
        let now = current_millis();
        let mut thread = self
            .load_thread(thread_id)
            .await?
            .unwrap_or_else(|| Thread::with_id(thread_id));
        self.validate_thread_hierarchy(thread_id, checkpoint_parent_thread_id(Some(&thread), run))
            .await?;
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();

        let mut selected = Vec::with_capacity(selected_indexes.len());
        for index in selected_indexes.iter().rev() {
            selected.push(pending.remove(*index));
        }
        selected.reverse();
        Self::normalize_pending_positions(&mut pending);
        let selected_messages = selected
            .into_iter()
            .map(|record| record.message)
            .collect::<Vec<_>>();
        awaken_contract::contract::storage::message_append::validate_append_only_delta(
            &committed
                .iter()
                .map(|record| record.message.clone())
                .collect::<Vec<_>>(),
            &selected_messages,
        )?;

        let thread_payload = serde_json::to_string_pretty(&thread)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let run_payload = serde_json::to_string_pretty(run)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let thread_write = stage_write(
            &self.threads_dir(),
            &format!("{thread_id}.json"),
            &thread_payload,
        )
        .await?;
        let mut ops = vec![StagedFileOp::Write(thread_write)];
        let appended = match self
            .stage_append_message_records(thread_id, actual + 1, selected_messages, &mut ops)
            .await
        {
            Ok(appended) => appended,
            Err(error) => {
                cleanup_staged_file_ops(&ops).await;
                return Err(error);
            }
        };
        let pending_write = self
            .write_pending_messages_locked(thread_id, &pending)
            .await?;
        let run_write = stage_write(
            &self.runs_dir(),
            &format!("{}.json", run.run_id),
            &run_payload,
        )
        .await?;
        ops.push(StagedFileOp::Write(pending_write));
        ops.push(StagedFileOp::Write(run_write));
        if let Err(error) = commit_staged_file_ops(&self.base_path, &ops).await {
            cleanup_staged_file_ops(&ops).await;
            return Err(error);
        }
        Ok(appended)
    }
}
