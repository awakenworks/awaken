//! The host adapter that backs the `send_message` builtin tool with the outbox.
//!
//! The extension owns the model-visible `send_message` tool over a neutral
//! [`MessageSender`] port (ADR-0007); the host injects this adapter. It resolves
//! the target Run's awaiting ticket and stages a durable cross-thread
//! delivery into the outbox, which the daemon relays to the target's pending
//! input (ADR-0017). Only a wait whose reason accepts ordinary input is bound to
//! the current Run. A tool approval, scheduled action, background wait, rate
//! limit, or idle thread receives *unbound* thread input instead; the next
//! ordinary Run consumes it (ADR-0021). Thus a chat message can never become an
//! authorization decision.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ToolAwaitReason};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_ext_builtin_tools::{MessageSendRequest, MessageSender};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::tool::ToolError;

use crate::RunDispatch;
use crate::dispatch::{ContinuationAdmission, DispatchQueue, Outbox, PendingInput};

/// Stages `send_message` deliveries into the durable outbox. Generic over the
/// store so any backend (memory/Postgres/SQLite) can back the tool. Messages are
/// addressed to a *thread* (the stable unit). Ordinary messages bind only to a
/// compatible input-accepting ticket and otherwise remain idle-Thread input;
/// Managed follow-ups bind to their separately admitted deterministic fresh Run.
pub struct OutboxMessageSender<S> {
    store: Arc<S>,
    reader: Arc<dyn CommittedThreadView>,
}

enum MessageBinding {
    AwaitingOrIdle,
    FreshRun(RunId),
}

impl<S: DispatchQueue + Outbox> OutboxMessageSender<S> {
    /// Build the adapter from the dispatch store (to resolve the target thread's
    /// awaiting run and stage) and the commit boundary's read port (for its ticket).
    pub fn new(store: Arc<S>, reader: Arc<dyn CommittedThreadView>) -> Self {
        Self { store, reader }
    }

    /// Persist a Managed Agent follow-up as input bound to its deterministic
    /// fresh Run. It deliberately never binds to an older Awaiting Run:
    /// confirmations and custom tool results have their own typed reply port,
    /// while a follow-up continues the Thread's history in a new ordinary Run.
    pub async fn send_fresh_continuation(
        &self,
        request: MessageSendRequest,
        continuation: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), ToolError> {
        let input = self
            .pending_input(
                request,
                MessageBinding::FreshRun(continuation.run_id().clone()),
            )
            .await?;
        self.store
            .relay_and_enqueue(input, continuation, admission)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    /// The one PendingInput construction and message-id owner shared by the
    /// ordinary model-visible message tool and Managed's fresh-Run follow-up
    /// adapter. Callers may choose binding policy, but may not duplicate ticket
    /// or idempotency semantics.
    async fn pending_input(
        &self,
        request: MessageSendRequest,
        binding: MessageBinding,
    ) -> Result<PendingInput, ToolError> {
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

        // Read the latest Run and its ticket from one committed Thread snapshot.
        // Dispatch is delivery state only and cannot decide whether a Thread is
        // awaiting. Otherwise preserve an unbound delivery for a fresh ordinary
        // Run (ADR-0021).
        Ok(match binding {
            MessageBinding::AwaitingOrIdle => match self.reader.open_wait_for_thread(&thread) {
                Some((_run_id, ticket))
                    if matches!(
                        ticket.target(),
                        AwaitTarget::RemoteInput { .. }
                            | AwaitTarget::Pause(PauseReason::Manual)
                            | AwaitTarget::ToolCall {
                                reason: ToolAwaitReason::Delegation,
                                ..
                            }
                    ) =>
                {
                    PendingInput {
                        message_id,
                        run_id: ticket.run_id,
                        thread_id: ticket.thread_id,
                        correlation_id: ticket.correlation_id,
                        available_at_ms: None,
                        context_messages: Vec::new(),
                        result: ResumeResult::Input(request.content),
                    }
                }
                _ => PendingInput {
                    message_id,
                    run_id: RunId(String::new()),
                    thread_id: thread,
                    correlation_id: String::new(),
                    available_at_ms: None,
                    context_messages: Vec::new(),
                    result: ResumeResult::Input(request.content),
                },
            },
            MessageBinding::FreshRun(run_id) => PendingInput {
                message_id,
                run_id,
                thread_id: thread,
                correlation_id: String::new(),
                available_at_ms: None,
                context_messages: Vec::new(),
                result: ResumeResult::Input(request.content),
            },
        })
    }
}

#[async_trait]
impl<S: DispatchQueue + Outbox + 'static> MessageSender for OutboxMessageSender<S> {
    async fn send(&self, request: MessageSendRequest) -> Result<(), ToolError> {
        let input = self
            .pending_input(request, MessageBinding::AwaitingOrIdle)
            .await?;
        let _inserted = self
            .store
            .stage(input)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?;
        Ok(())
    }
}
