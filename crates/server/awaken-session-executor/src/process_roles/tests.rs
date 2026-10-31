// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-executor/src/process_roles.rs
// @formal reason: cfg(test) only; exercises the public complete constructor, exact ownership and refusal before service/model effects.
use super::*;
use crate::test_application_bindings::{fresh, retaining};
use awaken_session_execution_contract::{
    SessionApplicationBindingError as E, SessionApplicationMode as M, SessionApplications as A,
};

async fn host() -> Arc<crate::ExecutorComponents> {
    crate::SessionExecutorBuilder::ephemeral(Arc::new(crate::NoModelConfiguredExecutor), "startup")
        .await
        .build()
        .unwrap()
}

#[tokio::test]
async fn complete_startup_retains_sole_ports_through_clone_drain_and_final_drop() {
    // P1 sole inputs; P2 original lifecycle/Host references released; P3 clone
    // drained/dropped. Real Weak observations and Host-retaining ports detect
    // either premature release or a Host -> application ownership cycle.
    let host = host().await;
    let host_weak = Arc::downgrade(&host);
    let coordination = retaining(host.clone());
    let background = retaining(host.clone());
    let coordination_weak = Arc::downgrade(&coordination);
    let background_weak = Arc::downgrade(&background);
    let lifecycle = session_dispatch_lifecycle(
        &host,
        host.worker_completion_sink(),
        A::Local {
            coordination,
            background_runs: background,
        },
    )
    .unwrap();
    assert_eq!(host.session_application_binding().mode(), Ok(M::Local));
    assert!(
        !host.dispatch().has_started_services(),
        "construction alone starts no pool or scanner"
    );
    lifecycle.start_local_pool().unwrap();
    assert!(host.dispatch().has_started_services());
    let clone = lifecycle.clone();
    drop(lifecycle);
    drop(host);
    assert!(host_weak.upgrade().is_some());
    assert!(coordination_weak.upgrade().is_some());
    assert!(background_weak.upgrade().is_some());
    assert!(clone.drain(std::time::Duration::from_secs(2)).await);
    drop(clone);
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert!(host_weak.upgrade().is_none(), "no application/Host cycle");
}

#[tokio::test]
async fn wrong_topology_and_duplicate_selection_leave_the_original_whole_pair() {
    let host = host().await;
    assert!(matches!(
        session_dispatch_lifecycle(&host, host.worker_completion_sink(), A::ClaimedWorker),
        Err(E::InvalidTopology(_))
    ));
    assert_eq!(host.session_application_binding().mode(), Err(E::Unbound));
    assert!(!host.dispatch().has_started_services());
    let original_coordination = fresh();
    let original_background = fresh();
    let coordination_weak = Arc::downgrade(&original_coordination);
    let background_weak = Arc::downgrade(&original_background);
    let lifecycle = session_dispatch_lifecycle(
        &host,
        host.worker_completion_sink(),
        A::Local {
            coordination: original_coordination,
            background_runs: original_background,
        },
    )
    .unwrap();
    let candidate_coordination = fresh();
    let candidate_background = fresh();
    let refused_coordination = Arc::downgrade(&candidate_coordination);
    let refused_background = Arc::downgrade(&candidate_background);
    assert!(matches!(
        session_dispatch_lifecycle(
            &host,
            host.worker_completion_sink(),
            A::Local {
                coordination: candidate_coordination,
                background_runs: candidate_background
            }
        ),
        Err(E::AlreadyBound)
    ));
    assert!(refused_coordination.upgrade().is_none());
    assert!(refused_background.upgrade().is_none());
    assert!(coordination_weak.upgrade().is_some());
    assert!(background_weak.upgrade().is_some());
    assert_eq!(host.session_application_binding().mode(), Ok(M::Local));
    drop(lifecycle);
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert!(
        host.session_application_binding().acquire().is_err(),
        "expiry cannot authorize a fallback mode"
    );
}

#[tokio::test]
async fn unselected_host_refuses_resolver_before_any_service_effect() {
    let host = host().await;
    assert!(matches!(
        host.session_application_binding().acquire(),
        Err(E::Unbound)
    ));
    assert!(crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch).is_err());
    assert_eq!(host.session_application_binding().mode(), Err(E::Unbound));
    assert!(!host.dispatch().has_started_services());
}

#[tokio::test]
async fn started_services_reject_application_replacement_without_publishing_a_role() {
    let host = host().await;
    let lifecycle =
        session_dispatch_lifecycle(&host, host.worker_completion_sink(), A::Standalone).unwrap();
    lifecycle.start_local_pool().unwrap();
    assert!(host.dispatch().has_started_services());
    let coordination = fresh();
    let background = fresh();
    let coordination_weak = Arc::downgrade(&coordination);
    let background_weak = Arc::downgrade(&background);
    assert!(matches!(
        session_dispatch_lifecycle(
            &host,
            host.worker_completion_sink(),
            A::Local {
                coordination,
                background_runs: background
            }
        ),
        Err(E::ServicesAlreadyStarted)
    ));
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert_eq!(host.session_application_binding().mode(), Ok(M::Standalone));
    assert!(lifecycle.drain(std::time::Duration::from_secs(2)).await);
}

