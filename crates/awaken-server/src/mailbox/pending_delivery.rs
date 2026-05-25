use std::sync::Arc;

use super::Mailbox;

impl Mailbox {
    /// Attach the durable pending-message store used by ADR-0042 delivery.
    #[must_use]
    pub fn with_pending_message_store(
        mut self,
        store: Arc<dyn awaken_stores::PendingMessageStore>,
    ) -> Self {
        self.pending_message_store = Some(store);
        self
    }

    #[must_use]
    pub fn pending_message_store(&self) -> Option<&Arc<dyn awaken_stores::PendingMessageStore>> {
        self.pending_message_store.as_ref()
    }
}
