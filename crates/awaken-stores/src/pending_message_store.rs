use async_trait::async_trait;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
};
use awaken_contract::contract::storage::StorageError;

/// Store-local extension for delivered-but-unconsumed thread messages.
#[async_trait]
pub trait PendingMessageStore: Send + Sync {
    async fn load_pending_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingMessageRecord>, StorageError>;

    async fn append_pending_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, StorageError>;

    async fn update_pending_message_record(
        &self,
        thread_id: &str,
        pending_id: &str,
        message: Message,
    ) -> Result<PendingMessageRecord, StorageError>;

    async fn retract_pending_message_record(
        &self,
        thread_id: &str,
        pending_id: &str,
    ) -> Result<PendingMessageRecord, StorageError>;

    async fn reorder_pending_message_records(
        &self,
        thread_id: &str,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError>;

    async fn freeze_pending_message_records(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, StorageError>;
}
