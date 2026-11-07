// @formal evidence worker_drain.closed_generation_is_absorbing
// @formal evidence worker_drain.no_claim_after_acknowledgement
//! Real startup/drain interleavings over the original admission owner.

use super::*;

fn local_pool() -> awaken_deployment_config::ClaimPool {
    awaken_deployment_config::ClaimPool::InProcess(DispatchPoolSizing::LocalParallelism(
        std::num::NonZeroUsize::new(1).unwrap(),
    ))
}

#[tokio::test]
async fn cold_pool_drain_is_absorbing_before_construction() {
    // Causes: C1 local pool topology with its real queue but no service; C2
    // drain before start; C3 repeated drain/start. Effects: E1 an actual local
    // admission fence; E2 no resolver construction, pool or reopened readiness.
    // Decision rules: D1=C1+C2 -> E1+E2; D2=D1+C3 -> the same E1+E2.
    // Existing failed/concurrent-start tables own retries before a drain.
    let fixture = coordinator_fixture(local_pool());
    for rule in ["D1", "D2"] {
        let fence = fixture.dispatch.begin_drain().await;
        let calls = AtomicUsize::new(0);
        let start = fixture.dispatch.ensure_pool(
            Some(DispatchBinding::local(fixture.owner.clone())),
            Arc::new(NoCompletion),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(fixture.resolver.clone())
            },
        );
        let published = fixture.dispatch.pool().is_some();
        // Release a pool created by the old implementation before asserting,
        // so a negative-control run cannot leave an unrelated live service.
        let _cleanup = fixture.dispatch.begin_drain().await;
        assert!(
            fence.is_some(),
            "{rule}/E1 the cold admission owner is fenced"
        );
        assert!(
            matches!(
                start,
                Err(Error::Dispatch(crate::DispatchError::Rejected(_)))
            ),
            "{rule}/E2 typed startup refusal"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{rule}/E2 no factory");
        assert!(!published, "{rule}/E2 no pool");
        assert!(
            !fixture.dispatch.accepting_work().await,
            "{rule}/E2 readiness"
        );
        assert!(
            !fixture.dispatch.resume_admission().await,
            "{rule}/E2 absorbing"
        );
    }
}

#[tokio::test]
async fn cold_terminal_drain_is_absorbing_before_construction() {
    // D3: C1 durable truth owner without a local pool; C2 drain before scan
    // construction -> E1 no pool fence, E2 no resolver, service or scan.
    // D4: repeating drain/start preserves all effects. A missing owner and
    // a declined/panicking resolver before drain retain their existing table.
    let fixture = coordinator_fixture(awaken_deployment_config::ClaimPool::RemoteWorkersOnly);
    for rule in ["D3", "D4"] {
        assert!(fixture.dispatch.begin_drain().await.is_none(), "{rule}/E1");
        let calls = AtomicUsize::new(0);
        let started = fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(fixture.resolver.clone())
            },
        );
        let published = fixture.dispatch.has_started_services();
        let _cleanup = fixture.dispatch.begin_drain().await;
        assert!(!started, "{rule}/E2 stopped before construction");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{rule}/E2 no factory");
        assert!(!published, "{rule}/E2 no service");
        assert_eq!(fixture.scans.load(Ordering::SeqCst), 0, "{rule}/E2 no scan");
    }
}

#[derive(Clone, Copy)]
enum PausedConstruction {
    Resolver,
    QueueBinding,
}

