//! Actual scheduler entrypoints, with deterministic returning/pending I/O cuts.
use super::*;
use harness::maintenance::{MaintenanceProbe, Operation};
use std::future::{Future, poll_fn};
use std::task::Poll;

struct ObservedResolver {
    inner: FlakyResolver,
    probe: Arc<MaintenanceProbe>,
}
#[async_trait]
impl WorkerResolver<FlakyDispatchStore> for ObservedResolver {
    async fn worker_for(
        &self,
        thread: &ThreadId,
        agent: Option<&str>,
    ) -> Result<Arc<FlakyWorker>, Error> {
        self.inner.worker_for(thread, agent).await
    }
    async fn reconcile_committed_terminals(
        &self,
        _: usize,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        Ok(self
            .probe
            .observe(Operation::Scan, async { Ok(Vec::new()) })
            .await?)
    }
}

pub(super) async fn check_all() {
    // Cause/effect table: ownership x actual entrypoint x optional timer.
    // B0-B11 preserve the original 12 initial/hint/fallback/shutdown rules.
    // P0-P29 additionally suspend each enabled GC/relay/scan port, request
    // shutdown, and return either success or an error. Constraints: only owner
    // rows can enter these ports; scan cuts require the timer to be enabled.
    // Effects: nonowners never maintain; Reconciler never drains; no maintenance
    // call overlaps another; explicit shutdown stays Pending until the in-flight
    // call returns, then joins. Errors remain returning observations, not stop
    // receipts or fabricated success. The raw production-call sequence is fed
    // to the existing TLA observation gate; these finite traces do not prove all
    // Tokio interleavings, nonreturning I/O or universal scheduling fairness.
    let mut steps = Vec::new();
    for owns in [false, true] {
        for topology in ["service", "pool", "reconciler"] {
            for terminal in [false, true] {
                steps.push(check_case(owns, topology, terminal, None, false).await);
                if owns {
                    for pause in [Operation::Gc, Operation::Relay, Operation::Scan] {
                        if pause == Operation::Scan && !terminal {
                            continue;
                        }
                        for fail in [false, true] {
                            steps.push(
                                check_case(owns, topology, terminal, Some(pause), fail).await,
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(steps.len(), 42, "12 baseline + 30 pending-return rules");
    awaken_reliability_testkit::write_observation_trace(
        "maintenance-scheduler-composition",
        "maintenance",
        serde_json::json!(steps),
    );
}

async fn check_case(
    owns: bool,
    topology: &str,
    terminal: bool,
    pause: Option<Operation>,
    fail: bool,
) -> serde_json::Value {
    let probe = Arc::new(MaintenanceProbe::new(pause, fail));
    let store = Arc::new(
        FlakyDispatchStore::new(0)
            .with_local_maintenance(owns)
            .with_maintenance_probe(probe.clone()),
    );
    let clock = Arc::new(ManualClock::new(0));
    let wake = Arc::new(LocalWakeSignal::new());
    let worker = Arc::new(DispatchWorker::new(
        text_runtime(),
        store.clone(),
        Arc::new(MemoryCommitCoordinator::new()),
        "pool",
    ));
    let resolver = Arc::new(ObservedResolver {
        inner: FlakyResolver {
            worker: worker.clone(),
        },
        probe: probe.clone(),
    });
    let config = DispatchServiceConfig {
        poll_interval: Duration::from_millis(50),
        dead_letter_ttl: Some(Duration::from_secs(1)),
        terminal_reconciliation_interval: terminal.then_some(Duration::from_millis(10)),
        ..Default::default()
    };
    let mut stop: std::pin::Pin<Box<dyn Future<Output = ()>>> = match topology {
        "service" => Box::pin(
            awaken_dispatch::DispatchService::spawn_with_wake(
                worker,
                clock,
                config,
                wake.clone(),
            )
            .shutdown(),
        ),
        "pool" => Box::pin(
            DispatchPool::spawn_with_wake(
                store.clone(),
                clock,
                "pool",
                DEFAULT_LEASE_MS,
                config,
                resolver,
                1,
                wake.clone(),
            )
            .shutdown(),
        ),
        "reconciler" => Box::pin(
            DispatchReconciler::spawn(store.clone(), clock, wake.clone(), config, resolver, None)
                .shutdown(),
        ),
        _ => unreachable!(),
    };
    if pause.is_some() {
        tokio::time::timeout(Duration::from_secs(1), probe.entered.notified())
            .await
            .expect("reachable owner I/O cut");
        probe.event("cancel", None);
        poll_fn(|cx| {
            assert!(
                stop.as_mut().poll(cx).is_pending(),
                "in-flight I/O is joined"
            );
            Poll::Ready(())
        })
        .await;
        // Give the actual spawned scheduler a turn after cancellation. A drop
        // or select-on-shutdown mutant must not masquerade as a Pending join.
        tokio::task::yield_now().await;
        poll_fn(|cx| {
            assert!(stop.as_mut().poll(cx).is_pending(), "no early stop receipt");
            Poll::Ready(())
        })
        .await;
        probe.event("pending", None);
        probe.release();
    } else {
        tokio::task::yield_now().await;
        probe.event("hint", None);
        wake.publish().await.unwrap();
        tokio::task::yield_now().await;
        for _ in 0..3 {
            probe.event("poll", None);
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }
        let calls = store.maintenance_calls();
        assert!(calls.iter().all(|count| (*count > 0) == owns));
        let scan_count = probe
            .snapshot()
            .iter()
            .filter(|event| event["kind"] == "begin" && event["operation"] == "scan")
            .count();
        assert_eq!(scan_count > 0, owns && terminal);
        if topology != "reconciler" {
            assert!(store.claim_attempts() > 0, "execution owner retains drains");
            if !owns {
                assert!(store.claim_attempts() <= 16, "no disabled-timer spin");
            }
        } else {
            assert_eq!(store.claim_attempts(), 0, "maintenance never drains");
        }
        probe.event("cancel", None);
    }
    tokio::time::timeout(Duration::from_secs(1), stop)
        .await
        .expect("shutdown joins returning I/O");
    probe.event("stop", None);
    let trace = probe.snapshot();
    assert_eq!(
        trace
            .iter()
            .filter(|event| event["kind"] == "error")
            .count(),
        usize::from(fail)
    );
    serde_json::json!({"rule": format!("owner-{owns}-{topology}-timer-{terminal}-pause-{}-error-{fail}", pause.map_or("none", Operation::name)),
        "owns": owns, "topology": topology, "terminal": terminal,
        "pause": pause.map_or("none", Operation::name), "fail": fail, "trace": trace})
}
