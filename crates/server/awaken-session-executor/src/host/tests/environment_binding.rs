//! Session environment binding commits, adoption, and dispatch topology.

use super::*;

struct MovingRealizationFenceSink {
    authorize_calls: AtomicUsize,
    calls: AtomicUsize,
    accepted_epoch: AtomicU64,
}

#[derive(Default)]
struct ResponseLossBindingSink {
    authorize_calls: AtomicUsize,
    persist_calls: AtomicUsize,
    committed: std::sync::Mutex<Option<(String, String)>>,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for ResponseLossBindingSink {
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((effect_id, binding)) = self.committed.lock().unwrap().as_ref()
            && effect_id == intent.effect_id()
        {
            Ok(
                awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding: binding.clone(),
                },
            )
        } else {
            Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized)
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.persist_calls.fetch_add(1, Ordering::SeqCst);
        let mut committed = self.committed.lock().unwrap();
        if committed.as_ref() == Some(&(receipt.effect_id.clone(), receipt.binding.clone())) {
            return Ok(test_committed_environment(receipt));
        }
        *committed = Some((receipt.effect_id, receipt.binding));
        Err(awaken_session_types::RunError::unavailable(
            "root CAS response was lost",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for MovingRealizationFenceSink {
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        let asserted_epoch = intent.realization().map_or(0, |lease| lease.epoch);
        if asserted_epoch == self.accepted_epoch.load(Ordering::SeqCst) {
            Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized)
        } else {
            Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_stale",
                "a newer exact realization fence owns the Session",
            ))
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let asserted_epoch = receipt.realization.as_ref().map_or(0, |lease| lease.epoch);
        if asserted_epoch == self.accepted_epoch.load(Ordering::SeqCst) {
            Ok(test_committed_environment(receipt))
        } else {
            Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_stale",
                "a newer exact realization fence owns the Session",
            ))
        }
    }
}

// Immediate binding decision table: resident -> reuse without a write; absent +
// concurrent callers -> one lifecycle owner; pre-effect rejection -> zero
// provider/receipt effects; successful CAS -> persist before publish; ambiguous
// persistence -> retain the exact hidden Candidate and retry the same Store
// port; definite failure remains retryable with that Candidate and publishes
// nothing. Repository tests own CAS/idempotence at the aggregate.
#[tokio::test]
async fn claimed_ordinary_environment_uses_the_run_binding_not_the_session_root() {
    // Causes: C1 a current Run claim is installed; C2 Session admission/baseline
    // is absent or present; C3 a Session binding sink is configured. Effects:
    // E1 ordinary execution returns Unowned without calling the Session root;
    // E2 Managed execution calls that root. The later create path binds E1 to
    // the exact Run claim through DispatchQueue::bind_sandbox.
    //
    // | Rule | C1 | C2 Session | C3 | Effect |
    // | R1   | Y  | N          | Y  | E1     |
    // | R2   | Y  | Y          | Y  | E2     |
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());
    let thread = "claimed-ordinary-environment-authority";
    host.session_slots.update(thread, |slot| {
        slot.dispatch_claim = Some(awaken_dispatch_contract::RunClaim {
            run_id: RunId("claimed-ordinary-run".into()),
            owner: "worker-a".into(),
            epoch: 1,
        });
    });

    let ordinary = host
        .authorize_environment_effect_before_io(
            thread,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            None,
        )
        .await
        .expect("R1 ordinary authorization");
    assert!(
        matches!(
            ordinary.authorization,
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned
        ),
        "R1/E1"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 0, "R1/E1");

    host.session_slots
        .update(thread, |slot| slot.session_dispatch = true);
    let managed = host
        .authorize_environment_effect_before_io(
            thread,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            None,
        )
        .await
        .expect("R2 Managed authorization");
    assert!(
        matches!(
            managed.authorization,
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized
        ),
        "R2/E2"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1, "R2/E2");
}

#[tokio::test]
async fn new_environment_binding_commits_once_before_concurrent_contexts_can_use_it() {
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let (left, right) = tokio::join!(
        host.ctx_for("binding-order", None),
        host.ctx_for("binding-order", None)
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("binding-order").await.is_some());
}

