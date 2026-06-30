//! The host adapter that backs the `send_message` builtin tool with the outbox.
//!
//! The extension owns the model-visible `send_message` tool over a neutral
//! [`MessageSender`] port (ADR-0007); the host injects this adapter. It resolves
//! the target run's parked waiting ticket and stages a durable cross-thread
//! delivery into the outbox, which the daemon relays to the target's pending
//! input (ADR-0017). Delivery to a run that is *not* waiting fails closed:
//! unsolicited delivery to an idle run needs new-input (not resume) semantics,
//! which is deferred.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_builtin_tools::MessageSender;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::tool::ToolError;

use crate::dispatch::{MessageOutbox, PendingInput, RunDispatch};

/// Stages `send_message` deliveries into the durable outbox. Generic over the
/// store so any backend (memory/Postgres/SQLite) can back the tool. Messages are
/// addressed to a *thread* (the stable unit); the adapter resolves the run
/// currently parked on it and the ticket that run is waiting on.
pub struct OutboxMessageSender<S> {
    store: Arc<S>,
    reader: Arc<dyn ThreadReader>,
    seq: AtomicU64,
}

impl<S: RunDispatch + MessageOutbox> OutboxMessageSender<S> {
    /// Build the adapter from the dispatch store (to resolve the target thread's
    /// parked run and stage) and the commit boundary's read port (for its ticket).
    pub fn new(store: Arc<S>, reader: Arc<dyn ThreadReader>) -> Self {
        Self {
            store,
            reader,
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl<S: RunDispatch + MessageOutbox + 'static> MessageSender for OutboxMessageSender<S> {
    async fn send(&self, target_thread: &str, content: &str) -> Result<(), ToolError> {
        let thread = ThreadId(target_thread.to_string());
        let run = self
            .store
            .parked_run(&thread)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "thread {target_thread} has no run waiting for input"
                ))
            })?;
        let ticket = self
            .reader
            .waiting_ticket(&run)
            .ok_or_else(|| ToolError::Execution(format!("run {} is no longer waiting", run.0)))?;

        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let input = PendingInput {
            message_id: format!("{target_thread}-msg-{n}"),
            run_id: ticket.run_id,
            thread_id: ticket.thread_id,
            correlation_id: ticket.correlation_id,
            available_at_ms: None,
            result: ResumeResult::Input(content.to_string()),
        };
        self.store
            .stage(input)
            .await
            .map_err(|err| ToolError::Execution(err.to_string()))?;
        Ok(())
    }
}
