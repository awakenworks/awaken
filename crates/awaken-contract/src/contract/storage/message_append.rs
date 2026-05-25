use super::Message;

/// Merge an append checkpoint delta into the committed message projection.
///
/// New message ids append at the tail. A delta message whose id is already in
/// the committed projection replaces that visible message in place; this lets
/// checkpoint cleanup persist read-view changes such as removing superseded
/// suspended tool calls without restoring whole-list last-writer-wins behavior.
pub fn merge_checkpoint_append_messages(existing: &mut Vec<Message>, delta: &[Message]) {
    for message in delta {
        if let Some(message_id) = message.id.as_deref()
            && let Some(existing_message) = existing
                .iter_mut()
                .find(|existing| existing.id.as_deref() == Some(message_id))
        {
            *existing_message = message.clone();
            continue;
        }
        existing.push(message.clone());
    }
}
