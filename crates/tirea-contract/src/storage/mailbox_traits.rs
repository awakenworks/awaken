use async_trait::async_trait;

use super::{MailboxEntry, MailboxPage, MailboxQuery, MailboxStoreError};

#[async_trait]
pub trait MailboxReader: Send + Sync {
    async fn load_mailbox_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn load_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn list_mailbox_entries(
        &self,
        query: &MailboxQuery,
    ) -> Result<MailboxPage, MailboxStoreError>;
}

#[async_trait]
pub trait MailboxWriter: MailboxReader {
    async fn enqueue_mailbox_entry(&self, entry: &MailboxEntry) -> Result<(), MailboxStoreError>;

    async fn claim_mailbox_entries(
        &self,
        limit: usize,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError>;

    async fn ack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        accepted_run_id: &str,
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

    async fn cancel_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
        now: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError>;

    async fn cancel_pending_mailbox_for_thread(
        &self,
        thread_id: &str,
        now: u64,
        exclude_run_id: Option<&str>,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError>;
}

pub trait MailboxStore: MailboxWriter {}

impl<T: MailboxWriter + ?Sized> MailboxStore for T {}
