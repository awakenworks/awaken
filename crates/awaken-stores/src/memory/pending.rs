use std::collections::HashMap;
use std::collections::HashSet;

use async_trait::async_trait;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
    pending_queue_revision, select_pending_for_freeze, select_pending_for_freeze_for_run,
};
use awaken_contract::contract::storage::{
    RunRecord, StorageError, checkpoint_parent_thread_id, message_append,
};
use awaken_contract::thread::Thread;

use crate::PendingMessageStore;

use super::validate_thread_hierarchy_map;
use super::{InMemoryStore, current_millis};

fn normalize_pending_positions(pending: &mut [PendingMessageRecord]) {
    for (index, record) in pending.iter_mut().enumerate() {
        record.position = index as u64 + 1;
    }
}

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

fn selected_pending_ids(
    pending: &[PendingMessageRecord],
    selected_indexes: &[usize],
) -> Vec<String> {
    selected_indexes
        .iter()
        .map(|index| pending[*index].pending_id.clone())
        .collect()
}

impl InMemoryStore {
    async fn committed_message_exists(
        &self,
        thread_id: &str,
        message_id: &str,
    ) -> Result<bool, StorageError> {
        let guard = self.messages.read().await;
        Ok(guard.get(thread_id).is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message.id.as_deref() == Some(message_id))
        }))
    }
}

