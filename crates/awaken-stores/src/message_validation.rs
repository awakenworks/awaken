use awaken_server_contract::contract::message::{Message, MessageRecord};
use awaken_server_contract::contract::storage::{StorageError, message_append};

pub(crate) fn validate_committed_messages(messages: &[Message]) -> Result<(), StorageError> {
    message_append::validate_append_only_delta(&[], messages)
}

pub(crate) fn validate_committed_message_records(
    thread_id: &str,
    records: &[MessageRecord],
) -> Result<(), StorageError> {
    for (index, record) in records.iter().enumerate() {
        let expected_seq = index as u64 + 1;
        if record.thread_id != thread_id {
            return Err(StorageError::Validation(format!(
                "committed message '{}' belongs to thread '{}', expected '{}'",
                record.message_id, record.thread_id, thread_id
            )));
        }
        if record.seq != expected_seq {
            return Err(StorageError::Serialization(format!(
                "committed message seq must be continuous: expected {expected_seq}, got {}",
                record.seq
            )));
        }
        match record.message.id.as_deref() {
            Some(message_id) if message_id == record.message_id => {}
            Some(message_id) => {
                return Err(StorageError::Validation(format!(
                    "committed message record '{}' cannot carry message id '{}'",
                    record.message_id, message_id
                )));
            }
            None => {
                return Err(StorageError::Validation(format!(
                    "committed message record '{}' must carry message id",
                    record.message_id
                )));
            }
        }
    }
    validate_committed_messages(
        &records
            .iter()
            .map(|record| record.message.clone())
            .collect::<Vec<_>>(),
    )
}
