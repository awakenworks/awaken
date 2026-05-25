use std::sync::Arc;

use awaken_contract::contract::message::{
    DeliveryBoundary, DeliveryMode, Message, MessageRecord, PendingMessageRecord,
};

use super::Mailbox;
use super::MailboxError;
use super::helpers::normalize_message_ids;

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

    pub async fn deliver(
        &self,
        thread_id: &str,
        messages: &[Message],
        delivery_mode: DeliveryMode,
    ) -> Result<Vec<PendingMessageRecord>, MailboxError> {
        let Some(store) = self.pending_message_store.as_ref() else {
            return Err(MailboxError::Internal(
                "pending message store is not configured".to_string(),
            ));
        };
        let normalized = normalize_message_ids(messages);
        Ok(store
            .append_pending_message_records(thread_id, &normalized, delivery_mode)
            .await?)
    }

    pub async fn freeze_pending(
        &self,
        thread_id: &str,
        boundary: DeliveryBoundary,
        expected_message_version: Option<u64>,
    ) -> Result<Vec<MessageRecord>, MailboxError> {
        let Some(store) = self.pending_message_store.as_ref() else {
            return Err(MailboxError::Internal(
                "pending message store is not configured".to_string(),
            ));
        };
        Ok(store
            .freeze_pending_message_records(thread_id, boundary, expected_message_version)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::event_sink::EventSink;
    use awaken_contract::contract::message::{DeliveryGranularity, Message};
    use awaken_contract::contract::suspension::ToolCallResume;
    use awaken_runtime::RunActivation;
    use awaken_runtime::loop_runner::{AgentLoopError, AgentRunResult};
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore, PendingMessageStore};

    use crate::mailbox::{MailboxConfig, RunDispatchExecutor};

    struct NoopExecutor;

    #[async_trait]
    impl RunDispatchExecutor for NoopExecutor {
        async fn run(
            &self,
            _activation: RunActivation,
            _sink: Arc<dyn EventSink>,
        ) -> Result<AgentRunResult, AgentLoopError> {
            unreachable!("deliver test does not execute runs")
        }

        fn cancel(&self, _id: &str) -> bool {
            false
        }

        async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
            false
        }

        fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn deliver_appends_normalized_messages_to_pending_store() {
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Mailbox::new(
            Arc::new(NoopExecutor),
            Arc::new(InMemoryMailboxStore::new()),
            thread_store.clone(),
            "consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_pending_message_store(thread_store.clone() as Arc<dyn PendingMessageStore>);

        let delivered = mailbox
            .deliver(
                "thread-deliver",
                &[Message::user("hello").with_id(String::new())],
                DeliveryMode::new_run(DeliveryGranularity::Batch),
            )
            .await
            .unwrap();

        assert_eq!(delivered.len(), 1);
        assert!(!delivered[0].pending_id.is_empty());
        assert_eq!(delivered[0].message.text(), "hello");
        let pending = thread_store
            .load_pending_message_records("thread-deliver")
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].pending_id, delivered[0].pending_id);
    }

    #[tokio::test]
    async fn freeze_pending_commits_delivered_messages() {
        let thread_store = Arc::new(InMemoryStore::new());
        let mailbox = Mailbox::new(
            Arc::new(NoopExecutor),
            Arc::new(InMemoryMailboxStore::new()),
            thread_store.clone(),
            "consumer".to_string(),
            MailboxConfig::default(),
        )
        .with_pending_message_store(thread_store.clone() as Arc<dyn PendingMessageStore>);

        mailbox
            .deliver(
                "thread-freeze",
                &[Message::user("queued")],
                DeliveryMode::new_run(DeliveryGranularity::Batch),
            )
            .await
            .unwrap();

        let frozen = mailbox
            .freeze_pending("thread-freeze", DeliveryBoundary::NewRun, Some(0))
            .await
            .unwrap();

        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].seq, 1);
        assert_eq!(frozen[0].message.text(), "queued");
        assert!(
            thread_store
                .load_pending_message_records("thread-freeze")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
