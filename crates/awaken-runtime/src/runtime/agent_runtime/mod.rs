//! Agent runtime: top-level orchestrator for run management, routing, and control.

mod active_registry;
mod control;
mod run_request;
mod runner;

use std::sync::Arc;

use awaken_contract::contract::storage::ThreadRunStore;

use crate::error::RuntimeError;
#[cfg(feature = "a2a")]
use crate::registry::composite::CompositeAgentSpecRegistry;
use awaken_contract::contract::suspension::ToolCallResume;
use futures::channel::mpsc;

use crate::cancellation::CancellationToken;
use crate::registry::AgentResolver;

pub use run_request::RunRequest;

use active_registry::ActiveRunRegistry;

pub(crate) type DecisionBatch = Vec<(String, ToolCallResume)>;

// ---------------------------------------------------------------------------
// RunHandle
// ---------------------------------------------------------------------------

/// Internal control handle for a running agent loop.
///
/// Stored in `ActiveRunRegistry` for the lifetime of a run.
/// External control is exposed via `AgentRuntime::cancel()` / `send_decisions()`.
#[derive(Clone)]
pub(crate) struct RunHandle {
    pub(crate) run_id: String,
    cancellation_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<DecisionBatch>,
}

impl RunHandle {
    /// Cancel the running agent loop cooperatively.
    pub(crate) fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Send one or more tool call decisions to the running loop atomically.
    pub(crate) fn send_decisions(
        &self,
        decisions: DecisionBatch,
    ) -> Result<(), Box<mpsc::TrySendError<DecisionBatch>>> {
        self.decision_tx.unbounded_send(decisions).map_err(Box::new)
    }

    /// Send a single tool call decision to the running loop.
    pub(crate) fn send_decision(
        &self,
        call_id: String,
        resume: ToolCallResume,
    ) -> Result<(), Box<mpsc::TrySendError<DecisionBatch>>> {
        self.send_decisions(vec![(call_id, resume)])
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
    pub(crate) profile_store:
        Option<Arc<dyn awaken_contract::contract::profile_store::ProfileStore>>,
    pub(crate) active_runs: ActiveRunRegistry,
    #[cfg(feature = "a2a")]
    composite_registry: Option<Arc<CompositeAgentSpecRegistry>>,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self {
            resolver,
            storage: None,
            profile_store: None,
            active_runs: ActiveRunRegistry::new(),
            #[cfg(feature = "a2a")]
            composite_registry: None,
        }
    }

    /// Create a new runtime that preserves the current stores but swaps the resolver.
    ///
    /// Active runs are not shared with the original runtime.
    #[must_use]
    pub fn clone_with_resolver(&self, resolver: Arc<dyn AgentResolver>) -> Self {
        Self {
            resolver,
            storage: self.storage.clone(),
            profile_store: self.profile_store.clone(),
            active_runs: ActiveRunRegistry::new(),
            #[cfg(feature = "a2a")]
            composite_registry: self.composite_registry.clone(),
        }
    }

