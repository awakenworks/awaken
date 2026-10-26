// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Foreground completion observes the one dispatch owner's settlement of the exact Run.
// @formal reason: This classifies the observation edge, not a proof claim.
//! [`CompletionRegistry`]: the temporary completion observers and best-effort
//! live routes of each foreground Run a Worker settles.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_dispatch_contract::CompletionSink;
use awaken_session_execution_contract::SessionRunActivation;

use crate::DeliveryLock;

/// A fixed completion relay followed by its committed-progress notification.
/// Both collaborators are selected together before a pool, scan or transport
/// can publish through this sink. Neither notification port is durable truth.
///
/// S4: a selected wakeup cannot be replaced.
/// ```compile_fail,E0616
/// use std::sync::Arc;
/// use awaken_session_run_delivery::SettlementWakeup;
/// fn replace(sink: &mut SettlementWakeup, other: Arc<dyn Fn() + Send + Sync>) {
///     sink.committed_progress = other;
/// }
/// ```
///
/// A notifying composition must supply its wakeup at construction.
/// ```compile_fail,E0061
/// use std::sync::Arc;
/// use awaken_dispatch_contract::CompletionSink;
/// use awaken_session_run_delivery::SettlementWakeup;
/// fn partial(relay: Arc<dyn CompletionSink>) {
///     let _ = SettlementWakeup::new(relay);
/// }
/// ```
pub struct SettlementWakeup {
    completion: Arc<dyn CompletionSink>,
    committed_progress: Arc<dyn Fn() + Send + Sync>,
}

impl SettlementWakeup {
    #[must_use]
    pub fn new(
        completion: Arc<dyn CompletionSink>,
        committed_progress: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            completion,
            committed_progress,
        }
    }
}

impl CompletionSink for SettlementWakeup {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        self.completion.settled(run_id, state);
        (self.committed_progress)();
    }
}

/// Wakes foreground durable submitters when the pool settles a Run and relays
/// that Run's best-effort live progress while each caller remains connected.
/// Exact replays share one run-id slot and retain independent drop guards;
/// neither is durable truth. Every exactly Thread-scoped observation is also
/// published through the existing Thread hub, so a background child needs no
/// completion registration.
///
/// The registry only notifies; committed Run truth decides. A poisoned
/// registry therefore refuses new observations and stops notifying, and each
/// already-registered waiter recovers through its bounded committed read:
///
/// | Rule | registry lock | operation            | effect |
/// |---|---|---|---|
/// | W1 | healthy  | register             | waiter installed before enqueue |
/// | W2 | poisoned | register             | refused (`session_executor_lock_poisoned`), nothing enqueued |
/// | W3 | healthy  | settle / live relay  | waiters woken / sinks fed |
/// | W4 | poisoned | settle / live relay  | reported; waiters settle by committed read |
pub struct CompletionRegistry {
    waiters: std::sync::Mutex<HashMap<String, Vec<ForegroundRegistration>>>,
    next_registration_id: std::sync::atomic::AtomicU64,
    hub: Arc<crate::ThreadEventHub>,
}

impl Default for CompletionRegistry {
    fn default() -> Self {
        Self::new(Arc::new(crate::ThreadEventHub::new()))
    }
}

struct ForegroundRegistration {
    id: u64,
    settled: tokio::sync::oneshot::Sender<RunState>,
    stream_sink: Option<Arc<dyn StreamSink>>,
}

impl CompletionRegistry {
    /// A registry that also publishes each Thread-scoped live observation on
    /// `hub`.
    #[must_use]
    pub fn new(hub: Arc<crate::ThreadEventHub>) -> Self {
        Self {
            waiters: Default::default(),
            next_registration_id: std::sync::atomic::AtomicU64::new(1),
            hub,
        }
    }

    /// Register interest in `run_id` BEFORE it is enqueued, so the pool cannot
    /// settle it before this caller is listening (no lost wakeup). Returns the
    /// receiver plus a [`WaiterGuard`] that removes the waiter if the caller's
    /// future is dropped before the run settles (e.g. a client disconnect), so an
    /// unwaited entry never lingers in the map.
    ///
    /// A poisoned registry refuses the observation before anything is
    /// enqueued, so no foreground caller waits on a waiter nobody can settle.
    pub fn register(
        self: &Arc<Self>,
        run_id: &RunId,
        stream_sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<(tokio::sync::oneshot::Receiver<RunState>, WaiterGuard), DeliveryLock> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let has_stream_sink = stream_sink.is_some();
        let registration_id = self
            .next_registration_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.waiters
            .lock()
            .map_err(|_| DeliveryLock::CompletionRegistry)?
            .entry(run_id.0.clone())
            .or_default()
            .push(ForegroundRegistration {
                id: registration_id,
                settled: tx,
                stream_sink,
            });
        tracing::trace!(
            run_id = %run_id.0,
            registration_id,
            has_stream_sink,
            "registered durable foreground observation"
        );
        let guard = WaiterGuard {
            registry: Arc::downgrade(self),
            run_id: run_id.0.clone(),
            registration_id,
        };
        Ok((rx, guard))
    }

