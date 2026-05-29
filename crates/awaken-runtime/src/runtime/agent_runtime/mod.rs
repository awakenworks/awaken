//! Agent runtime: top-level orchestrator for run management, routing, and control.

mod active_registry;
mod commit;
mod control;
mod entrypoints;
mod runner;

use std::sync::{Arc, RwLock};

use awaken_runtime_contract::contract::commit_coordinator::CommitCoordinator;
use awaken_runtime_contract::contract::live_control::{
    LiveRunCommand, LiveRunCommandEntry, LiveRunCommandSource, LiveRunTarget,
};
use awaken_runtime_contract::contract::storage::ThreadRunStore;

use crate::error::RuntimeError;
#[cfg(feature = "a2a")]
use crate::registry::composite::CompositeAgentSpecRegistry;
use awaken_runtime_contract::contract::message::Message;
use awaken_runtime_contract::contract::suspension::ToolCallResume;
use futures::StreamExt;
use futures::channel::mpsc;

use crate::cancellation::CancellationToken;
use crate::inbox::InboxSender;
use crate::registry::{AgentResolver, RegistryHandle, RegistrySet, RegistrySnapshot};
use crate::resolution::{
    BackendRequirements, CapabilityDecision, LocalRegistryResolver, ResolutionPolicy,
    ResolutionRequest, ResolutionTarget, ResolveError, ResolvedRunPlan, Resolver, RootScopeKind,
};

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
    pub(crate) dispatch_id: Option<String>,
    cancellation_token: CancellationToken,
    live_forwarder_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<DecisionBatch>,
    inbox_tx: Option<InboxSender>,
}