#[tokio::test]
async fn root_response_loss_is_read_back_without_recreating_the_environment() {
    // Cause/effect table: C1 provider creation succeeds; C2 root CAS commits;
    // C3 its response is lost; C4 exact authorize readback returns the committed
    // binding. R1 C1+C2+C3 retains the hidden Candidate and returns retryable;
    // R2=R1+C4 retries the same idempotent persist, publishes the Store-read
    // identity with no second provider effect, and later context lookup is pure
    // slot reuse.
    use awaken_session_execution_contract::SessionRuntime;
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(ResponseLossBindingSink::default());
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let first_error = match host.ctx_for("binding-response-loss", None).await {
        Err(error) => error,
        Ok(_) => panic!("R1 lost response must remain retryable"),
    };
    assert_eq!(first_error.kind, HostErrorKind::Unavailable, "R1");
    assert!(
        host.session_environment("binding-response-loss")
            .await
            .is_none(),
        "R1 hidden Candidate is not published"
    );
    let candidate = host
        .prepared_session_environment("binding-response-loss")
        .expect("R1 exact hidden Candidate");
    let first = host
        .ctx_for("binding-response-loss", None)
        .await
        .expect("R2 exact retry reads back committed identity");
    let first_handle = first
        .env
        .as_ref()
        .expect("R1 Environment")
        .lifecycle()
        .handle();
    assert_eq!(
        first_handle,
        candidate.environment.lifecycle().handle(),
        "R2 no recreate"
    );
    let second = host
        .ctx_for("binding-response-loss", None)
        .await
        .expect("R1 slot replay");
    assert!(Arc::ptr_eq(&first, &second), "R1 no context recreation");
    assert_eq!(
        second
            .env
            .as_ref()
            .expect("R1 Environment")
            .lifecycle()
            .handle(),
        first_handle,
        "R1 immutable physical identity"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2, "R1+R2");
    assert_eq!(sink.persist_calls.load(Ordering::SeqCst), 2, "R1+R2");
}

#[tokio::test]
async fn already_applied_create_adopts_under_the_existing_lifecycle_lock() {
    // Cause/effect table: C1 root already committed this exact Create effect;
    // C2 the process-local slot is cold; C3 the physical handle is a Ready V2
    // realization created from the complete frozen projection and its exact
    // provider-effective spec; C4 the committed intent carries that projection's
    // immutable Environment fingerprint. R1 C1+C2+C3+C4 adopts and publishes
    // under the one lifecycle lock without a second create, receipt mutation,
    // or recursive lock acquisition; one idempotent Store read returns the
    // aggregate-generated Resident identity. The shared fixture owns
    // projection/spec/fence construction; this row varies only response loss.
    use awaken_session_execution_contract::SessionRuntime;
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await,
    ));
    install_test_session_application(&host);
    let thread = "binding-already-applied";
    let activation =
        crate::host::worker_resolver::test_support::test_activation(thread, "binding-applied");
    let projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            crate::host::worker_resolver::test_support::eager_environment(),
            &activation.snapshot,
        );
    let binding = crate::host::worker_resolver::test_support::available_local_environment_binding(
        &host,
        thread,
        &projection,
    )
    .await;
    let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
        thread,
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        None,
    )
    .for_environment(projection.baseline.environment.config_fingerprint.0.clone());
    let sink = Arc::new(ResponseLossBindingSink {
        committed: std::sync::Mutex::new(Some((intent.effect_id().to_string(), binding.clone()))),
        ..Default::default()
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let context = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        host.ctx_for(thread, None),
    )
    .await
    .expect("R1 lifecycle owner must not re-enter its mutex")
    .expect("R1 exact committed binding is adopted");
    assert_eq!(
        serde_json::to_string(
            &context
                .env
                .as_ref()
                .expect("R1 Environment")
                .lifecycle()
                .handle()
        )
        .unwrap(),
        binding,
        "R1 root identity"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1, "R1");
    assert_eq!(
        sink.persist_calls.load(Ordering::SeqCst),
        1,
        "R1 Store-read identity"
    );
}

