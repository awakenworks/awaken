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

    /// List up to `limit` thread ids (ascending; `limit == 0` means unbounded)
    /// that currently hold at least one pending message, strictly greater than
    /// `after` (the previous page's last id) for cursor pagination. Startup
    /// recovery pages through this to detect threads whose consume opportunity
    /// may have been lost — pending was persisted but the dispatch/notification
    /// did not survive — without scanning the whole table at once (ADR-0042 D7).
    async fn list_threads_with_pending_messages(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<String>, StorageError>;

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
    ) -> Result<PendingMessageRecord, StorageError> {
        self.update_pending_message_record_checked(thread_id, pending_id, None, message)
            .await
    }

    async fn update_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
        message: Message,
    ) -> Result<PendingMessageRecord, StorageError>;

    async fn retract_pending_message_record(
        &self,
        thread_id: &str,
        pending_id: &str,
    ) -> Result<PendingMessageRecord, StorageError> {
        self.retract_pending_message_record_checked(thread_id, pending_id, None)
            .await
    }

    async fn retract_pending_message_record_checked(
        &self,
        thread_id: &str,
        pending_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<PendingMessageRecord, StorageError>;

    async fn reorder_pending_message_records(
        &self,
        thread_id: &str,
        ordered_pending_ids: &[String],
    ) -> Result<Vec<PendingMessageRecord>, StorageError> {
        self.reorder_pending_message_records_checked(thread_id, None, ordered_pending_ids)
            .await
    }

    async fn reorder_pending_message_records_checked(
        &self,
        thread_id: &str,
        expected_queue_revision: Option<u64>,
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

    /// Atomically append `new_messages` to pending and freeze the selected
    /// pending entries (existing + newly appended) with the run record, in one
    /// backend boundary (ADR-0042 D7).
    ///
    /// Closes the crash window a separate append-then-freeze leaves: a crash
    /// between them persists pending with no consume context. Here a crash
    /// either persists nothing (the client retry is the only request — no
    /// duplicate) or the complete frozen run (no orphan). `expected_pending_ids`
    /// is the caller's selection over `existing_pending ++ new_messages` (each
    /// appended entry takes `pending_id == message id`); it is CAS-checked, so a
    /// concurrent change since the caller's read aborts with
    /// `PendingSelectionConflict`.
    #[allow(clippy::too_many_arguments)]
    async fn append_and_freeze_pending_message_records_with_run(
        &self,
        thread_id: &str,
        new_messages: &[Message],
        append_delivery_mode: DeliveryMode,
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
