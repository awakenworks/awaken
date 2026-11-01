use super::*;
use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_deployment_config::{
    DispatchPoolSizing, ExecutionTopology, ForegroundRunDelivery, LeaseOwnerId, LocalExecution,
    QueueDepthSource,
};
use awaken_dispatch_contract::DispatchEnqueue as _;
use awaken_dispatch_execution::{DispatchHint, DispatchWorker, ProcessDispatchLifecycle as _};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct HeldClaim {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    auxiliary_entered: tokio::sync::Notify,
    auxiliary_release: tokio::sync::Notify,
}

impl HeldClaim {
    async fn return_after_registering_auxiliary(self: Arc<Self>, background: &BackgroundRuns) {
        self.entered.notify_one();
        self.release.notified().await;
        background
            .spawn(
                crate::BackgroundWorkClass::ExternalDurable {
                    durable_intent_id: "last-owned-dispatch-task-auxiliary".into(),
                },
                async move {
                    self.auxiliary_entered.notify_one();
                    self.auxiliary_release.notified().await;
                },
            )
            .await;
    }
}

struct ProbeResolver {
    queue: Arc<AnyDispatchStore>,
    _scan_state:
        Arc<awaken_dispatch_execution::session_reconciliation::TerminalReconciliationState>,
    scans: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
    background: Arc<BackgroundRuns>,
    held: Option<Arc<HeldClaim>>,
    held_scan: Option<Arc<HeldClaim>>,
}

impl Drop for ProbeResolver {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for ProbeResolver {
    fn dispatch_store(&self) -> Result<Arc<AnyDispatchStore>, Error> {
        Ok(self.queue.clone())
    }

    async fn worker_for(
        &self,
        _: &ThreadId,
        _: Option<&str>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let held = self
            .held
            .as_ref()
            .expect("only the held-claim case enqueues");
        held.clone()
            .return_after_registering_auxiliary(&self.background)
            .await;
        // This test exercises the pool's real claim/retry/release boundary;
        // no model or synthetic committed terminal is needed to test drain order.
        Err(
            awaken_dispatch_execution::session_execution::execution_error(
                "original resolution refusal after registering auxiliary work",
            ),
        )
    }

    async fn reconcile_committed_terminals(
        &self,
        _: usize,
    ) -> Result<Vec<(RunId, awaken_agent_contract::agent::run::RunState)>, Error> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        if let Some(held) = &self.held_scan {
            held.clone()
                .return_after_registering_auxiliary(&self.background)
                .await;
        }
        Ok(Vec::new())
    }
}

struct Fixture {
    lifecycle: SessionDispatchLifecycle,
    store: Arc<awaken_dispatch::MemoryDispatchStore>,
    factories: Arc<AtomicUsize>,
    scans: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

fn fixture(
    local: bool,
    bound: bool,
    scans: bool,
    fail_first: bool,
    held: Option<Arc<HeldClaim>>,
) -> Fixture {
    let clock = awaken_time::system_clock();
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    let owner = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(store.clone()));
    let dispatch = Arc::new(ProcessDispatch::new(
        ExecutionTopology {
            foreground_delivery: ForegroundRunDelivery::Durable,
            local: if local {
                LocalExecution::InProcessPool(DispatchPoolSizing::LocalParallelism(
                    std::num::NonZeroUsize::new(1).unwrap(),
                ))
            } else {
                LocalExecution::RemoteWorkersOnly
            },
            lease_owner: LeaseOwnerId::new("selected-process").unwrap(),
            lost_wake_recovery_bound: Duration::from_millis(5),
            queue_depth: QueueDepthSource::External,
        },
        clock,
        DispatchHint::new(Arc::new(awaken_dispatch_execution::LocalWakeSignal::new())),
    ));
    let factories = Arc::new(AtomicUsize::new(0));
    let scan_count = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let background = Arc::new(BackgroundRuns::new());
    let lifecycle = SessionDispatchLifecycle::new(
        dispatch,
        bound.then(|| DispatchBinding::local(owner.clone())),
        Arc::new(crate::CompletionRegistry::default()),
        scans.then(|| owner.clone()),
        background.clone(),
        {
            let factories = factories.clone();
            let scans = scan_count.clone();
            let dropped = dropped.clone();
            move |dispatch| {
                let call = factories.fetch_add(1, Ordering::SeqCst);
                if fail_first && call == 0 {
                    return Err(
                        awaken_dispatch_execution::session_execution::execution_error(
                            "original resolver construction refusal",
                        ),
                    );
                }
                Ok(Arc::new(ProbeResolver {
                    queue: owner.worker_view(),
                    _scan_state: dispatch.terminal_reconciliation().clone(),
                    scans: scans.clone(),
                    dropped: dropped.clone(),
                    background: background.clone(),
                    held: held.clone().filter(|_| local),
                    held_scan: held.clone().filter(|_| !local),
                }))
            }
        },
    );
    Fixture {
        lifecycle,
        store,
        factories,
        scans: scan_count,
        dropped,
    }
}

