//! Agent runtime: top-level orchestrator for run management, routing, and control.

mod active_registry;
mod control;
mod run_request;
mod runner;

use std::sync::Arc;

use awaken_contract::contract::storage::ThreadRunStore;

use crate::error::RuntimeError;
use crate::registry::composite::CompositeAgentSpecRegistry;
use awaken_contract::contract::suspension::ToolCallResume;
use futures::channel::mpsc;

use crate::runtime::cancellation::CancellationToken;
use crate::runtime::resolver::AgentResolver;

pub use run_request::{RunInput, RunOptions, RunRequest};

use active_registry::ActiveRunRegistry;

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
    decision_tx: mpsc::UnboundedSender<(String, ToolCallResume)>,
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
    ) -> Result<(), Box<mpsc::TrySendError<(String, ToolCallResume)>>> {
        self.decision_tx
            .unbounded_send((call_id, resume))
            .map_err(Box::new)
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
    pub(crate) resolver: Arc<dyn AgentResolver>,
    pub(crate) storage: Option<Arc<dyn ThreadRunStore>>,
    pub(crate) active_runs: ActiveRunRegistry,
    composite_registry: Option<Arc<CompositeAgentSpecRegistry>>,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self {
            resolver,
            storage: None,
            active_runs: ActiveRunRegistry::new(),
            composite_registry: None,
        }
    }

    #[must_use]
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.storage = Some(store);
        self
    }

    pub fn resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    /// Return a cloned `Arc` of the agent resolver.
    pub fn resolver_arc(&self) -> Arc<dyn AgentResolver> {
        Arc::clone(&self.resolver)
    }

    #[must_use]
    pub fn with_composite_registry(mut self, registry: Arc<CompositeAgentSpecRegistry>) -> Self {
        self.composite_registry = Some(registry);
        self
    }

    /// Return the composite registry, if one was configured.
    pub fn composite_registry(&self) -> Option<&Arc<CompositeAgentSpecRegistry>> {
        self.composite_registry.as_ref()
    }

    /// Initialize the runtime — discover remote agents.
    /// Call this after `build()` to complete async initialization.
    pub async fn initialize(&self) -> Result<(), RuntimeError> {
        if let Some(composite) = &self.composite_registry {
            composite
                .discover()
                .await
                .map_err(|e| RuntimeError::ResolveFailed {
                    message: format!("remote agent discovery failed: {e}"),
                })?;
        }
        Ok(())
    }

    pub fn thread_run_store(&self) -> Option<&dyn ThreadRunStore> {
        self.storage.as_deref()
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
        mpsc::UnboundedReceiver<(String, ToolCallResume)>,
    ) {
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded();

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
    ///
    /// Uses atomic try-insert to avoid TOCTOU race between check and insert.
    pub(crate) fn register_run(
        &self,
        thread_id: &str,
        handle: RunHandle,
    ) -> Result<(), RuntimeError> {
        let run_id = handle.run_id.clone();
        if !self.active_runs.register(&run_id, thread_id, handle) {
            return Err(RuntimeError::ThreadAlreadyRunning {
                thread_id: thread_id.to_string(),
            });
        }
        Ok(())
    }

    /// Unregister an active run when it completes (by run_id).
    pub(crate) fn unregister_run(&self, run_id: &str) {
        self.active_runs.unregister(run_id);
    }
}