impl RunHandle {
    /// Cancel the running agent loop cooperatively.
    pub(crate) fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    pub(crate) fn stop_live_forwarder(&self) {
        self.live_forwarder_token.cancel();
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

    /// Send direct input messages into the running loop's inbox.
    pub(crate) fn send_messages(&self, messages: Vec<Message>) -> bool {
        let Some(inbox_tx) = self.inbox_tx.as_ref() else {
            return false;
        };
        if messages.is_empty() || inbox_tx.is_closed() {
            return false;
        }
        inbox_tx.try_send(crate::inbox::inbox_messages_payload(messages))
    }

    /// Wake the running loop so it freezes pending messages without adding an
    /// inbox message.
    pub(crate) fn wake_pending_boundary(&self) -> bool {
        let Some(inbox_tx) = self.inbox_tx.as_ref() else {
            return false;
        };
        if inbox_tx.is_closed() {
            return false;
        }
        inbox_tx.try_send(crate::inbox::pending_boundary_wake_payload())
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
    pub(crate) run_resolver: RwLock<Arc<dyn Resolver>>,
    pub(crate) storage: Option<Arc<dyn ThreadRunStore>>,
    pub(crate) commit_coordinator: Option<Arc<dyn CommitCoordinator>>,
    pub(crate) profile_store:
        Option<Arc<dyn awaken_runtime_contract::contract::profile_store::ProfileStore>>,
    pub(crate) live_control_source: Option<Arc<dyn LiveRunCommandSource>>,
    pub(crate) active_runs: ActiveRunRegistry,
    pub(crate) registry_handle: Option<RegistryHandle>,
    /// One-shot guard for the "live control source not wired" warning; flips true
    /// on the first `register_run` without a store so we emit exactly one
    /// tracing event per runtime instance.
    missing_live_control_source_warned: std::sync::atomic::AtomicBool,
    #[cfg(feature = "a2a")]
    composite_registry: Option<Arc<CompositeAgentSpecRegistry>>,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self::new_with_execution_resolver(resolver)
    }

    pub fn new_with_execution_resolver(resolver: Arc<dyn AgentResolver>) -> Self {
        let run_resolver = Arc::new(LocalRegistryResolver::new(resolver.clone()));
        Self {
            resolver,
            run_resolver: RwLock::new(run_resolver),
            storage: None,
            commit_coordinator: None,
            profile_store: None,
            live_control_source: None,
            active_runs: ActiveRunRegistry::new(),
            registry_handle: None,
            missing_live_control_source_warned: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "a2a")]
            composite_registry: None,
        }
    }

    #[must_use]
    pub fn with_run_resolver(mut self, resolver: Arc<dyn Resolver>) -> Self {
        *self
            .run_resolver
            .get_mut()
            .expect("run resolver lock is not poisoned during construction") = resolver;
        self
    }

    pub fn set_run_resolver(&self, resolver: Arc<dyn Resolver>) {
        *self
            .run_resolver
            .write()
            .expect("run resolver lock poisoned") = resolver;
    }

    #[must_use]
    pub fn with_registry_handle(mut self, handle: RegistryHandle) -> Self {
        self.registry_handle = Some(handle);
        self
    }

    #[must_use]
    pub fn with_thread_run_store(mut self, store: Arc<dyn ThreadRunStore>) -> Self {
        self.storage = Some(store);
        self
    }

    /// Wire the live-control source used to subscribe to ephemeral commands
    /// for each active run. If unset, runs never receive remote
    /// `LiveRunCommand`s — this is the single-process / test default.
    #[must_use]
    pub fn with_live_control_source(mut self, source: Arc<dyn LiveRunCommandSource>) -> Self {
        self.live_control_source = Some(source);
        self
    }

    #[must_use]
    pub(crate) fn with_profile_store(
        mut self,
        store: Arc<dyn awaken_runtime_contract::contract::profile_store::ProfileStore>,
    ) -> Self {
        self.profile_store = Some(store);
        self
    }

    pub fn resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    /// Return a cloned `Arc` of the agent resolver.
    pub fn resolver_arc(&self) -> Arc<dyn AgentResolver> {
        self.resolver.clone()
    }

    pub fn execution_resolver(&self) -> &dyn AgentResolver {
        self.resolver.as_ref()
    }

    pub fn execution_resolver_arc(&self) -> Arc<dyn AgentResolver> {
        self.resolver.clone()
    }

    pub fn registry_handle(&self) -> Option<RegistryHandle> {
        self.registry_handle.clone()
    }

    pub fn run_resolver(&self) -> Arc<dyn Resolver> {
        self.run_resolver_arc()
    }

    pub fn run_resolver_arc(&self) -> Arc<dyn Resolver> {
        self.run_resolver
            .read()
            .expect("run resolver lock poisoned")
            .clone()
    }

    pub async fn resolve_activation(
        &self,
        activation: &crate::RunActivation,
        policy: ResolutionPolicy,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        let request = ResolutionRequest::from_activation(activation, policy);
        let expected = BackendRequirements::from_features(&request.features);
        let resolver = self.run_resolver_arc();
        let plan = resolver.resolve(request).await?;
        if let CapabilityDecision::Unsupported(mismatches) = plan.backend_profile().check(&expected)
        {
            return Err(ResolveError::CapabilityMismatch(mismatches));
        }
        if matches!(policy, ResolutionPolicy::PersistentServer)
            && matches!(plan, ResolvedRunPlan::LiveOnly(_))
        {
            return Err(ResolveError::UnsupportedPersistence(
                "persistent execution requires ResolvedRun<ReplayableScope>".into(),
            ));
        }
        Ok(plan)
    }

    /// Resolve a sub-run (delegate / handoff) spawned mid-execution.
    ///
    /// ADR-0040 D7: a `Replayable` parent run requires every nested sub-run
    /// to also resolve as `Replayable`. A `LiveOnly` parent accepts either
    /// scope. The runtime fails closed on `Replayable` → `LiveOnly` with
    /// `ResolveError::NestedScopeMismatch` rather than silently downgrading
    /// the run's replayability guarantee.
    ///
    /// `sub_target` must be `Delegate` or `Handoff`; passing `Root` is a
    /// caller bug and is rejected.
    pub async fn resolve_nested(
        &self,
        parent_scope: RootScopeKind,
        sub_activation: &crate::RunActivation,
        sub_target: ResolutionTarget,
    ) -> Result<ResolvedRunPlan, ResolveError> {
        if matches!(sub_target, ResolutionTarget::Root { .. }) {
            return Err(ResolveError::UnsupportedTarget(
                "resolve_nested requires Delegate or Handoff target".into(),
            ));
        }
        let policy = match parent_scope {
            RootScopeKind::Replayable => ResolutionPolicy::PersistentServer,
            RootScopeKind::LiveOnly => ResolutionPolicy::LiveOnlyEmbedded,
        };
        let mut request = ResolutionRequest::from_activation(sub_activation, policy);
        request.target = sub_target;
        let expected = BackendRequirements::from_features(&request.features);
        let resolver = self.run_resolver_arc();
        let plan = resolver.resolve(request).await?;
        if let CapabilityDecision::Unsupported(mismatches) = plan.backend_profile().check(&expected)
        {
            return Err(ResolveError::CapabilityMismatch(mismatches));
        }
        if parent_scope == RootScopeKind::Replayable && matches!(plan, ResolvedRunPlan::LiveOnly(_))
        {
            return Err(ResolveError::NestedScopeMismatch(
                "replayable parent run cannot spawn a live-only sub-run".into(),
            ));
        }
        Ok(plan)
    }

    pub fn registry_snapshot(&self) -> Option<RegistrySnapshot> {
        self.registry_handle.as_ref().map(RegistryHandle::snapshot)
    }

    pub fn registry_version(&self) -> Option<u64> {
        self.registry_handle.as_ref().map(RegistryHandle::version)
    }

    pub fn registry_set(&self) -> Option<RegistrySet> {
        self.registry_snapshot()
            .map(RegistrySnapshot::into_registries)
    }

    pub fn replace_registry_set(&self, registries: RegistrySet) -> Option<u64> {
        self.registry_handle
            .as_ref()
            .map(|handle| handle.replace(registries))
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
    #[cfg(test)]
    pub(crate) fn create_run_channels(
        &self,
        run_id: String,
    ) -> (
        RunHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<DecisionBatch>,
    ) {
        self.create_run_channels_with_inbox(run_id, None, None)
    }

    pub(crate) fn create_run_channels_with_inbox(
        &self,
        run_id: String,
        dispatch_id: Option<String>,
        inbox_tx: Option<InboxSender>,
    ) -> (
        RunHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<DecisionBatch>,
    ) {
        let token = CancellationToken::new();
        let live_forwarder_token = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded();

        let handle = RunHandle {
            run_id,
            dispatch_id,
            cancellation_token: token.clone(),
            live_forwarder_token,
            decision_tx: tx,
            inbox_tx,
        };

        (handle, token, rx)
    }

    /// Register an active run. Returns error if thread already has one.
    ///
    /// Uses atomic try-insert to avoid TOCTOU race between check and insert.
    /// When a mailbox store is wired, spawns the live-command forwarder that
    /// dispatches remote `LiveRunCommand`s into this run's in-process channels.
    pub(crate) fn register_run(
        &self,
        thread_id: &str,
        handle: RunHandle,
    ) -> Result<(), RuntimeError> {
        let run_id = handle.run_id.clone();
        let dispatch_id = handle.dispatch_id.clone();
        let forwarder_inputs = self.live_control_source.as_ref().map(|source| {
            (
                Arc::clone(source),
                handle.inbox_tx.clone(),
                handle.cancellation_token.clone(),
                handle.live_forwarder_token.clone(),
                handle.decision_tx.clone(),
            )
        });
        if !self.active_runs.register(&run_id, thread_id, handle) {
            return Err(RuntimeError::ThreadAlreadyRunning {
                thread_id: thread_id.to_string(),
            });
        }
        if let Some((source, inbox_tx, token, forwarder_token, decision_tx)) = forwarder_inputs {
            let thread_id = thread_id.to_string();
            let mut target = LiveRunTarget::new(thread_id.clone(), run_id.clone());
            if let Some(dispatch_id) = dispatch_id {
                target = target.with_dispatch_id(dispatch_id);
            }
            tokio::spawn(async move {
                run_live_forwarder(
                    source,
                    target,
                    inbox_tx,
                    token,
                    forwarder_token,
                    decision_tx,
                )
                .await;
            });
        } else if !self
            .missing_live_control_source_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "AgentRuntime has no live control source wired: cross-node live steering \
                 (LiveRunCommand) will always fall through to durable queue. Call \
                 `AgentRuntime::with_live_control_source(source)` on multi-node deployments."
            );
        }
        Ok(())
    }

    /// Unregister an active run when it completes (by run_id).
    pub(crate) fn unregister_run(&self, run_id: &str) {
        self.active_runs.unregister(run_id);
    }
}