#[tokio::test]
async fn concurrent_complete_constructors_publish_exactly_one_coherent_pair() {
    // Two public constructors race on the same cloned Host, with distinct
    // exact pairs. A barrier controls the race; there is no timing sleep.
    // One success owns both winning ports, one AlreadyBound drops both losers;
    // readers must never observe coordination from A and background from B.
    let host = host().await;
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut weak_pairs = Vec::new();
    let mut workers = Vec::new();
    for _ in 0..2 {
        let coordination = fresh();
        let background = fresh();
        weak_pairs.push((Arc::downgrade(&coordination), Arc::downgrade(&background)));
        let worker_host = host.clone();
        let gate = barrier.clone();
        let completion = host.worker_completion_sink();
        workers.push(std::thread::spawn(move || {
            gate.wait();
            session_dispatch_lifecycle(
                &worker_host,
                completion,
                A::Local {
                    coordination,
                    background_runs: background,
                },
            )
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(E::AlreadyBound)))
            .count(),
        1
    );
    let winner = results.iter().position(Result::is_ok).unwrap();
    let coordination = host
        .session_application_binding()
        .agent_coordination()
        .unwrap();
    let background = host
        .session_application_binding()
        .background_runs()
        .unwrap();
    assert_eq!(
        Arc::as_ptr(&coordination) as *const (),
        weak_pairs[winner].0.as_ptr() as *const ()
    );
    assert_eq!(
        Arc::as_ptr(&background) as *const (),
        weak_pairs[winner].1.as_ptr() as *const ()
    );
    let loser = 1 - winner;
    assert!(weak_pairs[loser].0.upgrade().is_none());
    assert!(weak_pairs[loser].1.upgrade().is_none());
    drop(coordination);
    drop(background);
    drop(results);
    assert!(weak_pairs.iter().all(
        |(coordination, background)| coordination.upgrade().is_none()
            && background.upgrade().is_none()
    ));
}

#[tokio::test]
async fn drain_only_does_not_select_applications_or_construct_services() {
    let host = host().await;
    assert!(super::drain_session_dispatch(&host, std::time::Duration::from_secs(2)).await);
    assert_eq!(host.session_application_binding().mode(), Err(E::Unbound));
    assert!(!host.dispatch().has_started_services());
}

async fn worker_host(
    realization_control: bool,
    coordination_control: bool,
) -> Arc<crate::ExecutorComponents> {
    let clock = awaken_time::system_clock();
    let mut manifest = awaken_dispatch_contract::WorkerManifest::default();
    manifest.capacity.max_concurrent = 1;
    let registered = awaken_dispatch_contract::RegisteredCapacity::of(&manifest).unwrap();
    let memory = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        clock.clone(),
    ));
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    let authority = crate::host::test_upstream_authority().with_dispatch_binding(
        awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
            awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(store),
        )),
    );
    let process = crate::SessionExecutionProcess::from_deployment(
        &awaken_deployment_config::DeploymentConfig::ephemeral(),
        awaken_deployment_config::mint_local_workspace(),
        clock,
    )
    .unwrap();
    let builder = crate::SessionExecutorBuilder::for_worker(
        Arc::new(crate::NoModelConfiguredExecutor),
        "startup",
        crate::WorkerContentAdapters {
            file_content_source: Arc::new(awaken_resource_contract::UnavailableFileContentSource),
            memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
        },
        process,
        authority,
        registered,
    )
    .unwrap();
    let mut builder = builder.with_session_control(crate::coordination::test_claimed_control());
    if !realization_control {
        builder.worker.session_control = None;
    }
    if !coordination_control {
        builder.worker.session_coordination_control = None;
    }
    builder.build().unwrap()
}

