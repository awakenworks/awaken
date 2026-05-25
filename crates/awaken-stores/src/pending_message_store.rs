use async_trait::async_trait;
use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
};
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};

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

    async fn freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
        expected_pending_ids: &[String],
        run: &RunRecord,
    ) -> Result<Vec<MessageRecord>, StorageError>;
}

/// Thread/run store that owns the pending partition for the same backend.
///
/// ADR-0042 freeze operations consume pending messages and write committed
/// messages plus the run record in one backend boundary, so mailbox wiring
/// should depend on this combined capability instead of a separate pending
/// store handle.
pub trait PendingThreadRunStore: ThreadRunStore + PendingMessageStore {}

impl<T> PendingThreadRunStore for T where T: ThreadRunStore + PendingMessageStore {}