#[tokio::test]
async fn dispatch_store_topology_keeps_one_physical_execution_owner() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the Runtime authority is ephemeral or durable; C2
    // this process owns local execution or is coordinator-only. Effects: E1 one
    // local pool drains accepted ordinary/background child Runs; E2 a
    // coordinator-only process never starts a competing claimer. Durability is a
    // persistence axis, not an execution-placement gate: otherwise an ephemeral
    // Managed Session can persist a coordinated child in its authority queue with
    // no Worker able to claim it.
    //
    // | Rule | Authority | Local owner | Effect |
    // |---|---|---|---|
    // | T1 | ephemeral | yes | E1 |
    // | T2 | durable injected | yes | E1 |
    // | T3 | durable injected | no | E2 |
    let ephemeral = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    assert!(!ephemeral.deployment.durable, "T1 precondition");
    ephemeral.ensure_dispatch_pool();
    assert!(ephemeral.dispatch.dispatch_pool.get().is_some(), "T1/E1");

    let local_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store(":memory:", system_clock())
            .await
            .unwrap(),
    );
    let local = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_dispatch_store(local_store),
    ));
    assert!(local.runs_local_dispatch_pool(), "local");
    local.ensure_dispatch_pool();
    assert!(local.dispatch.dispatch_pool.get().is_some(), "T2/E1");

    let coordinator_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store(":memory:", system_clock())
            .await
            .unwrap(),
    );
    let coordinator = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_coordinator_dispatch_store(coordinator_store),
    ));
    assert!(!coordinator.runs_local_dispatch_pool(), "coordinator");
    coordinator.ensure_dispatch_pool();
    assert!(coordinator.dispatch.dispatch_pool.get().is_none(), "T3/E2");
}

#[tokio::test]
async fn registered_dispatch_capacity_replaces_cpu_default_without_enabling_coordinator() {
    // Causes: C1 a local CPU-derived default; C2 registered manifest capacity is
    // lower/equal/higher than that default; C3 execution owner is Worker or
    // Coordinator. Effects: R1 local Host retains C1 and reports it as the
    // unregistered branch; R2 Worker installs exactly C2 slots (including I/O
    // concurrency above CPU quota) and reports it as the registered branch; R3
    // Coordinator owns no pool. Every capacity assertion observes the installed
    // pool or the installed sizing — never the manifest it came from.
    // The existing pool_memory concurrent-Run tests own slot/execution bounds;
    // this regression owns the production Host transport-to-capacity wiring.
    let default =
        std::thread::available_parallelism().unwrap_or(std::num::NonZeroUsize::new(4).unwrap());
    let local =
        SharedHost::from(crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await);
    assert_eq!(
        local.dispatch_pool_sizing(),
        crate::DispatchPoolSizing::LocalParallelism(default),
        "R1"
    );
    assert_eq!(local.dispatch_pool_sizing().registered(), None, "R1");
    for capacity in [1, default.get(), default.get() + 2] {
        // The only constructor of a `RegisteredCapacity` is the manifest this
        // Worker registers, so the installer cannot be handed a number the
        // registry never saw.
        let mut manifest = awaken_dispatch_contract::WorkerManifest::default();
        manifest.capacity.max_concurrent = capacity as u32;
        let registered = awaken_dispatch_contract::RegisteredCapacity::of(&manifest)
            .expect("R2 non-zero registered capacity");
        let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(system_clock()));
        let worker = Arc::new(SharedHost::from(
            crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_worker_dispatch(store, registered),
        ));
        worker.ensure_dispatch_pool();
        assert_eq!(
            worker.dispatch.dispatch_pool.get().unwrap().concurrency(),
            capacity,
            "R2 actual slots"
        );
        assert_eq!(
            worker.dispatch_pool_sizing(),
            crate::DispatchPoolSizing::Registered(
                std::num::NonZeroUsize::new(capacity).expect("R2")
            ),
            "R2 registered branch"
        );
        let _local_fence = worker.begin_pool_drain().await;
        let coordinator = Arc::new(SharedHost::from(
            crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_worker_dispatch(
                    Arc::new(awaken_dispatch::MemoryDispatchStore::new(system_clock())),
                    awaken_dispatch_contract::RegisteredCapacity::of(&manifest).expect("R3"),
                )
                .with_coordinator_dispatch_store(Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store(":memory:", system_clock())
                        .await
                        .unwrap(),
                )),
        ));
        coordinator.ensure_dispatch_pool();
        assert!(coordinator.dispatch.dispatch_pool.get().is_none(), "R3");
    }
    // A manifest that registers no capacity yields no installable registration:
    // the zero can never reach the installer as a silently clamped default.
    let mut unregisterable = awaken_dispatch_contract::WorkerManifest::default();
    unregisterable.capacity.max_concurrent = 0;
    assert!(
        awaken_dispatch_contract::RegisteredCapacity::of(&unregisterable).is_err(),
        "R2 zero capacity is not a registration"
    );
}

