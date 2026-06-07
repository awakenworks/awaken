//! Host implementation of the runtime's `DurableMessageSink`: delivers durable
//! cross-thread `send_message` messages by enqueuing a background run on the
//! recipient thread via the [`Mailbox`].
//!
//! This is the only new type needed to wire `send_message` end-to-end — it
//! implements the existing `DurableMessageSink` trait and reuses the existing
//! `Mailbox::submit_background` path. It holds a `Weak<Mailbox>` to avoid the
//! `runtime → sink → mailbox → runtime` reference cycle (the mailbox runs on
//! the runtime as its dispatch executor).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use awaken_runtime::extensions::background::{DurableMessageRequest, DurableMessageSink};

use crate::mailbox::Mailbox;

/// Maps a [`DurableMessageRequest`] to a mailbox background dispatch.
pub struct MailboxDurableMessageSink {
    mailbox: Weak<Mailbox>,
}

impl MailboxDurableMessageSink {
    pub fn new(mailbox: &Arc<Mailbox>) -> Self {
        Self {
            mailbox: Arc::downgrade(mailbox),
        }
    }
}

#[async_trait]
impl DurableMessageSink for MailboxDurableMessageSink {
    async fn send_agent_message(&self, request: DurableMessageRequest) -> Result<String, String> {
        let mailbox = self
            .mailbox
            .upgrade()
            .ok_or_else(|| "mailbox has been dropped".to_string())?;

        // All the durable semantics — idempotent redelivery keyed on
        // `message_id`, id-addressed recipient message, sender attribution — live
        // in `Mailbox::submit_durable_message`, which owns the run store. This
        // sink is just the trait adapter.
        mailbox
            .submit_durable_message(request)
            .await
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::MailboxConfig;
    use awaken_runtime::AgentRuntime;
    use awaken_server_contract::contract::storage::ThreadStore;
    use awaken_stores::{InMemoryMailboxStore, InMemoryStore};

    struct StubResolver;
    impl awaken_runtime::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
            Err(awaken_runtime::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    /// Build a mailbox over an in-memory store, returning the store handle so a
    /// test can inspect the committed recipient log.
    fn mailbox_with_store() -> (Arc<Mailbox>, Arc<InMemoryStore>) {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let store = Arc::new(InMemoryStore::new());
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            Arc::new(InMemoryMailboxStore::new()),
            store.clone(),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        (mailbox, store)
    }

    fn request(message_id: &str) -> DurableMessageRequest {
        DurableMessageRequest {
            message_id: message_id.into(),
            recipient_thread_id: "thread-2".into(),
            recipient_agent_id: None,
            sender_agent_id: "sender-agent".into(),
            message: "hello".into(),
        }
    }

    #[tokio::test]
    async fn maps_request_to_mailbox_dispatch() {
        let (mailbox, _store) = mailbox_with_store();
        let sink = MailboxDurableMessageSink::new(&mailbox);

        let result = sink.send_agent_message(request("m1")).await;

        assert!(result.is_ok(), "sink delivers via the mailbox: {result:?}");
        assert!(!result.unwrap().is_empty(), "returns a dispatch id");
    }

    /// Issue #2 regression: the committed recipient message is id-addressed by
    /// the sender-side `message_id` and carries `sender_agent_id`, instead of
    /// being an anonymous user message with a fresh id.
    #[tokio::test]
    async fn committed_message_is_id_addressed_and_attributes_sender() {
        let (mailbox, store) = mailbox_with_store();
        let sink = MailboxDurableMessageSink::new(&mailbox);

        sink.send_agent_message(request("m1")).await.unwrap();

        let committed = store
            .load_committed_messages("thread-2")
            .await
            .unwrap()
            .unwrap_or_default();
        assert_eq!(committed.len(), 1, "exactly one recipient message");
        assert_eq!(
            committed[0].id.as_deref(),
            Some("m1"),
            "recipient message is id-addressed by the sender message_id"
        );
        assert_eq!(
            committed[0]
                .metadata
                .as_ref()
                .and_then(|m| m.sender_agent_id.as_deref()),
            Some("sender-agent"),
            "sender identity is preserved, not dropped"
        );
    }

    /// Issue #1 regression: redelivering the same `message_id` (at-least-once
    /// replay) returns the original dispatch id and does NOT append a duplicate
    /// recipient message.
    #[tokio::test]
    async fn redelivery_is_idempotent_no_duplicate_append() {
        let (mailbox, store) = mailbox_with_store();
        let sink = MailboxDurableMessageSink::new(&mailbox);

        let first = sink.send_agent_message(request("m1")).await.unwrap();
        let second = sink.send_agent_message(request("m1")).await.unwrap();

        assert_eq!(first, second, "redelivery reuses the same dispatch id");
        let committed = store
            .load_committed_messages("thread-2")
            .await
            .unwrap()
            .unwrap_or_default();
        assert_eq!(
            committed
                .iter()
                .filter(|m| m.id.as_deref() == Some("m1"))
                .count(),
            1,
            "redelivered message_id appended exactly once"
        );
    }

    #[tokio::test]
    async fn errors_when_mailbox_dropped() {
        let (mailbox, _store) = mailbox_with_store();
        let sink = MailboxDurableMessageSink::new(&mailbox);
        drop(mailbox);

        let result = sink.send_agent_message(request("m1")).await;
        assert!(
            result.is_err(),
            "a dropped mailbox surfaces an error, not a panic"
        );
    }
}
