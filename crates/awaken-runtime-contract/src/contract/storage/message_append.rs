use std::collections::HashSet;

use super::StorageError;
use crate::contract::message::{CompactionMark, Message, Role};

/// Validate that a checkpoint append delta contains only new message ids.
///
/// Re-submitting an already committed id would be an update/upsert attempt, not
/// an append. Reject it before merging so stale projection writers cannot hide
/// behind a silent no-op.
pub fn validate_append_only_delta(
    existing: &[Message],
    delta: &[Message],
) -> Result<(), StorageError> {
    let existing_ids: HashSet<&str> = existing
        .iter()
        .filter_map(|message| message.id.as_deref())
        .collect();
    let mut delta_ids = HashSet::new();
    for (index, message) in delta.iter().enumerate() {
        let summary_seq = existing.len() as u64 + index as u64 + 1;
        validate_message_for_append(message, summary_seq)?;
        let message_id = message
            .id
            .as_deref()
            .expect("validate_message_for_append requires message id");
        if existing_ids.contains(message_id) {
            return Err(StorageError::Validation(format!(
                "append delta contains already committed message id '{message_id}'"
            )));
        }
        if !delta_ids.insert(message_id) {
            return Err(StorageError::Validation(format!(
                "append delta contains duplicate message id '{message_id}'"
            )));
        }
    }
    Ok(())
}

fn validate_message_for_append(message: &Message, summary_seq: u64) -> Result<(), StorageError> {
    validate_message_shape(message, "append delta")?;
    let message_id = message
        .id
        .as_deref()
        .expect("validate_message_shape requires message id");

    if let Some(mark) = message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.compaction)
    {
        validate_compaction_mark(message_id, mark, summary_seq)?;
    }

    Ok(())
}

pub fn validate_message_shape(message: &Message, context: &str) -> Result<(), StorageError> {
    let Some(message_id) = message.id.as_deref() else {
        return Err(StorageError::Validation(format!(
            "{context} message id must not be missing"
        )));
    };
    if message_id.trim().is_empty() {
        return Err(StorageError::Validation(format!(
            "{context} message id must not be empty"
        )));
    }

    match message.role {
        Role::Tool => {
            let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                return Err(StorageError::Validation(format!(
                    "tool message '{message_id}' must carry tool_call_id"
                )));
            };
            if tool_call_id.trim().is_empty() {
                return Err(StorageError::Validation(format!(
                    "tool message '{message_id}' tool_call_id must not be empty"
                )));
            }
        }
        _ => {
            if message.tool_call_id.is_some() {
                return Err(StorageError::Validation(format!(
                    "non-tool message '{message_id}' must not carry tool_call_id"
                )));
            }
        }
    }

    if message.role != Role::Assistant && message.tool_calls.is_some() {
        return Err(StorageError::Validation(format!(
            "non-assistant message '{message_id}' must not carry tool_calls"
        )));
    }

    let Some(tool_calls) = message.tool_calls.as_ref() else {
        return Ok(());
    };

    let mut ids = HashSet::new();
    for call in tool_calls {
        if call.id.trim().is_empty() {
            return Err(StorageError::Validation(format!(
                "assistant message '{message_id}' tool call id must not be empty"
            )));
        }
        if call.name.trim().is_empty() {
            return Err(StorageError::Validation(format!(
                "assistant message '{message_id}' tool call name must not be empty"
            )));
        }
        if !ids.insert(call.id.as_str()) {
            return Err(StorageError::Validation(format!(
                "assistant message '{message_id}' contains duplicate tool call id '{}'",
                call.id
            )));
        }
    }

    Ok(())
}

fn validate_compaction_mark(
    message_id: &str,
    mark: CompactionMark,
    summary_seq: u64,
) -> Result<(), StorageError> {
    if mark.from_seq == 0 || mark.from_seq > mark.to_seq {
        return Err(StorageError::Validation(format!(
            "compaction mark on message '{message_id}' must be 1-based and non-empty"
        )));
    }
    if mark.to_seq >= summary_seq {
        return Err(StorageError::Validation(format!(
            "compaction mark on summary message '{message_id}' must not cover the summary seq {summary_seq}"
        )));
    }
    Ok(())
}

/// Merge an append checkpoint delta into the committed message projection.
///
/// Committed history is append-only (ADR-0042 I1/D6): only message ids not
/// already committed are appended, at the tail. A delta entry whose id is
/// already committed is an error — committed messages are never rewritten in
/// place, so the committed-count version guard alone is multi-instance safe and
/// concurrent writers can never silently last-writer-wins an existing message.
///
/// Read-view changes (e.g. hiding superseded suspended tool calls) are applied
/// at read time by `strip_unpaired_tool_calls_*`, driven by appended `Internal`
/// retraction markers, never by mutating the committed log.
pub fn merge_checkpoint_append_messages(
    existing: &mut Vec<Message>,
    delta: &[Message],
) -> Result<(), StorageError> {
    validate_append_only_delta(existing, delta)?;
    existing.extend(delta.iter().cloned());
    Ok(())
}
