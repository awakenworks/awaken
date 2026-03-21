//! Agent runtime: top-level orchestrator for run management, routing, and control.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, mpsc};

use crate::contract::inference::InferenceOverride;
use crate::contract::message::Message;
use crate::contract::storage::{RunStore, ThreadStore};
use crate::contract::suspension::ToolCallResume;
use crate::error::StateError;

use super::cancellation::CancellationToken;
use super::resolver::AgentResolver;

// ---------------------------------------------------------------------------
// RunRequest
// ---------------------------------------------------------------------------

/// Unified request for starting or resuming a run.
pub struct RunRequest {
    /// Target agent ID. `None` = use default or infer from thread state.
    pub agent_id: Option<String>,
    /// Thread ID. Existing → load history; new → create.
    pub thread_id: String,
    /// New messages to append before running.
    pub messages: Vec<Message>,
    /// Runtime parameter overrides for this run.
    pub overrides: Option<InferenceOverride>,
    /// Resume decisions for suspended tool calls. Empty = fresh run.
    pub decisions: Vec<(String, ToolCallResume)>,
}

// ---------------------------------------------------------------------------
// RunHandle
// ---------------------------------------------------------------------------

/// External control handle for a running agent loop.
///
/// Returned by `AgentRuntime`. Enables cancellation and
/// live decision injection.
#[derive(Clone)]
pub struct RunHandle {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    cancellation_token: CancellationToken,
    decision_tx: mpsc::Sender<(String, ToolCallResume)>,
}

impl RunHandle {
    /// Cancel the running agent loop cooperatively.
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Send a tool call decision to the running loop.
    pub fn send_decision(
        &self,
        call_id: String,
        resume: ToolCallResume,
    ) -> Result<(), mpsc::SendError<(String, ToolCallResume)>> {
        self.decision_tx.send((call_id, resume))
    }
}

// ---------------------------------------------------------------------------
// ActiveRunRegistry
// ---------------------------------------------------------------------------

struct RunEntry {
    #[allow(dead_code)]
    run_id: String,
    #[allow(dead_code)]
    agent_id: String,
    handle: RunHandle,
}

/// Tracks active runs. At most one active run per thread.
pub(crate) struct ActiveRunRegistry {
    by_thread_id: RwLock<HashMap<String, RunEntry>>,
}

impl ActiveRunRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_thread_id: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert(&self, thread_id: String, entry: RunEntry) {
        self.by_thread_id
            .write()
            .expect("active runs lock poisoned")
            .insert(thread_id, entry);
    }

    pub(crate) fn remove(&self, thread_id: &str) {
        self.by_thread_id
            .write()
            .expect("active runs lock poisoned")
            .remove(thread_id);
    }

    pub(crate) fn get_handle(&self, thread_id: &str) -> Option<RunHandle> {
        self.by_thread_id
            .read()
            .expect("active runs lock poisoned")
            .get(thread_id)
            .map(|e| e.handle.clone())
    }

    pub(crate) fn has_active_run(&self, thread_id: &str) -> bool {
        self.by_thread_id
            .read()
            .expect("active runs lock poisoned")
            .contains_key(thread_id)
    }
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

/// Top-level agent runtime. Manages runs across threads.
///
/// Provides methods for cancelling and sending decisions
/// to active agent runs. Enforces one active run per thread.
pub struct AgentRuntime {
    resolver: Arc<dyn AgentResolver>,
    thread_store: Option<Arc<dyn ThreadStore>>,
    run_store: Option<Arc<dyn RunStore>>,
    active_runs: ActiveRunRegistry,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self {
            resolver,
            thread_store: None,
            run_store: None,
            active_runs: ActiveRunRegistry::new(),
        }
    }

    #[must_use]
    pub fn with_thread_store(mut self, store: Arc<dyn ThreadStore>) -> Self {
        self.thread_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_run_store(mut self, store: Arc<dyn RunStore>) -> Self {
        self.run_store = Some(store);
        self
    }

    pub fn resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    pub fn thread_store(&self) -> Option<&dyn ThreadStore> {
        self.thread_store.as_deref()
    }

    /// Cancel an active run by thread ID.
    pub fn cancel_by_thread(&self, thread_id: &str) -> bool {
        if let Some(handle) = self.active_runs.get_handle(thread_id) {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Send decisions to an active run by thread ID.
    pub fn send_decisions(
        &self,
        thread_id: &str,
        decisions: Vec<(String, ToolCallResume)>,
    ) -> bool {
        if let Some(handle) = self.active_runs.get_handle(thread_id) {
            for (call_id, resume) in decisions {
                if handle.send_decision(call_id, resume).is_err() {
                    return false;
                }
            }
            true
        } else {
            false
        }
    }

    /// Create a run handle pair (handle + internal channels).
    ///
    /// Returns (RunHandle for caller, CancellationToken for loop, decision_rx for loop).
    pub(crate) fn create_run_channels(
        &self,
        run_id: String,
        thread_id: String,
        agent_id: String,
    ) -> (
        RunHandle,
        CancellationToken,
        mpsc::Receiver<(String, ToolCallResume)>,
    ) {
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::channel();

        let handle = RunHandle {
            run_id,
            thread_id,
            agent_id,
            cancellation_token: token.clone(),
            decision_tx: tx,
        };

        (handle, token, rx)
    }

    /// Register an active run. Returns error if thread already has one.
    pub(crate) fn register_run(
        &self,
        thread_id: &str,
        handle: RunHandle,
    ) -> Result<(), StateError> {
        if self.active_runs.has_active_run(thread_id) {
            return Err(StateError::ThreadAlreadyRunning {
                thread_id: thread_id.to_string(),
            });
        }
        self.active_runs.insert(
            thread_id.to_string(),
            RunEntry {
                run_id: handle.run_id.clone(),
                agent_id: handle.agent_id.clone(),
                handle,
            },
        );
        Ok(())
    }

    /// Unregister an active run when it completes.
    pub(crate) fn unregister_run(&self, thread_id: &str) {
        self.active_runs.remove(thread_id);
    }
}