async fn observed(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        // awaken-allow(wait): resolver destruction has no durable receipt;
        // observe its ownership witness within the enclosing bounded timeout.
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

#[tokio::test]
async fn shared_lifecycle_preserves_lazy_selection_and_original_startup_refusals() {
    // Causes: C1 local pool / remote placement, C2 dispatch binding present,
    // C3 committed-terminal owner selected. Effects: E1 no factory or service,
    // E2 one original pool, E3 one original scanner. Decision rules:
    // S1 local + !bound => E1; S2 local + bound => E2 regardless of C3;
    // S3 remote + !C3 => E1; S4 remote + C3 => E3. Repeated start is idempotent.
    for (local, bound, scans, started) in [
        (true, false, false, false),
        (true, true, true, true),
        (false, true, false, false),
        (false, true, true, true),
    ] {
        let f = fixture(local, bound, scans, false, None);
        assert_eq!(
            f.factories.load(Ordering::SeqCst),
            0,
            "construction stays lazy"
        );
        f.lifecycle.start().unwrap();
        f.lifecycle.start().unwrap();
        assert_eq!(f.factories.load(Ordering::SeqCst), usize::from(started));
        assert_eq!(f.lifecycle.dispatch().pool().is_some(), local && started);
        assert_eq!(f.lifecycle.dispatch().has_started_services(), started);
        assert!(f.lifecycle.drain(Duration::from_secs(2)).await);
    }
    // S5: original construction fails before service publication => no slot;
    // retry uses that same initializer. Pool errors propagate unchanged; the
    // scanner preserves its original false result. Drain prevents reopening.
    for local in [false, true] {
        let f = fixture(local, true, true, true, None);
        if local {
            assert!(
                f.lifecycle
                    .start_local_pool()
                    .unwrap_err()
                    .to_string()
                    .contains("original resolver construction refusal")
            );
        } else {
            assert!(!f.lifecycle.start_terminal_reconciliation());
        }
        assert!(!f.lifecycle.dispatch().has_started_services());
        f.lifecycle.start().unwrap();
        assert_eq!(f.factories.load(Ordering::SeqCst), 2);
        assert!(f.lifecycle.drain(Duration::from_secs(2)).await);
        f.lifecycle.start().unwrap();
        assert_eq!(
            f.factories.load(Ordering::SeqCst),
            2,
            "no restarted factory"
        );
        assert!(!f.lifecycle.dispatch().can_resume_work().await);
    }
}

#[tokio::test]
async fn shared_lifecycle_drains_claimed_work_before_its_last_auxiliary_registration() {
    // Causes: C1 a real claimed Run is resolving / the remote terminal scanner
    // is observing; C2 drain begins; C3 that task registers auxiliary work just
    // before return; C4 the auxiliary work remains live / completes; C5 deadline.
    // D0=C1+C2+C5(expired) => false, original task remains owned.
    // D1=C1+C2+C5(open) => close admission and await the original task.
    // D2=C3+C4(live) => keep waiting; D3=C4(done) => true.
    // Constraint: both C1 cases have zero physical execution capacity; that
    // heartbeat observation never proves dispatch-task completion. Resolution
    // returns its original failure through normal retry, with no fake terminal.
    for local in [true, false] {
        let held = Arc::new(HeldClaim::default());
        let f = fixture(local, true, !local, false, Some(held.clone()));
        if local {
            f.store
                .enqueue(awaken_dispatch_contract::RunDispatch::new(
                    awaken_runtime_contract::RunActivation::new(
                        RunId("held-run".into()),
                        ThreadId("held-thread".into()),
                        crate::test_support::publication("agent"),
                        Vec::new(),
                    ),
                ))
                .await
                .unwrap();
        }
        f.lifecycle.start().unwrap();
        tokio::time::timeout(Duration::from_secs(5), held.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            f.lifecycle.dispatch().in_flight(),
            0,
            "control work is not physical execution capacity, local={local}"
        );
        assert!(
            !f.lifecycle.drain(Duration::from_millis(10)).await,
            "D0 timeout retains the original dispatch task, local={local}"
        );
        let mut draining = tokio::spawn({
            let lifecycle = f.lifecycle.clone();
            async move { lifecycle.drain(Duration::from_secs(5)).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut draining)
                .await
                .is_err(),
            "D1 wait for original control work, local={local}"
        );
        held.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), held.auxiliary_entered.notified())
            .await
            .unwrap();
        assert!(
            !draining.is_finished(),
            "D2 last registration tracked, local={local}"
        );
        held.auxiliary_release.notify_one();
        assert!(
            draining.await.unwrap(),
            "D3 both original owners quiesced, local={local}"
        );
        assert_eq!(f.factories.load(Ordering::SeqCst), 1, "no recreated task");
    }
}