/// Forwarder task: subscribes to the mailbox's live channel for a specific
/// thread and translates each `LiveRunCommand` into the matching in-process signal.
///
/// Exits when:
/// - the run is unregistered and its forwarder token is cancelled,
/// - the subscription stream ends (store closed the channel),
/// - a `Cancel` has been dispatched (nothing more for this run to process),
/// - or a downstream channel is closed (agent loop already finished).
async fn run_live_forwarder(
    source: Arc<dyn LiveRunCommandSource>,
    target: LiveRunTarget,
    inbox_tx: Option<InboxSender>,
    cancellation_token: CancellationToken,
    live_forwarder_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<DecisionBatch>,
) {
    let mut stream = match source.open_live_channel_for(&target).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                thread_id = %target.thread_id,
                run_id = %target.run_id,
                dispatch_id = ?target.dispatch_id,
                error = %err,
                "live channel subscribe failed"
            );
            return;
        }
    };

    loop {
        if live_forwarder_token.is_cancelled() {
            break;
        }
        let next = tokio::select! {
            biased;
            _ = live_forwarder_token.cancelled() => break,
            next = stream.next() => next,
        };
        let Some(LiveRunCommandEntry { command, receipt }) = next else {
            break;
        };
        match command {
            LiveRunCommand::Messages(messages) => {
                let Some(tx) = inbox_tx.as_ref() else {
                    // No inbox: can't deliver. Drop the receipt without
                    // acking so the producer's `deliver_live` resolves as
                    // `NoSubscriber` and falls back to durable dispatch.
                    drop(receipt);
                    continue;
                };
                if tx.is_closed() {
                    drop(receipt);
                    break;
                }
                if tx.try_send(crate::inbox::inbox_messages_payload(messages)) {
                    receipt.ack();
                } else {
                    // Channel full or closed between the `is_closed` check
                    // and the send; treat as non-delivery.
                    drop(receipt);
                }
            }
            LiveRunCommand::PendingBoundaryWake => {
                let Some(tx) = inbox_tx.as_ref() else {
                    drop(receipt);
                    continue;
                };
                if tx.is_closed() {
                    drop(receipt);
                    break;
                }
                if tx.try_send(crate::inbox::pending_boundary_wake_payload()) {
                    receipt.ack();
                } else {
                    drop(receipt);
                }
            }
            LiveRunCommand::Cancel => {
                cancellation_token.cancel();
                // Cancellation is idempotent and always "accepted" once
                // the token is flipped; ack before exiting.
                receipt.ack();
                break;
            }
            LiveRunCommand::Decision(decisions) => {
                if decision_tx.is_closed() {
                    drop(receipt);
                    break;
                }
                if decision_tx.unbounded_send(decisions).is_ok() {
                    receipt.ack();
                } else {
                    drop(receipt);
                }
            }
            _ => {
                // `LiveRunCommand` is `#[non_exhaustive]`. A variant this
                // forwarder doesn't recognize usually means the producer is
                // newer than the consumer; silently dropping would let the
                // run continue in a state the producer believes it has
                // already mutated. Cancel the run so the caller observes
                // the version mismatch instead of getting corrupted output.
                tracing::error!(
                    thread_id = %target.thread_id,
                    run_id = %target.run_id,
                    dispatch_id = ?target.dispatch_id,
                    "unsupported live run command received; cancelling run to avoid silent divergence"
                );
                cancellation_token.cancel();
                drop(receipt);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests;
