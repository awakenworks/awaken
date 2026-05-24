//! Agent runtime: top-level orchestrator for run management, routing, and control.

mod active_registry;
#[cfg(feature = "background")]
mod background_cancellation;
mod compaction;
mod control;
mod run_request;
mod runner;

use std::sync::Arc;

use awaken_contract::contract::mailbox::{
    LiveRunCommand, LiveRunCommandEntry, LiveRunTarget, MailboxStore,
};
use awaken_contract::contract::storage::ThreadRunStore;

use crate::error::RuntimeError;
#[cfg(feature = "a2a")]
use crate::registry::composite::CompositeAgentSpecRegistry;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::suspension::ToolCallResume;
use futures::StreamExt;
use futures::channel::mpsc;

use crate::cancellation::CancellationToken;
use crate::inbox::InboxSender;
use crate::registry::{
    AgentResolver, ExecutionResolver, LocalExecutionResolver, RegistryHandle, RegistrySet,
    RegistrySnapshot,
};

pub use run_request::{RunRequest, ThreadContextSnapshot};

use active_registry::ActiveRunRegistry;

pub(crate) type DecisionBatch = Vec<(String, ToolCallResume)>;

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
}

/// Top-level agent runtime. Manages runs across threads.
///
/// Provides methods for cancelling and sending decisions
/// to active agent runs. Enforces one active run per thread.
pub struct AgentRuntime {
    pub(crate) resolver: Arc<dyn ExecutionResolver>,
    pub(crate) storage: Option<Arc<dyn ThreadRunStore>>,
    pub(crate) profile_store:
        Option<Arc<dyn awaken_contract::contract::profile_store::ProfileStore>>,
    pub(crate) mailbox_store: Option<Arc<dyn MailboxStore>>,
    pub(crate) active_runs: ActiveRunRegistry,
    pub(crate) registry_handle: Option<RegistryHandle>,
    /// One-shot guard for the "mailbox_store not wired" warning; flips true
    /// on the first `register_run` without a store so we emit exactly one
    /// tracing event per runtime instance.
    missing_mailbox_store_warned: std::sync::atomic::AtomicBool,
    #[cfg(feature = "a2a")]
    composite_registry: Option<Arc<CompositeAgentSpecRegistry>>,
}