#[tokio::test]
async fn claimed_worker_requires_both_control_capabilities_before_selection() {
    // Independent preconditions: 00, 01, 10 refuse before publication; 11
    // starts. Either missing capability alone must reject the composition.
    for (realization, coordination) in [(false, false), (false, true), (true, false)] {
        let absent = worker_host(realization, coordination).await;
        assert!(
            matches!(
                WorkerRoles::of(&absent, A::ClaimedWorker),
                Err(E::RequiredApplicationUnavailable(_))
            ),
            "realization={realization} coordination={coordination}"
        );
        assert_eq!(absent.session_application_binding().mode(), Err(E::Unbound));
        assert!(!absent.dispatch().has_started_services());
    }
    let complete = worker_host(true, true).await;
    assert!(matches!(
        session_dispatch_lifecycle(&complete, complete.worker_completion_sink(), A::Standalone),
        Err(E::InvalidTopology(_))
    ));
    let coordination = fresh();
    let background = fresh();
    let coordination_weak = Arc::downgrade(&coordination);
    let background_weak = Arc::downgrade(&background);
    assert!(matches!(
        session_dispatch_lifecycle(
            &complete,
            complete.worker_completion_sink(),
            A::Local {
                coordination,
                background_runs: background
            },
        ),
        Err(E::InvalidTopology(_))
    ));
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert_eq!(
        complete.session_application_binding().mode(),
        Err(E::Unbound)
    );
    assert!(!complete.dispatch().has_started_services());
    let roles =
        WorkerRoles::of(&complete, A::ClaimedWorker).expect("both original control roles supplied");
    assert_eq!(
        complete.session_application_binding().mode(),
        Ok(M::ClaimedWorker)
    );
    roles.start_pool().unwrap();
    assert!(complete.dispatch().has_started_services());
    assert!(roles.drain(std::time::Duration::from_secs(2)).await);
}

#[tokio::test]
async fn inflight_service_construction_refuses_complete_binding_and_releases_rejected_ports() {
    // Both service constructors hold their original completion/resolver ports
    // while the OnceCell is still empty. A complete public Local construction
    // must refuse without publishing a role or retaining either supplied Arc.
    // Failure releases the window; the same Host then composes and starts.
    for local_pool in [false, true] {
        let clock = awaken_time::system_clock();
        let owner = Arc::new(
            awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
                awaken_dispatch::MemoryDispatchStore::new(clock),
            )),
        );
        let builder = crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::NoModelConfiguredExecutor),
            "startup-race",
        )
        .await;
        let builder = if local_pool {
            builder.with_ephemeral_dispatch(owner.clone())
        } else {
            builder.with_ephemeral_coordinator_dispatch(owner.clone())
        };
        let host = builder.build().unwrap();
        let (entered, observed) = std::sync::mpsc::sync_channel(1);
        let (resume, release) = std::sync::mpsc::sync_channel(1);
        let starting_host = host.clone();
        let runtime = tokio::runtime::Handle::current();
        let starter = std::thread::spawn(move || {
            let _runtime = runtime.enter();
            if local_pool {
                starting_host
                    .dispatch
                    .ensure_pool(
                        starting_host.run_assembly.authority.dispatch_binding(),
                        starting_host.worker_completion_sink(),
                        || {
                            entered.send(()).unwrap();
                            release
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .unwrap();
                            Err(
                                awaken_dispatch_execution::session_execution::execution_error(
                                    "controlled startup refusal",
                                ),
                            )
                        },
                    )
                    .is_err()
            } else {
                !starting_host.dispatch.ensure_terminal_reconciliation(
                    true,
                    Some(owner),
                    starting_host.worker_completion_sink(),
                    || {
                        entered.send(()).unwrap();
                        release
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                        None
                    },
                )
            }
        });
        observed
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let coordination = fresh();
        let background = fresh();
        let coordination_weak = Arc::downgrade(&coordination);
        let background_weak = Arc::downgrade(&background);
        let composing_host = host.clone();
        let (finished, result) = std::sync::mpsc::sync_channel(1);
        let composer = std::thread::spawn(move || {
            let construction = session_dispatch_lifecycle(
                &composing_host,
                composing_host.worker_completion_sink(),
                A::Local {
                    coordination,
                    background_runs: background,
                },
            );
            let rejected = matches!(&construction, Err(E::ServicesAlreadyStarted));
            let selected = composing_host.session_application_binding().mode();
            drop(construction);
            finished.send((rejected, selected)).unwrap();
        });
        let outcome = result.recv_timeout(std::time::Duration::from_secs(10));
        // Always release/join the owned threads before asserting. A regression
        // that blocks configuration cannot strand the service factory.
        resume.send(()).unwrap();
        assert!(starter.join().unwrap());
        composer.join().unwrap();
        let (rejected, selected) =
            outcome.expect("configuration must refuse without awaiting the factory");
        assert!(rejected);
        assert_eq!(selected, Err(E::Unbound));
        assert_eq!(host.session_application_binding().mode(), Err(E::Unbound));
        assert!(!host.dispatch.has_started_services());
        assert!(coordination_weak.upgrade().is_none());
        assert!(background_weak.upgrade().is_none());
        let lifecycle = session_dispatch_lifecycle(
            &host,
            host.worker_completion_sink(),
            A::Local {
                coordination: fresh(),
                background_runs: fresh(),
            },
        )
        .expect("a failed service initialization leaves complete composition retryable");
        if local_pool {
            lifecycle.start_local_pool().unwrap();
        } else {
            assert!(lifecycle.start_terminal_reconciliation());
        }
        assert!(lifecycle.drain(std::time::Duration::from_secs(2)).await);
    }
}
