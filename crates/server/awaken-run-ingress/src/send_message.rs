//! The host adapter that backs the `send_message` builtin tool with the outbox.
//!
//! The extension owns the model-visible `send_message` tool over a neutral
//! [`MessageSender`] port (ADR-0007); the host injects this adapter. It resolves
//! the target run's awaiting awaiting ticket and stages a durable cross-thread
//! delivery into the outbox, which the daemon relays to the target's pending
//! input (ADR-0017). Only a wait whose reason accepts ordinary input is bound to
//! the current Run. A tool approval, scheduled action, background wait, rate
//! limit, or idle thread receives *unbound* thread input instead; the next
//! ordinary turn consumes it (ADR-0021). Thus a chat message can never become an
//! authorization decision.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_builtin_tools::{MessageSendRequest, MessageSender};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::tool::ToolError;

use crate::dispatch::{DispatchQueue, Outbox, PendingInput};

/// Stages `send_message` deliveries into the durable outbox. Generic over the
/// store so any backend (memory/Postgres/SQLite) can back the tool. Messages are
/// addressed to a *thread* (the stable unit); the adapter binds only to a
/// compatible input-accepting ticket and otherwise preserves it for the next Run.
pub struct OutboxMessageSender<S> {
    store: Arc<S>,
    reader: Arc<dyn CommittedThreadView>,
}

impl<S: DispatchQueue + Outbox> OutboxMessageSender<S> {
    /// Build the adapter from the dispatch store (to resolve the target thread's
    /// awaiting run and stage) and the commit boundary's read port (for its ticket).
    pub fn new(store: Arc<S>, reader: Arc<dyn CommittedThreadView>) -> Self {
        Self { store, reader }
    }
}

#[async_trait]
impl<S: DispatchQueue + Outbox + 'static> MessageSender for OutboxMessageSender<S> {
    async fn send(&self, request: MessageSendRequest) -> Result<(), ToolError> {
        let thread = ThreadId(request.target_thread.clone());
        // CE-SM4..SM7: the optional caller key is scoped by the sending Run; when
        // absent, the runtime-owned operation id is the stable retry identity.
        // Payload is deliberately excluded so reuse with changed intent reaches
        // the Outbox's canonical idempotency-conflict check instead of becoming a
        // second message.
        let identity = match request.idempotency_key.as_deref() {
            Some(key) => ("caller-key", request.source_run_id.as_str(), key),
            None => (
                "operation",
                request.source_run_id.as_str(),
                request.operation_id.as_str(),
            ),
        };
        let message_id = format!(
            "send-message-{}",
            awaken_agent_contract::stable_fingerprint(&identity)
        );

        // Bind to the run awaiting on the thread if there is one; otherwise stage
        // an unbound delivery (empty run/correlation) the thread's next run
        // consumes as new input (ADR-0021).
        let input = match self
            .store
            .awaiting_run(&thread)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?
            .and_then(|run| self.reader.resume_ticket(&run))
        {
            Some(ticket)
                if matches!(
                    ticket.reason(),
                    AwaitReason::UserInput
                        | AwaitReason::ExternalEvent
                        | AwaitReason::ManualPause
                        | AwaitReason::Delegation
                ) =>
            {
                PendingInput {
                    message_id,
                    run_id: ticket.run_id,
                    thread_id: ticket.thread_id,
                    correlation_id: ticket.correlation_id,
                    available_at_ms: None,
                    result: ResumeResult::Input(request.content.clone()),
                }
            }
            // An ordinary agent message never approves a protected tool, performs
            // a scheduled action, completes background work, or bypasses a rate
            // limit. Keep it as unbound thread input for the next ordinary turn.
            _ => PendingInput {
                message_id,
                run_id: RunId(String::new()),
                thread_id: thread,
                correlation_id: String::new(),
                available_at_ms: None,
                result: ResumeResult::Input(request.content),
            },
        };
        let _inserted = self
            .store
            .stage(input)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?;
        Ok(())
    }
}