    async fn route_observation(
        &self,
        observation: awaken_agent_contract::stream::event::Observation,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        if let Some(coordinate) = &observation.assistant_response {
            self.hub.publish(
                &coordinate.thread_id.0,
                crate::ThreadEvent::Live(observation.clone()),
            );
        }
        let sinks = match self.waiters.lock() {
            Ok(waiters) => waiters
                .get(&observation.event.run_id.0)
                .into_iter()
                .flat_map(|registrations| registrations.iter())
                .filter_map(|registration| registration.stream_sink.clone())
                .collect::<Vec<_>>(),
            Err(_) => {
                completion_registry_poisoned("live observation not relayed");
                return Ok(());
            }
        };
        tracing::trace!(
            run_id = %observation.event.run_id.0,
            matched = sinks.len(),
            "routed durable foreground stream event"
        );
        for sink in sinks {
            sink.send_observation(observation.clone()).await?;
        }
        Ok(())
    }
}

fn completion_registry_poisoned(consequence: &'static str) {
    tracing::error!(
        code = crate::LOCK_POISONED_CODE,
        consequence,
        "foreground completion registry lock poisoned"
    );
}

/// Removes a completion waiter on drop, so a foreground submit whose future is
/// dropped (client disconnect) or which timed out never leaves a stale sender in
/// the registry. On normal completion the sender is already gone (consumed by
/// [`CompletionSink::settled`]), so the removal is a harmless no-op.
pub struct WaiterGuard {
    registry: std::sync::Weak<CompletionRegistry>,
    run_id: String,
    registration_id: u64,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut waiters) = registry.waiters.lock()
        {
            let mut removed = false;
            let mut remove_run = false;
            if let Some(registrations) = waiters.get_mut(&self.run_id) {
                let before = registrations.len();
                registrations.retain(|registration| registration.id != self.registration_id);
                removed = registrations.len() != before;
                remove_run = registrations.is_empty();
            }
            if remove_run {
                waiters.remove(&self.run_id);
            }
            tracing::trace!(
                run_id = %self.run_id,
                registration_id = self.registration_id,
                removed,
                "released durable foreground observation"
            );
        }
    }
}

impl CompletionSink for CompletionRegistry {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        let Ok(mut waiters) = self.waiters.lock() else {
            completion_registry_poisoned("settlement not notified");
            return;
        };
        if let Some(registrations) = waiters.remove(&run_id.0) {
            tracing::trace!(
                run_id = %run_id.0,
                observers = registrations.len(),
                ?state,
                "settled durable foreground observation"
            );
            for registration in registrations {
                // A receiver may have already gone — a dropped send is fine.
                let _ = registration.settled.send(state.clone());
            }
        }
    }
}

#[async_trait::async_trait]
impl StreamSink for CompletionRegistry {
    async fn send(
        &self,
        event: awaken_agent_contract::stream::event::Event,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        self.route_observation(event.into()).await
    }

    async fn send_observation(
        &self,
        observation: awaken_agent_contract::stream::event::Observation,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        self.route_observation(observation).await
    }
}

/// The completion signal of a foreground Run closed while committed truth
/// still showed it unsettled: the observation cannot conclude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("durable completion signal closed before committed Run settlement")]
pub struct CompletionSignalClosed;

/// Await the existing completion signal while reconciling the one committed Run
/// authority. `None` means the Run is still live and must never be projected as a
/// timeout/error by this transport helper. Dispatch lease recovery and dead-letter
/// policy own genuinely abandoned execution; cancellation owns caller departure.
pub async fn await_completion_state<E, Read, ReadFuture>(
    settled: tokio::sync::oneshot::Receiver<RunState>,
    reconciliation_interval: std::time::Duration,
    mut read_settled: Read,
) -> Result<RunState, E>
where
    E: From<CompletionSignalClosed>,
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<Option<RunState>, E>>,
{
    let mut settled = std::pin::pin!(settled);
    // The local event is the fast path. A peer Coordinator can commit the same
    // shared PostgreSQL Run without owning this process's oneshot sender, so
    // committed-truth reconciliation is also required. One foreground waiter
    // performs one narrow read per interval; it never claims, settles, times out,
    // or creates a second completion authority.
    loop {
        tokio::select! {
            result = &mut settled => {
                if let Ok(state) = result {
                    return Ok(state);
                }
                return read_settled().await?.ok_or_else(|| CompletionSignalClosed.into());
            }
            _ = tokio::time::sleep(reconciliation_interval) => {
                if let Some(state) = read_settled().await? {
                    return Ok(state);
                }
            }
        }
    }
}

/// One register-before-activate composition shared by every Session User Run
/// foreground caller. Activation outcomes remain typed, while the only value
/// returned across the observation boundary is committed `RunState`.
pub async fn activate_and_await_session_run<E, Activate, ActivateFuture, Read, ReadFuture>(
    registry: &Arc<CompletionRegistry>,
    run_id: &RunId,
    stream_sink: Option<Arc<dyn StreamSink>>,
    reconciliation_interval: std::time::Duration,
    activate: Activate,
    mut read_settled: Read,
) -> Result<RunState, E>
where
    E: From<CompletionSignalClosed> + From<DeliveryLock>,
    Activate: FnOnce() -> ActivateFuture,
    ActivateFuture: std::future::Future<Output = Result<SessionRunActivation, E>>,
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<Option<RunState>, E>>,
{
    // The guard spans activation and the complete committed-truth wait. Dropping
    // the caller at either phase removes only this observation; Dispatch/Thread
    // authorities continue independently.
    let (settled, _waiter_guard) = registry.register(run_id, stream_sink)?;
    let activation = activate().await?;
    if matches!(activation, SessionRunActivation::Completed)
        && let Some(state) = read_settled().await?
    {
        return Ok(state);
    }
    await_completion_state(settled, reconciliation_interval, read_settled).await
}

#[cfg(test)]
mod tests;
