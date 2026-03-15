use async_trait::async_trait;

use super::{
    MailboxEntry, MailboxInterrupt, MailboxPage, MailboxQuery, MailboxState, MailboxStoreError,
};

#[async_trait]
pub trait MailboxReader: Send + Sync {
    async fn load_mailbox_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn load_mailbox_state(
        &self,
        mailbox_id: &str,
    ) -> Result<Option<MailboxState>, MailboxStoreError>;

    async fn list_mailbox_entries(
        &self,
        query: &MailboxQuery,
    ) -> Result<MailboxPage, MailboxStoreError>;
}

#[async_trait]
pub trait MailboxWriter: MailboxReader {
    async fn enqueue_mailbox_entry(&self, entry: &MailboxEntry) -> Result<(), MailboxStoreError>;

    async fn ensure_mailbox_state(
        &self,
        mailbox_id: &str,
        now: u64,
    ) -> Result<MailboxState, MailboxStoreError>;

    async fn claim_mailbox_entries(
        &self,
        mailbox_id: Option<&str>,
        limit: usize,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError>;

    async fn claim_mailbox_entry(
        &self,
        entry_id: &str,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn ack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError>;

    async fn nack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError>;

    async fn dead_letter_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError>;

    async fn cancel_mailbox_entry(
        &self,
        entry_id: &str,
        now: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn supersede_mailbox_entry(
        &self,
        entry_id: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn cancel_pending_for_mailbox(
        &self,
        mailbox_id: &str,
        now: u64,
        exclude_entry_id: Option<&str>,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError>;

    async fn interrupt_mailbox(
        &self,
        mailbox_id: &str,
        now: u64,
    ) -> Result<MailboxInterrupt, MailboxStoreError>;

    /// Extend the lease on a claimed entry.
    ///
    /// Returns `true` if the lease was extended, `false` if the claim token does
    /// not match or the entry is no longer in `Claimed` status.
    async fn extend_lease(
        &self,
        entry_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, MailboxStoreError>;

    /// Delete terminal entries older than `older_than` (unix millis). Returns count deleted.
    async fn purge_terminal_mailbox_entries(
        &self,
        older_than: u64,
    ) -> Result<usize, MailboxStoreError>;
}

pub trait MailboxStore: MailboxWriter {}

impl<T: MailboxWriter + ?Sized> MailboxStore for T {}