/// Restart authorization cause/effect graph: C1 the replacement realization
/// lease is absent/present. R1 !C1 rejects before provider or receipt I/O with a
/// retryable stale code; R2 C1 authorizes, persists once, then publishes. The
/// second attempt reloads current root authority instead of relabeling an effect
/// created under the rejected lease.
#[tokio::test]
async fn recovered_environment_waits_for_replacement_realization_before_publish() {
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: true,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let error = match host.ctx_for("binding-restart", None).await {
        Ok(_) => panic!("R1 missing realization reached provider I/O"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Unavailable, "R1");
    assert_eq!(error.code, "session_realization_stale", "R1");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1, "R1");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "R1");
    assert!(
        host.session_environment("binding-restart").await.is_none(),
        "R1"
    );
    host.install_session_realization_lease(
        "binding-restart",
        awaken_session_resource_contract::SessionRealizationLease {
            owner: "local-worker".into(),
            runtime_incarnation: "replacement".into(),
            epoch: 2,
            expires_at_unix_ms: u64::MAX,
        },
    )
    .unwrap();
    host.ctx_for("binding-restart", None).await.expect("R2");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2, "R2");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "R2");
    assert!(host.session_environment("binding-restart").await.is_some());
}

/// Multi-renewal FMECA: C1 no lease, C2 stale epoch 2, C3 accepted epoch 3.
/// E1 each stale attempt stops at the one pre-effect authorization; E2 only C3
/// reaches provider/receipt publication. The three attempts prove retryability
/// without reusing a substrate produced under an older epoch.
#[tokio::test]
async fn environment_binding_catches_up_across_multiple_realization_fences() {
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(MovingRealizationFenceSink {
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        accepted_epoch: AtomicU64::new(3),
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let first = match host.ctx_for("binding-moving-fence", None).await {
        Ok(_) => panic!("C1 reached provider I/O"),
        Err(error) => error,
    };
    assert_eq!(first.kind, HostErrorKind::Unavailable, "E1");
    for epoch in [2, 3] {
        host.install_session_realization_lease(
            "binding-moving-fence",
            awaken_session_resource_contract::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/incarnation".into(),
                epoch,
                expires_at_unix_ms: u64::MAX,
            },
        )
        .unwrap();
        let result = host.ctx_for("binding-moving-fence", None).await;
        if epoch == 2 {
            let error = match result {
                Ok(_) => panic!("C2 reached provider I/O"),
                Err(error) => error,
            };
            assert_eq!(error.kind, HostErrorKind::Unavailable, "E1");
        } else {
            result.expect("C3 commits");
        }
    }

    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 3, "E1/E2");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "E2");
    assert!(
        host.session_environment("binding-moving-fence")
            .await
            .is_some()
    );
}