    #[must_use]
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.storage = Some(store);
        self
    }

    #[must_use]
    pub(crate) fn with_profile_store(
        mut self,
        store: Arc<dyn awaken_contract::contract::profile_store::ProfileStore>,
    ) -> Self {
        self.profile_store = Some(store);
        self
    }

    pub fn resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    /// Return a cloned `Arc` of the agent resolver.
    pub fn resolver_arc(&self) -> Arc<dyn AgentResolver> {
        Arc::clone(&self.resolver)
    }

    #[cfg(feature = "a2a")]
    #[must_use]
    pub fn with_composite_registry(mut self, registry: Arc<CompositeAgentSpecRegistry>) -> Self {
        self.composite_registry = Some(registry);
        self
    }

    /// Return the composite registry, if one was configured.
    #[cfg(feature = "a2a")]
    pub fn composite_registry(&self) -> Option<&Arc<CompositeAgentSpecRegistry>> {
        self.composite_registry.as_ref()
    }

    /// Initialize the runtime — discover remote agents.
    /// Call this after `build()` to complete async initialization.
    #[cfg(feature = "a2a")]
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
    ) -> (
        RunHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<DecisionBatch>,
    ) {
        let token = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded();

        let handle = RunHandle {
            run_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
    use serde_json::Value;

    struct StubResolver;
    impl crate::registry::AgentResolver for StubResolver {
        fn resolve(
            &self,
            agent_id: &str,
        ) -> Result<crate::registry::ResolvedAgent, crate::error::RuntimeError> {
            Err(crate::error::RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
    }

    fn make_runtime() -> AgentRuntime {
        AgentRuntime::new(Arc::new(StubResolver))
    }

    fn make_resume() -> ToolCallResume {
        ToolCallResume {
            decision_id: "d1".into(),
            action: ResumeDecisionAction::Resume,
            result: Value::Null,
            reason: None,
            updated_at: 0,
        }
    }

    #[test]
    fn new_creates_runtime() {
        let rt = make_runtime();
        assert!(rt.storage.is_none());
        assert!(rt.profile_store.is_none());
    }

    #[test]
    fn resolver_returns_ref() {
        let rt = make_runtime();
        // The stub resolver always returns AgentNotFound
        let err = rt.resolver().resolve("any").unwrap_err();
        assert!(
            matches!(err, crate::error::RuntimeError::AgentNotFound { .. }),
            "expected AgentNotFound, got {err:?}"
        );
    }

    #[test]
    fn resolver_arc_returns_clone() {
        let rt = make_runtime();
        let arc = rt.resolver_arc();
        let err = arc.resolve("x").unwrap_err();
        assert!(matches!(
            err,
            crate::error::RuntimeError::AgentNotFound { .. }
        ));
    }

    #[test]
    fn with_thread_run_store_sets_store() {
        let store = Arc::new(awaken_stores::InMemoryStore::new());
        let rt = make_runtime().with_thread_run_store(store);
        assert!(rt.thread_run_store().is_some());
    }

    #[test]
    fn thread_run_store_none_by_default() {
        let rt = make_runtime();
        assert!(rt.thread_run_store().is_none());
    }

    #[test]
    fn create_run_channels_returns_triple() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("run-1".into());
        assert_eq!(handle.run_id, "run-1");
        assert!(!token.is_cancelled());
    }

    #[test]
    fn register_run_succeeds() {
        let rt = make_runtime();
        let (handle, _token, _rx) = rt.create_run_channels("run-1".into());
        assert!(rt.register_run("thread-1", handle).is_ok());
    }

    #[test]
    fn register_run_fails_for_same_thread() {
        let rt = make_runtime();
        let (h1, _, _rx1) = rt.create_run_channels("run-1".into());
        let (h2, _, _rx2) = rt.create_run_channels("run-2".into());
        rt.register_run("thread-1", h1).unwrap();
        let err = rt.register_run("thread-1", h2).unwrap_err();
        assert!(
            matches!(err, RuntimeError::ThreadAlreadyRunning { ref thread_id } if thread_id == "thread-1"),
            "expected ThreadAlreadyRunning, got {err:?}"
        );
    }

    #[test]
    fn unregister_run_allows_reregistration() {
        let rt = make_runtime();
        let (h1, _, _rx1) = rt.create_run_channels("run-1".into());
        rt.register_run("thread-1", h1).unwrap();
        rt.unregister_run("run-1");

        let (h2, _, _rx2) = rt.create_run_channels("run-2".into());
        assert!(rt.register_run("thread-1", h2).is_ok());
    }

    #[test]
    fn run_handle_cancel() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("run-1".into());
        assert!(!token.is_cancelled());
        handle.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn run_handle_send_decisions() {
        let rt = make_runtime();
        let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
        let decisions = vec![("call-1".into(), make_resume())];
        handle.send_decisions(decisions).unwrap();

        // Receive the batch from the channel
        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "call-1");
    }

    #[test]
    fn run_handle_send_decision_single() {
        let rt = make_runtime();
        let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
        handle
            .send_decision("call-2".into(), make_resume())
            .unwrap();

        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "call-2");
    }

    #[test]
    fn run_handle_send_decisions_closed_channel() {
        let rt = make_runtime();
        let (handle, _token, rx) = rt.create_run_channels("run-1".into());
        // Drop the receiver to close the channel
        drop(rx);

        let result = handle.send_decisions(vec![("call-1".into(), make_resume())]);
        assert!(result.is_err(), "send should fail when receiver is dropped");
    }
}
