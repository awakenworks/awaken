//! Durable-foreground completion: the [`SharedHost`] pool-submit/await methods
//! and the [`CompletionRegistry`] event-wakeup machinery.

use super::*;

impl SharedHost {
    /// The process dispatch pool, or a fail-closed error when durable ingress (and
    /// thus the pool) is not enabled.
    pub(crate) fn dispatch_pool_or_err(
        &self,
    ) -> Result<&Arc<DispatchPool<AnyDispatchStore>>, HostError> {
        self.dispatch_pool.get().ok_or_else(|| {
            HostError::bad_request(
                "durable dispatch not enabled (set AWAKEN_INGRESS=durable to run the pool)",
            )
        })
    }

    /// Submit a durable run and wait for the pool to drive it to a settled state.
    /// Under the shared queue a session's own worker must not claim (it would grab
    /// foreign threads' runs), so the foreground durable path enqueues, nudges the
    /// pool, and waits for the pool to signal completion — **by event**, not by
    /// polling committed truth, so it pays no poll-interval latency. `supersede`
    /// marks the thread's prior pending work superseded first (ADR-0022).
    pub(crate) async fn submit_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        supersede: bool,
    ) -> Result<awaken_agent_contract::agent::run::RunState, HostError> {
        let run_id = activation.run_id.clone();
        // Register for the settle event BEFORE enqueue, so the pool cannot drive and
        // settle the run before this caller is listening (no lost wakeup). The guard
        // removes the waiter if this future is dropped (client disconnect) before it
        // settles — held to the end of this method.
        let (settled, _waiter_guard) = self.completion.register(&run_id);
        let pool = self.dispatch_pool_or_err()?;
        // Enqueue only — never drive here; the pool is the sole claimer. The common
        // path goes through `pool.submit` (which stamps the trace); a superseding
        // submit needs the supersede option, so it enqueues on the shared store and
        // nudges the pool directly.
        if supersede {
            let ingress = ctx
                .durable_ingress
                .as_ref()
                .ok_or_else(|| HostError::internal("durable submit requires durable ingress"))?;
            ingress
                .worker()
                .store()
                .enqueue_with(
                    RunDispatch::new(activation),
                    SubmitOptions {
                        supersede: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            pool.notify().await;
        } else {
            pool.submit(activation)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        }
        self.await_settled_event(ctx, &run_id, settled).await
    }

    /// Wait for the pool's settle signal for `run_id` (sub-millisecond wakeup), with
    /// a bounded timeout after which a single committed-truth read is the safety net
    /// (in case the pool died mid-drive). The event path replaces the old poll loop,
    /// removing the poll-interval floor from every durable foreground turn.
    async fn await_settled_event(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        settled: tokio::sync::oneshot::Receiver<RunState>,
    ) -> Result<RunState, HostError> {
        // ~60s ceiling — generous for a multi-step run's inference, bounded so a
        // stuck run surfaces as an error rather than hanging the request forever.
        match tokio::time::timeout(std::time::Duration::from_secs(60), settled).await {
            // The pool signalled the settled state the instant it settled.
            Ok(Ok(state)) => Ok(state),
            // Sender dropped without sending (pool died) or the wait timed out: fall
            // back to one committed-truth read, else surface a hard error. The
            // waiter entry is cleaned up by the caller's `WaiterGuard` on return.
            Ok(Err(_)) | Err(_) => self.read_settled_phase(ctx, run_id).ok_or_else(|| {
                HostError::internal(
                    "durable run did not settle: the dispatch pool never drove it to completion",
                )
            }),
        }
    }

    /// One committed-truth read: the run's state if it has settled (`Ended` or
    /// `Awaiting`), else `None`. The fallback path for `await_settled_event`.
    fn read_settled_phase(&self, ctx: &Arc<SessionCtx>, run_id: &RunId) -> Option<RunState> {
        use awaken_agent_contract::thread::read::run_store::RunStore;
        match RunStore::get(&*ctx.commit, run_id) {
            Some(record) if matches!(record.state, RunState::Ended(_) | RunState::Awaiting) => {
                Some(record.state)
            }
            _ => None,
        }
    }
}

/// Wakes a foreground durable submitter the instant the pool settles its run, so
/// the durable foreground path waits by **event** rather than polling committed
/// truth — removing the poll-interval latency floor. Keyed by run id; a run with no
/// registered waiter (a fire-and-forget background submit) settles as a no-op.
#[derive(Default)]
pub(crate) struct CompletionRegistry {
    waiters: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<RunState>>>,
}

impl CompletionRegistry {
    /// Register interest in `run_id` BEFORE it is enqueued, so the pool cannot
    /// settle it before this caller is listening (no lost wakeup). Returns the
    /// receiver plus a [`WaiterGuard`] that removes the waiter if the caller's
    /// future is dropped before the run settles (e.g. a client disconnect), so an
    /// unwaited entry never lingers in the map.
    fn register(
        self: &Arc<Self>,
        run_id: &RunId,
    ) -> (tokio::sync::oneshot::Receiver<RunState>, WaiterGuard) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiters
            .lock()
            .expect("completion registry poisoned")
            .insert(run_id.0.clone(), tx);
        let guard = WaiterGuard {
            registry: Arc::downgrade(self),
            run_id: run_id.0.clone(),
        };
        (rx, guard)
    }
}

/// Removes a completion waiter on drop, so a foreground submit whose future is
/// dropped (client disconnect) or which timed out never leaves a stale sender in
/// the registry. On normal completion the sender is already gone (consumed by
/// [`CompletionSink::settled`]), so the removal is a harmless no-op.
struct WaiterGuard {
    registry: std::sync::Weak<CompletionRegistry>,
    run_id: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut waiters) = registry.waiters.lock()
        {
            waiters.remove(&self.run_id);
        }
    }
}

impl CompletionSink for CompletionRegistry {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        if let Some(tx) = self
            .waiters
            .lock()
            .expect("completion registry poisoned")
            .remove(&run_id.0)
        {
            // The receiver may have already gone (timed out) — a dropped send is fine.
            let _ = tx.send(state.clone());
        }
    }
}

#[cfg(test)]
mod completion_tests {
    use super::{CompletionRegistry, RunId};
    use awaken_agent_contract::agent::run::RunState;
    use awaken_run_ingress::CompletionSink;
    use std::sync::Arc;

    /// A3: dropping the guard (caller future dropped / timed out) removes the
    /// waiter, so a run that never settles does not leak an entry.
    #[tokio::test]
    async fn dropping_the_guard_removes_the_registration() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, guard) = registry.register(&RunId("r".into()));
        assert_eq!(registry.waiters.lock().unwrap().len(), 1);
        drop(guard);
        drop(rx);
        assert!(
            registry.waiters.lock().unwrap().is_empty(),
            "the guard removed the leaked waiter"
        );
    }

    /// The happy path: `settled` delivers the state to the waiter and clears the
    /// slot, so the later guard drop is a no-op.
    #[tokio::test]
    async fn settled_delivers_the_state_and_clears_the_slot() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, _guard) = registry.register(&RunId("r".into()));
        registry.settled(&RunId("r".into()), &RunState::Awaiting);
        assert!(matches!(rx.await, Ok(RunState::Awaiting)));
        assert!(registry.waiters.lock().unwrap().is_empty());
    }
}