#[tokio::test(start_paused = true)]
async fn shared_lifecycle_timeout_keeps_original_background_work_and_cold_drain_closed() {
    // Causes: C1 no service started; C2 original auxiliary work remains live;
    // C3 the absolute budget expires / work later completes. T1=C1+C2+C3(expiry)
    // => false without aborting work; T2 subsequent start => closed original
    // admission, no factory; T3 C3(completes) => a later drain joins the same task.
    let f = fixture(true, true, false, false, None);
    let release = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    f.lifecycle
        .background
        .spawn(
            crate::BackgroundWorkClass::ExternalDurable {
                durable_intent_id: "original".into(),
            },
            {
                let release = release.clone();
                let finished = finished.clone();
                async move {
                    release.notified().await;
                    finished.store(true, Ordering::SeqCst);
                }
            },
        )
        .await;
    assert!(!f.lifecycle.drain(Duration::from_secs(2)).await, "T1");
    assert!(
        !finished.load(Ordering::SeqCst),
        "T1 no invented completion"
    );
    assert!(f.lifecycle.start_local_pool().is_err(), "T2");
    assert_eq!(
        f.factories.load(Ordering::SeqCst),
        0,
        "T2 cold gate precedes factory"
    );
    release.notify_one();
    assert!(f.lifecycle.drain(Duration::from_secs(2)).await, "T3");
    assert!(finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shared_lifecycle_releases_terminal_services_without_retaining_its_process() {
    // Causes: C1 original remote terminal loop is started; C2 the composition
    // and its selected lifecycle are dropped. L1=C1 => original scan executes;
    // L2=C2 => process releases, loop exits and releases resolver/scan state.
    // This constructs the shared lifecycle directly, with no executor or model.
    let f = fixture(false, true, true, false, None);
    let process = Arc::downgrade(f.lifecycle.dispatch());
    let scan_state = Arc::downgrade(f.lifecycle.dispatch().terminal_reconciliation());
    f.lifecycle.start().unwrap();
    observed("L1 original scan", || f.scans.load(Ordering::SeqCst) != 0).await;
    let dropped = f.dropped.clone();
    drop(f);
    assert!(
        process.upgrade().is_none(),
        "L2 no service-to-process back edge"
    );
    observed("L2 terminal resolver released", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    observed("L2 original scan state released", || {
        scan_state.upgrade().is_none()
    })
    .await;
}