#[async_trait]
impl PendingMessageStore for InMemoryStore {
    async fn load_pending_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        let guard = self.pending_messages.read().await;
        Ok(guard.get(thread_id).cloned().unwrap_or_default())
    }

    async fn list_threads_with_pending_messages(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<String>, StorageError> {
        let guard = self.pending_messages.read().await;
        let mut ids: Vec<String> = guard
            .iter()
            .filter(|(_, records)| !records.is_empty())
            .map(|(thread_id, _)| thread_id.clone())
            .filter(|thread_id| after.map_or(true, |cursor| thread_id.as_str() > cursor))
            .collect();
        ids.sort();
        if limit > 0 {
            ids.truncate(limit);
        }
        Ok(ids)
    }

    async fn append_pending_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        let now = current_millis() / 1000;
        let committed_guard = self.messages.read().await;
        let mut guard = self.pending_messages.write().await;
        let pending = guard.entry(thread_id.to_owned()).or_default();
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
                return Err(duplicate_pending_id(&record.pending_id));
            }
            if committed_guard.get(thread_id).is_some_and(|committed| {
                committed
                    .iter()
                    .any(|message| message.id.as_deref() == Some(record.pending_id.as_str()))
            }) {
                return Err(already_consumed(&record.pending_id));
            }
        }
        pending.extend(records.iter().cloned());
        Ok(records)
    }

    async fn update_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
        mut message: Message,
    ) -> Result<PendingMessageRecord, StorageError> {
        let mut guard = self.pending_messages.write().await;
        if let Some(pending) = guard.get_mut(thread_id)
            && let Some(record) = pending
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
            return Ok(record.clone());
        }
        drop(guard);
        if self.committed_message_exists(thread_id, pending_id).await? {
            return Err(already_consumed(pending_id));
        }
        Err(pending_not_found(thread_id, pending_id))
    }

    async fn retract_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<PendingMessageRecord, StorageError> {
        let mut guard = self.pending_messages.write().await;
        if let Some(pending) = guard.get_mut(thread_id)
            && let Some(index) = pending
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
            normalize_pending_positions(pending);
            return Ok(removed);
        }
        drop(guard);
        if self.committed_message_exists(thread_id, pending_id).await? {
            return Err(already_consumed(pending_id));
        }
        Err(pending_not_found(thread_id, pending_id))
    }

    async fn reorder_pending_message_records_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        let mut guard = self.pending_messages.write().await;
        let Some(pending) = guard.get_mut(thread_id) else {
            drop(guard);
            for pending_id in ordered_pending_ids {
                if self.committed_message_exists(thread_id, pending_id).await? {
                    return Err(already_consumed(pending_id));
                }
            }
            return Err(StorageError::NotFound(thread_id.to_owned()));
        };
        let actual_queue_revision = pending_queue_revision(pending);
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
        let consumed_id = ordered_pending_ids
            .iter()
            .find(|pending_id| !pending_ids.contains(pending_id.as_str()))
            .cloned();
        if let Some(pending_id) = consumed_id {
            drop(guard);
            if self
                .committed_message_exists(thread_id, &pending_id)
                .await?
            {
                return Err(already_consumed(&pending_id));
            }
            return Err(StorageError::NotFound(pending_id));
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
        let now = current_millis() / 1000;
        normalize_pending_positions(&mut reordered);
        for record in &mut reordered {
            record.revision += 1;
            record.updated_at = Some(now);
        }
        *pending = reordered.clone();
        Ok(reordered)
    }

    async fn freeze_pending_message_records(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let mut messages_guard = self.messages.write().await;
        let mut pending_guard = self.pending_messages.write().await;
        let committed = messages_guard.entry(thread_id.to_owned()).or_default();
        let actual = committed.len() as u64;
        if let Some(expected) = expected_message_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let Some(pending) = pending_guard.get_mut(thread_id) else {
            return Ok(Vec::new());
        };
        let selected_indexes = select_pending_for_freeze(pending, boundary);
        if selected_indexes.is_empty() {
            return Ok(Vec::new());
        }
        let selected_messages = selected_indexes
            .iter()
            .map(|index| pending[*index].message.clone())
            .collect::<Vec<_>>();
        message_append::validate_append_only_delta(committed, &selected_messages)?;

        let mut selected = Vec::with_capacity(selected_indexes.len());
        for index in selected_indexes.iter().rev() {
            selected.push(pending.remove(*index));
        }
        selected.reverse();
        normalize_pending_positions(pending);
        let start_seq = committed.len() as u64 + 1;
        let appended = selected
            .into_iter()
            .enumerate()
            .map(|(index, record)| {
                let message = record.message;
                committed.push(message.clone());
                MessageRecord::from_message(thread_id.to_owned(), start_seq + index as u64, message)
            })
            .collect();
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
        let now = current_millis();
        let mut thread_guard = self.threads.write().await;
        let existing_thread = thread_guard.get(thread_id).cloned();
        validate_thread_hierarchy_map(
            &thread_guard,
            thread_id,
            checkpoint_parent_thread_id(existing_thread.as_ref(), run),
        )?;
        let mut messages_guard = self.messages.write().await;
        let mut pending_guard = self.pending_messages.write().await;
        let mut run_guard = self.runs.write().await;
        let actual = messages_guard
            .get(thread_id)
            .map(|messages| messages.len() as u64)
            .unwrap_or(0);
        if let Some(expected) = expected_message_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let pending = pending_guard.entry(thread_id.to_owned()).or_default();
        let selected_indexes =
            select_pending_for_freeze_for_run(pending, boundary, Some(&run.run_id));
        let selected_ids = selected_pending_ids(pending, &selected_indexes);
        if selected_ids != expected_pending_ids {
            return Err(StorageError::PendingSelectionConflict {
                expected_ids: expected_pending_ids.to_vec(),
                actual_ids: selected_ids,
            });
        }
        let committed = messages_guard.entry(thread_id.to_owned()).or_default();
        let selected_messages = selected_indexes
            .iter()
            .map(|index| pending[*index].message.clone())
            .collect::<Vec<_>>();
        message_append::validate_append_only_delta(committed, &selected_messages)?;

        let mut selected = Vec::with_capacity(selected_indexes.len());
        for index in selected_indexes.iter().rev() {
            selected.push(pending.remove(*index));
        }
        selected.reverse();
        normalize_pending_positions(pending);
        let start_seq = committed.len() as u64 + 1;
        let appended = selected
            .into_iter()
            .enumerate()
            .map(|(index, record)| {
                let message = record.message;
                committed.push(message.clone());
                MessageRecord::from_message(thread_id.to_owned(), start_seq + index as u64, message)
            })
            .collect::<Vec<_>>();
        let mut thread = existing_thread.unwrap_or_else(|| Thread::with_id(thread_id));
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();
        thread_guard.insert(thread_id.to_owned(), thread);
        run_guard.insert(run.run_id.clone(), run.clone());
        self.run_insertion
            .write()
            .await
            .insert(run.run_id.clone(), self.next_run_seq());
        Ok(appended)
    }
}