/// Durable-binding decision table: no binding + no resident Environment permits
/// first creation; exact binding + adopted/resident permits reuse (covered by the
/// recovery E2E); exact binding + neither rejects ordinary execution before
/// provider creation, while Coordinator-only reservation and observation freeze
/// an environment-free context and leave adoption to the claimed Worker.
///
/// | Rule | durable binding | local adoption | context purpose | Effect |
/// | B1 | present | absent | execute | reject, no substitute |
/// | B2 | present | absent | reserve dispatch | env=None, preserve binding |
/// | B3 | present | absent | observe remote Run | env=None, preserve binding |
#[tokio::test]
async fn missing_durable_environment_adoption_is_deferred_only_for_coordination() {
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    host.install_session_environment_owner_projection(
        "binding-corrupt",
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "opaque".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .expect("project durable expectation");
    let error = match host.ctx_for("binding-corrupt", None).await {
        Ok(_) => panic!("missing adoption must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("was not adopted"), "B1");
    assert!(
        host.session_environment("binding-corrupt").await.is_none(),
        "B1"
    );

    let reserved = host
        .ctx_for_session_coordination("binding-corrupt", None)
        .await
        .expect("B2/B3 Coordinator-only projection does not adopt Worker substrate");
    assert!(reserved.env.is_none(), "B2/B3");
    assert_eq!(
        host.durable_session_environment_binding("binding-corrupt")
            .as_deref(),
        Some("opaque"),
        "B2/B3 durable identity is retained",
    );
    assert!(
        host.session_environment("binding-corrupt").await.is_none(),
        "B2/B3 no substitute or local adoption",
    );
}

#[tokio::test]
async fn binding_commit_failure_is_retryable_and_never_publishes_the_environment() {
    // Definite-failure decision table: C1 authorize succeeds; C2 persistence
    // fails before durable commit; C3 the caller retries. R1=C1+C2 retains one
    // hidden Candidate and returns Unavailable; R2=R1+C3 reuses that exact
    // Candidate, repeats only authorize/persist, and still publishes nothing.
    use awaken_session_execution_contract::SessionRuntime;
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host)
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());

    let error = match host.ctx_for("binding-failure", None).await {
        Ok(_) => panic!("binding failure must not publish a context"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("binding store unavailable"));
    assert_eq!(error.kind, HostErrorKind::Unavailable);
    let candidate = host
        .prepared_session_environment("binding-failure")
        .expect("R1 exact hidden Candidate");
    let retry = match host.ctx_for("binding-failure", None).await {
        Err(error) => error,
        Ok(_) => panic!("a definite failure must retry the retained Candidate"),
    };
    assert!(retry.to_string().contains("binding store unavailable"));
    let retried = host
        .prepared_session_environment("binding-failure")
        .expect("R2 retained Candidate");
    assert!(candidate.exact_matches(&retried), "R2 no provider recreate");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
    assert!(host.session_environment("binding-failure").await.is_none());
}

/// L3/L4: concurrent first Hand calls join one creator and only observe the
/// environment after its durable binding has succeeded.
#[tokio::test]
async fn on_tool_use_concurrent_hand_calls_create_and_persist_one_environment() {
    // Test design. Causes: C1 concurrent Hand calls race on one lazy Session.
    // Effects: E1 one creation wins; E2 all calls reuse the same persisted
    // Environment. Constraint/Invariant: Session realization has one CAS owner,
    // never one sandbox per caller. Decision rule: release concurrent C1 and
    // require creation cardinality one plus shared identity.
    use awaken_session_configuration_contract::SessionInit;
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    install_test_session_application(&host);
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = install_test_dispatch_runtime(&host);
    managed
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());
    managed
        .install_complete_test_session(
            "deferred-hand",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();
    let ctx = host.ctx_for("deferred-hand", None).await.unwrap();
    assert!(ctx.env.is_none());
    let hand = ctx
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("deferred hand")
        .clone();
    let left = awaken_tool_contract::tool::ToolCall {
        call_id: "read-left".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing-left"}),
    };
    let right = awaken_tool_contract::tool::ToolCall {
        call_id: "read-right".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing-right"}),
    };

    let (_left, _right) = tokio::join!(hand.invoke(&left), hand.invoke(&right));
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("deferred-hand").await.is_some());
}

/// L5: persistence failure fails closed; no deferred environment becomes visible.
#[tokio::test]
async fn on_tool_use_binding_failure_never_publishes_the_environment() {
    // Test design. Causes: C1 Environment binding fails after realization starts;
    // C2 the next tool call retries. Effects: E1 tool execution fails; E2 no
    // Environment is published as active; E3 C2 reuses the exact hidden Candidate.
    // Constraint/Invariant: publication follows complete binding and cannot expose
    // a partial sandbox. Rules L5=C1=>E1+E2; L6=L5+C2=>E1+E2+E3.
    use awaken_session_configuration_contract::SessionInit;
    let host = Arc::new(SharedHost::from(
        crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await,
    ));
    install_test_session_application(&host);
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = install_test_dispatch_runtime(&host);
    managed
        .realization()
        .ok_or_else(|| {
            awaken_session_types::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(sink.clone());
    managed
        .install_complete_test_session(
            "deferred-failure",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();
    let ctx = host.ctx_for("deferred-failure", None).await.unwrap();
    let hand = ctx
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("deferred hand")
        .clone();
    let call = awaken_tool_contract::tool::ToolCall {
        call_id: "read-failure".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing"}),
    };
    let error = hand.invoke(&call).await.expect_err("binding failure");
    assert!(error.to_string().contains("binding store unavailable"));
    let candidate = host
        .prepared_session_environment("deferred-failure")
        .expect("L5 hidden Candidate");
    let retry = hand
        .invoke(&call)
        .await
        .expect_err("the next tool attempt retries the retained Candidate");
    assert!(retry.to_string().contains("binding store unavailable"));
    let retried = host
        .prepared_session_environment("deferred-failure")
        .expect("L6 retained Candidate");
    assert!(candidate.exact_matches(&retried), "L6/E3");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
    assert!(host.session_environment("deferred-failure").await.is_none());
}
