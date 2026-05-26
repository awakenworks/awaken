use std::collections::HashSet;

use super::{Message, StorageError};

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
    for message in delta {
        let Some(message_id) = message.id.as_deref() else {
            continue;
        };
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