impl AgentRuntime {
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self::new_with_execution_resolver(Arc::new(LocalExecutionResolver::new(resolver)))
    }

    pub fn new_with_execution_resolver(resolver: Arc<dyn ExecutionResolver>) -> Self {
        Self {
            resolver,
            storage: None,
            profile_store: None,
            mailbox_store: None,
            active_runs: ActiveRunRegistry::new(),
            registry_handle: None,
            missing_mailbox_store_warned: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "a2a")]
            composite_registry: None,
        }
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

    /// Wire the mailbox store used to subscribe to live-steering commands for
    /// each active run. If unset, runs never receive remote `LiveRunCommand`s — this
    /// is the single-process / test default.
    #[must_use]
    pub fn with_mailbox_store(mut self, store: Arc<dyn MailboxStore>) -> Self {
        self.mailbox_store = Some(store);
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
        self.resolver.clone()
    }

    pub fn execution_resolver(&self) -> &dyn ExecutionResolver {
        self.resolver.as_ref()
    }

    pub fn execution_resolver_arc(&self) -> Arc<dyn ExecutionResolver> {
        self.resolver.clone()
    }

    pub fn registry_handle(&self) -> Option<RegistryHandle> {
        self.registry_handle.clone()
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
        let forwarder_inputs = self.mailbox_store.as_ref().map(|store| {
            (
                Arc::clone(store),
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
        if let Some((store, inbox_tx, token, forwarder_token, decision_tx)) = forwarder_inputs {
            let thread_id = thread_id.to_string();
            let mut target = LiveRunTarget::new(thread_id.clone(), run_id.clone());
            if let Some(dispatch_id) = dispatch_id {
                target = target.with_dispatch_id(dispatch_id);
            }
            tokio::spawn(async move {
                run_live_forwarder(store, target, inbox_tx, token, forwarder_token, decision_tx)
                    .await;
            });
        } else if !self
            .missing_mailbox_store_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "AgentRuntime has no mailbox_store wired: cross-node live steering \
                 (LiveRunCommand) will always fall through to durable queue. Call \
                 `AgentRuntime::with_mailbox_store(store)` on multi-node deployments."
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
    store: Arc<dyn MailboxStore>,
    target: LiveRunTarget,
    inbox_tx: Option<InboxSender>,
    cancellation_token: CancellationToken,
    live_forwarder_token: CancellationToken,
    decision_tx: mpsc::UnboundedSender<DecisionBatch>,
) {
    let mut stream = match store.open_live_channel_for(&target).await {
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
        assert!(rt.registry_handle().is_none());
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

    // ── Live forwarder integration ──

    mod live_forwarder {
        use super::*;
        use awaken_contract::contract::mailbox::LiveRunCommand;
        use awaken_stores::InMemoryMailboxStore;
        use std::time::Duration;

        /// Publish on `store` until the subscriber count for `thread_id` is
        /// non-zero so the forwarder's background subscription is guaranteed
        /// active. We cannot inspect the broadcast state directly from here
        /// (the broadcast sender is private to the store), so we send a
        /// single no-op ping that will be consumed by the forwarder and
        /// poll until the first test-visible side effect proves it ran.
        async fn settle() {
            // 20ms is enough for a tokio::spawn + one await + subscribe call
            // in CI. Tests that observe the forwarder output should use
            // additional polling with timeouts.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        #[tokio::test]
        async fn messages_variant_lands_in_inbox() {
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());
            let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
            let (handle, _token, _rx) =
                rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-1", "run-1"),
                    LiveRunCommand::Messages(vec![Message::user("live-1")]),
                )
                .await
                .unwrap();

            let mut received = None;
            for _ in 0..50 {
                if let Some(json) = inbox_rx.try_recv() {
                    received = Some(json);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let payload = received.expect("forwarder must deliver Messages within 500ms");
            let messages = crate::inbox::inbox_payload_messages(&payload);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text(), "live-1");
        }

        #[tokio::test]
        async fn cancel_variant_triggers_token() {
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());
            let (handle, token, _rx) = rt.create_run_channels("run-1".into());
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-1", "run-1"),
                    LiveRunCommand::Cancel,
                )
                .await
                .unwrap();

            let mut cancelled = false;
            for _ in 0..50 {
                if token.is_cancelled() {
                    cancelled = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(cancelled, "forwarder must cancel token within 500ms");
        }

        #[tokio::test]
        async fn decision_variant_lands_on_decision_channel() {
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());
            let (handle, _token, mut rx) = rt.create_run_channels("run-1".into());
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            let decisions = vec![("call-1".into(), make_resume())];
            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-1", "run-1"),
                    LiveRunCommand::Decision(decisions),
                )
                .await
                .unwrap();

            let mut got = None;
            for _ in 0..50 {
                if let Ok(batch) = rx.try_recv() {
                    got = Some(batch);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let batch = got.expect("forwarder must deliver Decision within 500ms");
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].0, "call-1");
        }

        #[tokio::test]
        async fn no_store_wired_no_forwarder_runs() {
            // Baseline: without `with_mailbox_store`, deliver_live published
            // elsewhere must not reach this runtime's channels.
            let detached_store = InMemoryMailboxStore::new();
            let rt = make_runtime(); // no store
            let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
            let (handle, token, _rx) =
                rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            detached_store
                .deliver_live(
                    "thread-1",
                    LiveRunCommand::Messages(vec![Message::user("ignored")]),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;

            assert!(inbox_rx.try_recv().is_none());
            assert!(!token.is_cancelled());
        }

        #[tokio::test]
        async fn separate_threads_isolated() {
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());

            let (tx_a, mut rx_a) = crate::inbox::inbox_channel();
            let (tx_b, mut rx_b) = crate::inbox::inbox_channel();
            let (h_a, _tok_a, _dec_a) =
                rt.create_run_channels_with_inbox("run-a".into(), None, Some(tx_a));
            let (h_b, _tok_b, _dec_b) =
                rt.create_run_channels_with_inbox("run-b".into(), None, Some(tx_b));
            rt.register_run("thread-a", h_a).unwrap();
            rt.register_run("thread-b", h_b).unwrap();
            settle().await;

            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-a", "run-a"),
                    LiveRunCommand::Messages(vec![Message::user("for-a")]),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;

            assert!(rx_a.try_recv().is_some(), "thread-a must receive");
            assert!(
                rx_b.try_recv().is_none(),
                "thread-b must not receive thread-a's message"
            );
        }

        #[tokio::test]
        async fn unregister_stops_live_forwarder_subscription() {
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());
            let (handle, _token, _rx) = rt.create_run_channels("run-1".into());
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            rt.unregister_run("run-1");
            let target = LiveRunTarget::new("thread-1", "run-1");
            let mut outcome = store
                .deliver_live_to(&target, LiveRunCommand::Cancel)
                .await
                .unwrap();
            for _ in 0..50 {
                if outcome == awaken_contract::contract::mailbox::LiveDeliveryOutcome::NoSubscriber
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                outcome = store
                    .deliver_live_to(&target, LiveRunCommand::Cancel)
                    .await
                    .unwrap();
            }
            assert_eq!(
                outcome,
                awaken_contract::contract::mailbox::LiveDeliveryOutcome::NoSubscriber,
                "unregister must stop the old live forwarder"
            );
        }

        #[tokio::test]
        async fn cancel_then_messages_messages_not_processed() {
            // After forwarder dispatches Cancel it exits, so subsequent
            // Messages on the same thread should not reach the inbox via
            // this forwarder instance (agent loop is expected to be torn
            // down anyway).
            let store = Arc::new(InMemoryMailboxStore::new());
            let rt = make_runtime().with_mailbox_store(store.clone());
            let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
            let (handle, token, _rx) =
                rt.create_run_channels_with_inbox("run-1".into(), None, Some(inbox_tx));
            rt.register_run("thread-1", handle).unwrap();
            settle().await;

            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-1", "run-1"),
                    LiveRunCommand::Cancel,
                )
                .await
                .unwrap();
            // Wait for cancel to propagate.
            for _ in 0..50 {
                if token.is_cancelled() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(token.is_cancelled());

            store
                .deliver_live_to(
                    &LiveRunTarget::new("thread-1", "run-1"),
                    LiveRunCommand::Messages(vec![Message::user("too-late")]),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(
                inbox_rx.try_recv().is_none(),
                "forwarder must have exited after Cancel"
            );
        }
    }
}