struct HeldQueueRead {
    resolver: Arc<ScanStateResolver>,
    race: Arc<StartRace>,
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for HeldQueueRead {
    fn dispatch_store(&self) -> Result<Arc<AnyDispatchStore>, Error> {
        self.race.enter_factory();
        self.resolver.dispatch_store()
    }

    async fn worker_for(
        &self,
        _thread: &awaken_agent_contract::agent::thread::Id,
        _agent: Option<&str>,
    ) -> Result<Arc<crate::DispatchWorker<AnyDispatchStore>>, Error> {
        unreachable!("a closed pool cannot resolve a claimed Run")
    }
}

async fn drain_during_service_construction(local: bool, phase: PausedConstruction) {
    // Cause/effect graph: C1 pool or maintenance topology; C2 the original
    // OnceCell initializer entered and is held before returning its resolver;
    // C3 drain runs while C2 remains held; C4 the constructor is released.
    // E1 drain completes without waiting for C4 and issues only C1's fence;
    // E2 the released constructor refuses and registers no service;
    // E3 later starts build no factory. Rules D5=pool+C2+C3+C4 and
    // D6=maintenance+C2+C3+C4 -> E1+E2+E3. D7 holds queue-identity validation
    // after the resolver returned, before pool publication: the same fence
    // still applies. Existing pool tests own in-flight
    // execution quiescence; this fixture owns no claim and invents no queue.
    let fixture = Arc::new(coordinator_fixture(if local {
        local_pool()
    } else {
        awaken_deployment_config::ClaimPool::RemoteWorkersOnly
    }));
    let race = Arc::new(StartRace::new());
    let first = {
        let fixture = fixture.clone();
        let race = race.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            if local {
                fixture
                    .dispatch
                    .ensure_pool(
                        Some(DispatchBinding::local(fixture.owner.clone())),
                        Arc::new(NoCompletion),
                        || match phase {
                            PausedConstruction::Resolver => {
                                race.enter_factory();
                                Ok(fixture.resolver.clone()
                                    as Arc<dyn WorkerResolver<AnyDispatchStore>>)
                            }
                            PausedConstruction::QueueBinding => Ok(Arc::new(HeldQueueRead {
                                resolver: fixture.resolver.clone(),
                                race: race.clone(),
                            })
                                as Arc<dyn WorkerResolver<AnyDispatchStore>>),
                        },
                    )
                    .map(|()| fixture.dispatch.pool().is_some())
            } else {
                Ok(fixture.dispatch.ensure_terminal_reconciliation(
                    true,
                    Some(fixture.owner.clone()),
                    Arc::new(NoCompletion),
                    || {
                        race.enter_factory();
                        Some(fixture.resolver.clone())
                    },
                ))
            }
        })
    };
    race.wait_for_first();
    // awaken-allow(wait): bound the real drain while a controlled synchronous initializer is held.
    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        fixture.dispatch.begin_drain(),
    )
    .await;
    let constructor_still_held = !first.is_finished();
    race.release.0.send(()).unwrap();
    let started = first.join().unwrap();
    let published = fixture.dispatch.has_started_services();
    let _cleanup = fixture.dispatch.begin_drain().await;
    assert!(constructor_still_held, "E1 drain precedes factory release");
    let fence = drained.expect("E1 drain must not await a synchronous factory");
    assert_eq!(fence.is_some(), local, "E1 exact topology's fence");
    if local {
        assert!(
            matches!(
                started,
                Err(Error::Dispatch(crate::DispatchError::Rejected(_)))
            ),
            "D5/E2 typed startup refusal"
        );
    } else {
        assert!(matches!(started, Ok(false)), "D6/E2 no loop start");
    }
    assert!(!published, "E2 no registered service after factory release");
    assert_eq!(
        race.calls.load(Ordering::SeqCst),
        1,
        "E2 original factory only"
    );
    assert!(!fixture.dispatch.accepting_work().await, "E2 not ready");
    if local {
        assert!(
            fixture
                .dispatch
                .ensure_pool(
                    Some(DispatchBinding::local(fixture.owner.clone())),
                    Arc::new(NoCompletion),
                    || panic!("D5/E3 a drained process cannot construct another resolver"),
                )
                .is_err()
        );
    } else {
        assert!(!fixture.dispatch.ensure_terminal_reconciliation(
            true,
            Some(fixture.owner.clone()),
            Arc::new(NoCompletion),
            || panic!("D6/E3 a drained process cannot construct another resolver"),
        ));
    }
}

#[tokio::test]
async fn pool_drain_closes_a_resolver_still_under_construction() {
    drain_during_service_construction(true, PausedConstruction::Resolver).await;
}

#[tokio::test]
async fn terminal_drain_closes_a_resolver_still_under_construction() {
    drain_during_service_construction(false, PausedConstruction::Resolver).await;
}

#[tokio::test]
async fn pool_drain_fences_a_late_pool_before_service_publication() {
    drain_during_service_construction(true, PausedConstruction::QueueBinding).await;
}
