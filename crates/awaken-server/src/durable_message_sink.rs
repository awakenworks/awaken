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
use awaken_runtime::RunActivation;
use awaken_runtime::extensions::background::{DurableMessageRequest, DurableMessageSink};
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::RunRequestOrigin;

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

        let message = Message::user(&request.message);
        let mut activation = RunActivation::new(request.recipient_thread_id, vec![message])
            .with_origin(RunRequestOrigin::Internal)
            // The sender-side message id is the dedup key for at-least-once
            // redelivery: a redelivered message reuses the same dispatch id.
            .with_dispatch_id_hint(request.message_id);
        if let Some(agent_id) = request.recipient_agent_id {
            activation = activation.with_agent_id(agent_id);
        }

        let result = mailbox
            .submit_background(activation)
            .await
            .map_err(|error| error.to_string())?;
        Ok(result.dispatch_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::MailboxConfig;
    use awaken_runtime::AgentRuntime;
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

    #[tokio::test]
    async fn maps_request_to_mailbox_dispatch() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            Arc::new(InMemoryMailboxStore::new()),
            Arc::new(InMemoryStore::new()),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        let sink = MailboxDurableMessageSink::new(&mailbox);

        let result = sink
            .send_agent_message(DurableMessageRequest {
                message_id: "m1".into(),
                recipient_thread_id: "thread-2".into(),
                recipient_agent_id: Some("reviewer".into()),
                sender_agent_id: "sender".into(),
                message: "hello".into(),
            })
            .await;

        assert!(result.is_ok(), "sink delivers via the mailbox: {result:?}");
        assert!(!result.unwrap().is_empty(), "returns a dispatch id");
    }

    #[tokio::test]
    async fn errors_when_mailbox_dropped() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
        let mailbox = Arc::new(Mailbox::new(
            runtime,
            Arc::new(InMemoryMailboxStore::new()),
            Arc::new(InMemoryStore::new()),
            "test".to_string(),
            MailboxConfig::default(),
        ));
        let sink = MailboxDurableMessageSink::new(&mailbox);
        drop(mailbox);

        let result = sink
            .send_agent_message(DurableMessageRequest {
                message_id: "m1".into(),
                recipient_thread_id: "t".into(),
                recipient_agent_id: None,
                sender_agent_id: "s".into(),
                message: "m".into(),
            })
            .await;
        assert!(
            result.is_err(),
            "a dropped mailbox surfaces an error, not a panic"
        );
    }
}
