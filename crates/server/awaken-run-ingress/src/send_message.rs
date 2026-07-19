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
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::MessageSender;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::tool::ToolError;

use crate::dispatch::{DispatchQueue, Outbox, PendingInput};

/// Stages `send_message` deliveries into the durable outbox. Generic over the
/// store so any backend (memory/Postgres/SQLite) can back the tool. Messages are
/// addressed to a *thread* (the stable unit); the adapter binds only to a
/// compatible input-accepting ticket and otherwise preserves it for the next Run.
pub struct OutboxMessageSender<S> {
    store: Arc<S>,
    reader: Arc<dyn ThreadReader>,
    seq: AtomicU64,
}

impl<S: DispatchQueue + Outbox> OutboxMessageSender<S> {
    /// Build the adapter from the dispatch store (to resolve the target thread's
    /// awaiting run and stage) and the commit boundary's read port (for its ticket).
    pub fn new(store: Arc<S>, reader: Arc<dyn ThreadReader>) -> Self {
        Self {
            store,
            reader,
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl<S: DispatchQueue + Outbox + 'static> MessageSender for OutboxMessageSender<S> {
    async fn send(&self, target_thread: &str, content: &str) -> Result<(), ToolError> {
        let thread = ThreadId(target_thread.to_string());
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let message_id = format!("{target_thread}-msg-{n}");

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
                    ticket.reason,
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
                    result: ResumeResult::Input(content.to_string()),
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
                result: ResumeResult::Input(content.to_string()),
            },
        };
        self.store
            .stage(input)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?;
        Ok(())
    }
}
