use super::claimed_dispatch::adopt_bound_sandbox;
use super::test_support::{
    AdoptionModel, ToggleBindingSink, available_local_environment_binding, claim,
    complete_managed_test_host, deferred_environment, eager_environment, empty_frozen_projection,
    empty_frozen_projection_for_snapshot, frozen_projection_for_manifest, managed_test_host,
    prepare_deferred_session, test_activation, with_empty_session_resources,
};
use super::*;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn session_run_reservation_repair_skips_execution_realization() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Cause/effect graph: C0 the Session application supplies the current
    // immutable command fingerprint; C1 a complete self-affine Session Run
    // reservation expires; C2 the queue grants its dedicated recovery claim;
    // C3 no Session Environment has been realized. Effects: E1 the claim is
    // marked admission-only; E2 Host resolves the existing environment-free
    // boundary Worker; E3 no Session Environment or Sandbox side effect is
    // created. Decision rule R1=C0+C1+C2+C3=>E1+E2+E3. Missing/unknown command
    // fingerprints remain owned by the queue conformance table.
    use awaken_run_ingress::{Clock, DispatchQueue};

    let store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let session_id = "reservation-repair";
    let run_id = "reservation-repair-run";
    let activation = test_activation(session_id, run_id);
    let command = awaken_session_contract::AdmitSessionRun {
        session_id: session_id.into(),
        agent_id: activation.snapshot.root_agent_id.0.clone(),
        operation_id: "reservation-repair-operation".into(),
        run_id: activation.run_id.clone(),
        messages: activation.input.clone(),
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
    };
    let request = awaken_run_ingress::RunDispatch::new(activation)
        .for_session(awaken_agent_contract::agent::thread::Id(session_id.into()))
        .with_session_command_fingerprint(
            awaken_session_contract::SessionRunCommandFingerprint::current(&command),
        );
    assert_eq!(
        store
            .reserve_session_run(request, 1)
            .await
            .expect("R1 reserve"),
        awaken_run_ingress::SessionRunReservationOutcome::Reserved,
        "R1/C1"
    );
    // C1 is governed by the queue-owned clock. Wait past the relative TTL,
    // then sample the edge clock used by the existing execution-lease claim.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let repair_now = awaken_run_ingress::SystemClock.now_ms();
    let claimed = store
        .claim("repair-worker", 1_000, repair_now, &Default::default())
        .await
        .expect("R1 recovery claim")
        .expect("R1 expired reservation is claimable");
    assert!(claimed.session_activity_admission_required, "R1/E1");

    let storage = tempfile::tempdir().expect("storage");
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_store_dir(storage.path())
            .with_dispatch_store(store),
    );
    HostWorkerResolver {
        host: Arc::downgrade(&host),
    }
    .worker_for_claimed(&claimed)
    .await
    .expect("R1/E2 environment-free worker");
    assert!(
        host.session_environment(session_id).await.is_none(),
        "R1/E3"
    );
}

#[tokio::test]
async fn ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the local Runtime authority is ephemeral; C2 the
    // parent Session therefore uses direct foreground delivery; C3 an ordinary
    // child Run is claimed from that same authority's dispatch queue with
    // parent affinity. Effects: E1 resolution reuses the parent commit,
    // Environment, and attempt context; E2 it constructs the existing child
    // boundary Worker without requiring a second per-Session durable ingress.
    // The durable-parent sibling paths are covered by cold-worker recovery.
    //
    // | Rule | Authority | Parent delivery | Claimed Thread | Effect |
    // |---|---|---|---|---|
    // | E1 | ephemeral | direct | child != parent | E1+E2 |
    use awaken_run_ingress::{DispatchQueue as _, WorkerResolver as _};

    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let _managed = managed_test_host(host.clone());
    let parent = awaken_agent_contract::agent::thread::Id("ephemeral-parent".into());
    let child = awaken_agent_contract::agent::thread::Id("ephemeral-child".into());
    let run = awaken_agent_contract::agent::run::Id("ephemeral-child-run".into());
    let parent_ctx = host
        .ctx_for(&parent.0, None)
        .await
        .expect("E1 direct parent context");
    assert!(!parent_ctx.delivery.is_durable(), "E1 precondition");
    let store = host.dispatch_store().expect("ephemeral dispatch authority");
    store
        .enqueue(
            awaken_run_ingress::RunDispatch::new(test_activation(&child.0, &run.0))
                .for_session(parent.clone())
                .with_session_activity_epoch(7),
        )
        .await
        .expect("E1 child admission");
    let claimed = store
        .claim("ephemeral-worker", 1_000, 0, &Default::default())
        .await
        .expect("E1 claim")
        .expect("E1 child work");
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };

    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("E1/E2 child boundary resolution");
}

#[tokio::test]
async fn claimed_root_worker_is_independent_from_foreground_delivery_durability() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 foreground delivery is direct or durable; C2
    // this process is a local execution owner or coordinator-only; C3 a root
    // continuation has already been accepted into the dispatch queue.
    // Effects: E1 direct foreground remains non-durable; E2 a local direct
    // Session exposes and executes through its canonical claimed Worker; E3
    // durable foreground and claim resolution share the exact Worker `Arc`;
    // E4 coordinator-only resolution rejects before constructing a context.
    // Constraint: child-affinity construction is distinct and remains owned
    // by `ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker`.
    //
    // | Rule | Foreground | Local owner | Claimed root | Effect |
    // |---|---|---|---|---|
    // | R1 | direct | yes | yes | E1+E2 |
    // | R2 | durable | yes | no | E3 |
    // | R3 | durable | no | yes | E4 |
    use awaken_run_ingress::{Clock as _, DispatchQueue as _, WorkerResolver as _};

    let ephemeral = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let _ephemeral_managed = managed_test_host(ephemeral.clone());
    let session_id = "ephemeral-report-root";
    let direct = ephemeral
        .ctx_for(session_id, None)
        .await
        .expect("R1 direct context");
    assert!(!direct.delivery.is_durable(), "R1/E1");

    let child_run = RunId("reported-child-run".into());
    let report_id = MessageId::agent_thread_report(&child_run);
    ephemeral
        .continue_session_agent_report(awaken_session_contract::SessionAgentReportContinuation {
            session_id: session_id.into(),
            source_thread_id: ThreadId("reported-child-thread".into()),
            source_run_id: child_run,
            session_activity_epoch: 7,
            message: Message::text(report_id.clone(), Role::User, "child report"),
        })
        .await
        .expect("R1 queue root continuation");
    let ephemeral_store = ephemeral.dispatch_store().expect("R1 dispatch authority");
    let now = awaken_run_ingress::SystemClock.now_ms();
    let claimed = ephemeral_store
        .claim(
            ephemeral.dispatch_owner(),
            awaken_run_ingress::DEFAULT_LEASE_MS,
            now,
            &Default::default(),
        )
        .await
        .expect("R1 claim root continuation")
        .expect("R1 root continuation is runnable");
    let ephemeral_resolver = HostWorkerResolver {
        host: Arc::downgrade(&ephemeral),
    };
    let worker = ephemeral_resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("R1/E2 resolve direct Session claim");
    let resident = ephemeral
        .session_slots
        .read(session_id, |slot| slot.runtime.clone())
        .flatten()
        .expect("R1 resident Session context");
    assert!(Arc::ptr_eq(&worker, &resident.claimed_worker), "R1/E2");
    let (_, state) = worker
        .drive_claimed(claimed, Arc::new(awaken_run_ingress::ManualClock::new(now)))
        .await
        .expect("R1/E2 execute root continuation")
        .expect("R1/E2 root continuation settles");
    assert!(matches!(state, RunState::Ended(_)), "R1/E2");
    assert!(
        ephemeral
            .committed_messages(session_id)
            .await
            .expect("R1 committed transcript")
            .iter()
            .any(|message| message.id == report_id),
        "R1/E2 report reached the root Run"
    );

    let durable_store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("R2 dispatch store"),
    );
    let durable = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(durable_store),
    );
    let _durable_managed = managed_test_host(durable.clone());
    let durable_thread = ThreadId("durable-claimed-root".into());
    let durable_ctx = durable
        .ctx_for(&durable_thread.0, None)
        .await
        .expect("R2 durable context");
    assert!(durable_ctx.delivery.is_durable(), "R2 precondition");
    let durable_resolver = HostWorkerResolver {
        host: Arc::downgrade(&durable),
    };
    let resolved = durable_resolver
        .worker_for(&durable_thread, None)
        .await
        .expect("R2 resolve resident durable worker");
    assert!(
        Arc::ptr_eq(&resolved, &durable_ctx.claimed_worker),
        "R2/E3 resolver reuses the same Worker Arc"
    );

    let coordinator_store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("R3 dispatch store"),
    );
    let coordinator = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_coordinator_dispatch_store(coordinator_store.clone()),
    );
    let coordinator_thread = "coordinator-only-claimed-root";
    let coordinator_claim = claim(
        &coordinator_store,
        coordinator_thread,
        "coordinator-only-run",
        "remote-worker",
        now,
    )
    .await;
    let coordinator_resolver = HostWorkerResolver {
        host: Arc::downgrade(&coordinator),
    };
    let error = match coordinator_resolver
        .worker_for_claimed(&coordinator_claim)
        .await
    {
        Ok(_) => panic!("R3/E4 coordinator-only Host must not resolve locally"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("coordinator-only Host"),
        "R3/E4: {error}"
    );
    assert!(
        coordinator
            .session_slots
            .read(coordinator_thread, |slot| slot.runtime.is_none())
            .unwrap_or(true),
        "R3/E4 rejects before context construction"
    );
}

/// Cold-Worker delegation cause/effect decision table and FMECA. Causes:
/// C1 the Worker has no process-local publication catalog; C2 the claim
/// carries the exact child publication; C3 the bundle is absent; C4 the frozen
/// Runtime and empty Resource generation were installed through their canonical
/// projections. Effects: E1 parent context and `agent_run` admission are constructed from claim
/// truth; E2 C3 fails before inference. Rules D1=C1+C2+C4=>E1 and
/// D2=C1+C3=>E2. FMECA: dropping the bundle is high severity and previously
/// caused an endlessly retried Session; D2 turns it into a classified,
/// claim-settled setup failure while D1 proves no Control lookup is needed.
#[tokio::test]
async fn cold_worker_uses_only_the_claimed_delegation_publication_closure() {
    use awaken_run_ingress::{Clock, DispatchQueue};
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata,
    };

    let now = awaken_run_ingress::SystemClock.now_ms();
    let store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
    );
    let managed = managed_test_host(host.clone());
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };
    let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
        .model(ModelBinding::new("test", "model", "native"))
        .fingerprint("researcher-v2")
        .metadata(AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("researcher".into()),
                revision: 2,
            },
            publication_version: AgentPublicationVersion("researcher-v2".into()),
            resolution: Default::default(),
            fingerprint: AgentSnapshotFingerprint("researcher-v2".into()),
        })
        .build();
    let delegated = |thread: &str, run: &str| {
        let mut activation = test_activation(thread, run);
        activation.snapshot.resolved_spec.plugin_config.agent = AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: Some(2),
                recursive_self: false,
            }],
            ..Default::default()
        };
        activation
            .snapshot
            .recompute_fingerprint()
            .expect("coherent delegated root fixture");
        activation
    };
    let runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
        deferred_environment(),
        Some(Default::default()),
        Vec::new(),
    )
    .expect("encode complete ordinary Runtime projection");
    let complete_activation = delegated("cold-delegation", "run-cold-delegation");
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "cold-delegation",
        empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            deferred_environment(),
            &complete_activation.snapshot,
        ),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("D1 install complete cold Runtime projection");

    store
        .enqueue(with_empty_session_resources(
            awaken_run_ingress::RunDispatch::new(complete_activation)
                .with_agent_publications(vec![child])
                .with_session_runtime(runtime.clone()),
            host.local_workspace(),
        ))
        .await
        .expect("enqueue D1");
    let complete = store
        .claim("worker-a", 1_000, now, &Default::default())
        .await
        .expect("claim D1")
        .expect("D1 available");
    resolver
        .worker_for_claimed(&complete)
        .await
        .expect("D1/E1 claimed publication constructs delegation");

    let missing_store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("missing dispatch store"),
    );
    let missing_host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(missing_store.clone()),
    );
    let _missing_managed =
        crate::ManagedHost::new(missing_host.clone()).install_dispatch_session_runtime();
    let missing_resolver = HostWorkerResolver {
        host: Arc::downgrade(&missing_host),
    };
    missing_store
        .enqueue(with_empty_session_resources(
            awaken_run_ingress::RunDispatch::new(delegated(
                "cold-delegation-missing",
                "run-cold-delegation-missing",
            ))
            .with_session_runtime(runtime),
            missing_host.local_workspace(),
        ))
        .await
        .expect("enqueue D2");
    let missing = missing_store
        .claim("worker-a", 1_000, now, &Default::default())
        .await
        .expect("claim D2")
        .expect("D2 available");
    let error = match missing_resolver.worker_for_claimed(&missing).await {
        Ok(_) => panic!("D2/E2 missing closure must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("invalid Agent publication closure"),
        "D2/E2: {error}"
    );
}

#[tokio::test]
async fn claimed_root_cache_is_bound_to_the_exact_dispatch_publication_closure() {
    // Cause/effect graph: C0 the ordinary complete projection owns the baseline,
    // exact empty Resource transition, and frozen model route before warmth;
    // C1 an unclaimed warm context exists; C2 a claim
    // carries the same root but a different unversioned child publication;
    // C3 run/owner/lease epoch change while the publication closure and
    // effective model remain identical; C4 only the per-run effective model
    // changes. Effects: E1 C1+C2 rebuilds instead of inheriting the warm
    // catalog source; E2 the cache records exactly the frozen root/non-root
    // publications and effective fallback model captured by Runtime plugins;
    // E3 C3 reuses that Runtime; E4 C4 rebuilds it. Constraints: claim and
    // lease identity remain attempt context, while publication/model inputs
    // are Runtime construction inputs. Rules K1=C1+C2=>E1+E2,
    // K2=C3=>E3, K3=C4=>E4.
    use awaken_run_ingress::{Clock, DispatchQueue};
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};

    let child_v1 = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
        .instructions("catalog v1")
        .model(ModelBinding::new("test", "model", "native"))
        .fingerprint("researcher-v1")
        .build();
    let child_v2 = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
        .instructions("claimed v2")
        .model(ModelBinding::new("test", "model", "native"))
        .fingerprint("researcher-v2")
        .build();
    let mut activation = test_activation("claimed-cache", "run-claimed-cache");
    activation.snapshot.resolved_spec.plugin_config.agent = AgentBindings {
        delegates: vec![AgentDelegateBinding {
            agent_id: AgentId("researcher".into()),
            source_revision: None,
            recursive_self: false,
        }],
        ..Default::default()
    };
    activation.snapshot.resolved_spec.model_candidates = vec![
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
            "test",
            "alternate-model",
            "native",
        )),
    ];
    activation
        .snapshot
        .recompute_fingerprint()
        .expect("coherent root fixture");
    let root = activation.snapshot.clone();
    let catalog = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([child_v1])
        .expect("warm catalog source");
    let store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_dispatch_store(store.clone())
            .with_agent_publications(Arc::new(catalog)),
    );
    let managed = managed_test_host(host.clone());
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "claimed-cache",
        empty_frozen_projection_for_snapshot(host.local_workspace(), deferred_environment(), &root),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("K1 install complete warm projection");
    let warm = host
        .ctx_for_snapshot("claimed-cache", Some("agent-a"), Some(root))
        .await
        .expect("K1 warm unclaimed context");
    let request =
        awaken_run_ingress::RunDispatch::new(activation).with_agent_publications(vec![child_v2]);
    store.enqueue(request).await.expect("enqueue K1");
    let now = awaken_run_ingress::SystemClock.now_ms();
    let claimed = store
        .claim("worker-a", 30_000, now, &Default::default())
        .await
        .expect("claim K1")
        .expect("K1 available");
    let expected_identity = RuntimePublicationIdentity::from_publications(
        &claimed.request.activation.snapshot,
        &claimed.request.agent_publications,
        claimed.request.activation.effective_model_ref(),
    );
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };
    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("K1 claimed resolve");
    let claimed_ctx = host
        .session_slots
        .read("claimed-cache", |slot| slot.runtime.clone())
        .flatten()
        .expect("K1 claimed context resident");
    assert!(!Arc::ptr_eq(&warm, &claimed_ctx), "K1/E1");
    assert_eq!(
        claimed_ctx.runtime_publication_identity.as_ref(),
        Some(&expected_identity),
        "K1/E2"
    );
    let reclaimed = store
        .claim("worker-b", 30_000, now + 31_000, &Default::default())
        .await
        .expect("K2 reclaim")
        .expect("K2 expired claim available");
    resolver
        .worker_for_claimed(&reclaimed)
        .await
        .expect("K2 same publications under a different claim");
    let retried = host
        .session_slots
        .read("claimed-cache", |slot| slot.runtime.clone())
        .flatten()
        .expect("K2 context resident");
    assert!(Arc::ptr_eq(&claimed_ctx, &retried), "K2/E3");

    store
        .settle(
            &reclaimed.lease.run_id,
            reclaimed.lease.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle K2");
    let mut changed_model_request = reclaimed.request;
    changed_model_request.activation.run_id = RunId("run-claimed-cache-model".into());
    changed_model_request.activation.model_ref_override = Some("alternate-model".into());
    store
        .enqueue(changed_model_request)
        .await
        .expect("enqueue K3");
    let changed_model = store
        .claim("worker-c", 30_000, now + 32_000, &Default::default())
        .await
        .expect("claim K3")
        .expect("K3 dispatch available");
    resolver
        .worker_for_claimed(&changed_model)
        .await
        .expect("K3 effective-model change");
    let model_changed_ctx = host
        .session_slots
        .read("claimed-cache", |slot| slot.runtime.clone())
        .flatten()
        .expect("K3 context resident");
    assert!(!Arc::ptr_eq(&retried, &model_changed_ctx), "K3/E4");
}

/// Durable lazy placement decision table. Brain resolution stays sandbox-free;
/// root persistence precedes the raw dispatch cache; both stale-cache rejection
/// and response loss retain the one hidden Candidate for the next owner.
///
/// | Rule | Root persist | Dispatch claim/cache | Effect |
/// | D1 | not attempted | current | sandbox-free Brain worker |
/// | D2 | committed | stale | reject exposure; retain Candidate |
/// | D3 | already committed | replacement current | reuse Candidate; publish exact identity |
/// | D4 | committed, response lost | current/cache empty | return error; retain Candidate |
/// | D5 | already committed | replacement current | one idempotent Store-read; publish |
///
/// D3/D5 perform no second provider create/adopt. Two sink entries in D4+D5
/// represent one attempted mutation plus its authoritative readback replay.
#[tokio::test]
async fn durable_deferred_sandbox_publication_decision_table() {
    use awaken_run_ingress::{Clock, DispatchQueue};
    use awaken_session_contract::SessionRuntime;

    let now = awaken_run_ingress::SystemClock.now_ms();
    let storage = tempfile::tempdir().expect("isolated deferred Sandbox root");
    let store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_store_dir(storage.path())
            .with_dispatch_store(store.clone()),
    );
    let sink = Arc::new(ToggleBindingSink {
        fail: std::sync::atomic::AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        binding: std::sync::Mutex::new(None),
    });
    let managed = prepare_deferred_session(host.clone(), "durable-lazy").await;
    managed.install_environment_binding_sink(sink.clone());
    let claimed = claim(&store, "durable-lazy", "run-lazy", "worker-a", now).await;
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };
    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("D1 resolves Brain worker without Sandbox");
    assert!(
        host.session_environment("durable-lazy").await.is_none(),
        "D1"
    );

    let replacement = store
        .claim("worker-b", 1_000, now + 2_000, &Default::default())
        .await
        .expect("replacement claim")
        .expect("expired claim is recoverable");
    let deferred = host
        .session_slots
        .read("durable-lazy", |slot| slot.deferred_executor.clone())
        .flatten()
        .expect("deferred executor");
    let error = deferred
        .invoke(&awaken_runtime_contract::tool::ToolCall {
            call_id: "stale-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "missing"}),
        })
        .await
        .expect_err("D3 stale claim is fenced");
    assert!(
        error.to_string().contains("replacement claim"),
        "D3: {error}"
    );
    assert!(
        host.session_environment("durable-lazy").await.is_none(),
        "D2"
    );

    resolver
        .worker_for_claimed(&replacement)
        .await
        .expect("D3 replacement repairs the root-first Candidate");
    assert!(
        host.session_environment("durable-lazy").await.is_some(),
        "D3 exact Candidate published"
    );
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2, "D2+D3");

    let loss_store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("loss dispatch store"),
    );
    let loss_host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_store_dir(storage.path().join("response-loss"))
            .with_dispatch_store(loss_store.clone()),
    );
    let loss_sink = Arc::new(ToggleBindingSink {
        fail: std::sync::atomic::AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        binding: std::sync::Mutex::new(None),
    });
    let loss_managed = prepare_deferred_session(loss_host.clone(), "durable-loss").await;
    loss_managed.install_environment_binding_sink(loss_sink.clone());
    let loss_now = awaken_run_ingress::SystemClock.now_ms();
    let loss_claim = claim(
        &loss_store,
        "durable-loss",
        "run-durable-loss",
        "worker-loss-a",
        loss_now,
    )
    .await;
    let loss_resolver = HostWorkerResolver {
        host: Arc::downgrade(&loss_host),
    };
    loss_resolver
        .worker_for_claimed(&loss_claim)
        .await
        .expect("D4 sandbox-free loss worker");
    loss_sink.fail.store(true, Ordering::SeqCst);
    let loss_deferred = loss_host
        .session_slots
        .read("durable-loss", |slot| slot.deferred_executor.clone())
        .flatten()
        .expect("D4 deferred executor");
    let error = loss_deferred
        .invoke(&awaken_runtime_contract::tool::ToolCall {
            call_id: "crash-gap-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "missing"}),
        })
        .await
        .expect_err("D4 Session binding response loss");
    assert!(
        error
            .to_string()
            .contains("injected Session binding failure"),
        "D4: {error}"
    );
    assert!(
        loss_host
            .session_environment("durable-loss")
            .await
            .is_none(),
        "D4 hidden Candidate is not exposed"
    );
    let loss_retried = loss_store
        .claim(
            "worker-loss-b",
            1_000,
            loss_now + 2_000,
            &Default::default(),
        )
        .await
        .expect("D5 retry claim")
        .expect("D5 response-loss Run is recoverable");
    assert!(loss_retried.sandbox.is_none(), "D4 cache remains empty");
    loss_sink.fail.store(false, Ordering::SeqCst);
    loss_resolver
        .worker_for_claimed(&loss_retried)
        .await
        .expect("D5 exact Candidate readback repair");
    assert_eq!(
        loss_sink.calls.load(Ordering::SeqCst),
        2,
        "D4 attempted the root mutation once; D5 uses one idempotent Store-read replay"
    );
    assert!(
        loss_host
            .session_environment("durable-loss")
            .await
            .is_some(),
        "D5 exact Candidate published"
    );
}

struct RecordingSessionControl {
    resume_calls: Arc<AtomicUsize>,
    phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
    projection: Arc<std::sync::Mutex<Option<awaken_session_contract::FrozenSessionProjection>>>,
    mcp_stage: Option<awaken_session_contract::StageMcpAttachment>,
    fail_begin: Arc<AtomicBool>,
}

#[derive(Default)]
struct RecordingMcpRealizer {
    calls: std::sync::Mutex<Vec<&'static str>>,
    fail_stage: std::sync::atomic::AtomicBool,
    required_physical_environment: Option<(std::sync::Weak<SharedHost>, String)>,
}

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
    {
        self.calls.lock().unwrap().push("stage");
        if let Some((host, thread)) = &self.required_physical_environment {
            let physical_only = host.upgrade().is_some_and(|host| {
                host.session_slots
                    .read(thread, |slot| {
                        slot.environment_owner.is_resident() && slot.runtime.is_none()
                    })
                    .unwrap_or(false)
            });
            if !physical_only {
                return Err(awaken_session_contract::RunError::classified(
                    "test_session_physical_environment_missing",
                    "claimed MCP stage requires the physical Environment but no parent Runtime",
                ));
            }
        }
        if self.fail_stage.load(Ordering::SeqCst) {
            return Err(awaken_session_contract::RunError::classified(
                "test_mcp_stage_failed",
                "test MCP stage failed",
            ));
        }
        Ok(awaken_session_contract::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.calls.lock().unwrap().push("publish");
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.calls.lock().unwrap().push("drain");
        Ok(())
    }
}

#[tokio::test]
async fn cold_claimed_stdio_mcp_opens_the_exact_agent_snapshot_before_staging() {
    use awaken_run_ingress::{Clock, DispatchQueue};

    /* Claimed sandbox-stdio recovery cause/effect decision table. Causes:
     * C1 a cold Worker has no process-local Agent publication; C2 the durable
     * Run carries its exact immutable snapshot and non-default backend
     * projection; C3 its Session runtime envelope contains a sandbox-stdio
     * MCP stage; C4 the Session has no resident Runtime/Environment; C5 its
     * complete baseline/model and empty Resource transition are installed but
     * retain no Agent publication. Effects:
     * E1 open only the physical Session Environment from the claimed snapshot
     * before MCP staging; E2 stage and publish the exact MCP generation; E3
     * build the root Runtime once after publication, never as stage substrate;
     * E4 never consult or synthesize a second publication source. Rule S1:
     * C1+C2+C3+C4=>E1+E2+E3+E4. The adjacent malformed-envelope and stale-
     * claim rules retain fail-closed coverage for invalid authority. */
    let store = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
    );
    let thread = "cold-stdio-recovery";
    let realizer = Arc::new(RecordingMcpRealizer {
        required_physical_environment: Some((Arc::downgrade(&host), thread.into())),
        ..Default::default()
    });
    let managed = complete_managed_test_host(
        crate::ManagedHost::new(host.clone()).with_mcp_attachment_realizer(realizer.clone()),
    );
    let activation = test_activation(thread, "run-cold-stdio-recovery");
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            deferred_environment(),
            &activation.snapshot,
        ),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("S1 install complete cold Runtime coordinates");
    let stage = awaken_session_contract::StageMcpAttachment {
        workspace_id: host.local_workspace().into(),
        generation: awaken_session_contract::McpGenerationRef {
            session_id: thread.into(),
            attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
            generation: awaken_session_contract::McpGeneration(1),
            runtime_incarnation: "worker-a".into(),
            lease_epoch: 1,
            lease_expires_at_unix_ms: u64::MAX,
        },
        realization_id: "realize-browser-1".into(),
        stage_idempotency_key: "stage-browser-1".into(),
        name: "browser".into(),
        target: awaken_session_contract::McpTarget::sandbox_stdio(
            "playwright-mcp",
            vec!["--headless".into()],
        )
        .expect("sandbox stdio target"),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
        deferred_environment(),
        Some(Default::default()),
        vec![stage],
    )
    .expect("runtime envelope");
    store
        .enqueue(with_empty_session_resources(
            awaken_run_ingress::RunDispatch::new(activation).with_session_runtime(runtime),
            host.local_workspace(),
        ))
        .await
        .expect("enqueue");
    let claimed = store
        .claim(
            "worker-a",
            30_000,
            awaken_run_ingress::SystemClock.now_ms(),
            &Default::default(),
        )
        .await
        .expect("claim")
        .expect("claimed Run");

    HostWorkerResolver {
        host: Arc::downgrade(&host),
    }
    .worker_for_claimed(&claimed)
    .await
    .expect("S1 cold stdio recovery");

    assert_eq!(
        realizer.calls.lock().unwrap().as_slice(),
        ["stage", "publish"]
    );
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "S1/E3"
    );
}

#[async_trait::async_trait]
impl awaken_run_ingress_contract::ClaimedSessionControl for RecordingSessionControl {
    async fn resume_frozen(
        &self,
        claim: &awaken_run_ingress::RunClaim,
        _session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionRealizationDirective>,
        awaken_run_ingress_contract::ClaimedSessionControlError,
    > {
        self.resume_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.projection.lock().unwrap().clone().map(|projection| {
            awaken_session_contract::SessionRealizationDirective {
                projection,
                lease: awaken_session_contract::SessionRealizationLease {
                    owner: claim.owner.clone(),
                    runtime_incarnation: claim.owner.clone(),
                    epoch: claim.epoch,
                    expires_at_unix_ms: u64::MAX,
                },
                action: awaken_session_contract::SessionRealizationAction::Complete,
            }
        }))
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for RecordingSessionControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("begin");
        if self.fail_begin.load(Ordering::SeqCst) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::NotReady);
        }
        let projection = self
            .projection
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        let mut stage = self
            .mcp_stage
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        stage.generation.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
        stage.stage_idempotency_key = format!("renew:{}", command.target.lease_expires_at_unix_ms);
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection,
            lease: awaken_session_contract::SessionRealizationLease {
                owner: command.target.owner,
                runtime_incarnation: command.target.runtime_incarnation,
                epoch: 1,
                expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
            },
            action: awaken_session_contract::SessionRealizationAction::Stage {
                prepare_session: false,
                mcp_stages: vec![stage],
            },
        })
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("activate");
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection: self
                .projection
                .lock()
                .unwrap()
                .clone()
                .expect("frozen Session projection"),
            lease: command.lease,
            action: awaken_session_contract::SessionRealizationAction::Publish {
                publish: command
                    .mcp_receipts
                    .into_iter()
                    .map(|receipt| receipt.generation)
                    .collect(),
                drain: Vec::new(),
            },
        })
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("acknowledge");
        Ok(awaken_session_contract::SessionRealizationDirective {
            projection: self
                .projection
                .lock()
                .unwrap()
                .clone()
                .expect("frozen Session projection"),
            lease: command.lease,
            action: awaken_session_contract::SessionRealizationAction::Complete,
        })
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.phases.lock().unwrap().push("fail");
        Ok(())
    }
}

fn ordinary_frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "test.worker",
    );
    empty_frozen_projection(
        "workspace",
        awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env".into(),
            revision: awaken_session_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "env-fingerprint".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                inference_holder: holder.clone(),
                mcp_holder: holder.clone(),
                resource_holder: holder,
            },
        },
    )
}

/// Cause/effect graph: C1 an authenticated Run claim is current; C2 its
/// Session is already frozen; C3 the Worker has the claimed Session-control
/// client; C4 the Worker has the canonical Environment-effect binding sink.
/// C1+C2+C3+C4 cause E1 resume the canonical realization, E2 install the exact
/// baseline and lease, and E3 durably create the Worker environment.
/// C3 absent is the co-located local-pool row, covered by the cold legacy test.
///
/// | Rule | Claim | Frozen | Control+binding | Effect |
/// |---|---|---|---|---|
/// | O1 | live | yes | present | resume and realize frozen projection |
/// | O2 | stale | any | present | reject before realization |
/// | O3 | live | no | present | fail closed |
/// | O4 | live | local pre-realized | absent | local-pool replay |
/// | O5 | live Run without Session pointer | n/a | present | bypass Session control |
#[tokio::test]
async fn claimed_worker_uses_only_frozen_session_realization() {
    use awaken_run_ingress::{Clock, DispatchQueue};

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("in-memory dispatch"),
    );
    let resume_calls = Arc::new(AtomicUsize::new(0));
    let projection = Arc::new(std::sync::Mutex::new(Some(ordinary_frozen_projection())));
    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_dispatch_store(dispatch.clone())
            .with_session_control(Arc::new(RecordingSessionControl {
                resume_calls: resume_calls.clone(),
                phases: Arc::new(std::sync::Mutex::new(Vec::new())),
                projection,
                mcp_stage: None,
                fail_begin: Arc::new(AtomicBool::new(false)),
            })),
    );
    let managed = managed_test_host(host.clone());
    awaken_session_contract::SessionRuntime::install_environment_binding_sink(
        &managed,
        Arc::new(ToggleBindingSink {
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            binding: std::sync::Mutex::new(None),
        }),
    );
    dispatch
        .enqueue(
            awaken_run_ingress::RunDispatch::new(test_activation(
                "ordinary-remote",
                "run-ordinary-remote",
            ))
            .for_session(awaken_agent_contract::agent::thread::Id(
                "ordinary-remote".into(),
            ))
            .with_session_activity_epoch(1),
        )
        .await
        .expect("O1 enqueue");
    let claimed = dispatch
        .claim(
            "worker-a",
            30_000,
            awaken_run_ingress::SystemClock.now_ms(),
            &Default::default(),
        )
        .await
        .expect("O1 claim")
        .expect("O1 claimed Run");
    HostWorkerResolver {
        host: Arc::downgrade(&host),
    }
    .worker_for_claimed(&claimed)
    .await
    .expect("O1 canonical realization");

    assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O1/E1");
    assert!(
        host.session_environment("ordinary-remote").await.is_some(),
        "O1/E3"
    );
    assert!(
        host.session_slots
            .read("ordinary-remote", |slot| {
                slot.baseline.is_some()
                    && slot.realization_lease.as_ref().is_some_and(|lease| {
                        lease.owner == claimed.lease.owner && lease.epoch == claimed.lease.epoch
                    })
            })
            .unwrap_or(false),
        "O1/E1-E2"
    );

    let ordinary_activation = test_activation("ordinary-run", "run-without-session");
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "ordinary-run",
        empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            deferred_environment(),
            &ordinary_activation.snapshot,
        ),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("O5 install complete ordinary Runtime projection");
    dispatch
        .enqueue(with_empty_session_resources(
            awaken_run_ingress::RunDispatch::new(ordinary_activation),
            host.local_workspace(),
        ))
        .await
        .expect("O5 enqueue");
    let claimed = dispatch
        .claim(
            "worker-a",
            30_000,
            awaken_run_ingress::SystemClock.now_ms(),
            &Default::default(),
        )
        .await
        .expect("O5 claim")
        .expect("O5 claimed Run");
    HostWorkerResolver {
        host: Arc::downgrade(&host),
    }
    .worker_for_claimed(&claimed)
    .await
    .expect("O5 ordinary Run bypasses Session realization");
    assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O5");
}

#[tokio::test]
async fn resource_manifest_must_match_the_durable_execution_scope() {
    // Test design. Causes: C1 a claim's execution scope is workspace-b while
    // its frozen Resource manifest names workspace-a. Effects: E1 resolution
    // fails before sandbox creation; E2 no mismatched manifest is installed.
    // Constraint/Invariant: durable execution scope owns Resource tenancy.
    // Decision rule: exercise the unequal-scope partition and require E1/E2.
    let storage = tempfile::tempdir().expect("storage");
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let run = RunId("run-scope-mismatch".to_string());
    let request =
        awaken_run_ingress::RunDispatch::new(test_activation("thread-scope-mismatch", &run.0))
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from("workspace-b"),
            ))
            .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                "workspace-a",
                r#"{"inputs":[],"skills":[]}"#,
            ));
    let claimed = awaken_run_ingress::Claimed {
        request,
        lease: awaken_run_ingress::Lease {
            run_id: run,
            owner: "worker-a".to_string(),
            expires_ms: 100,
            epoch: 1,
        },
        credential_bindings: Vec::new(),
        cancellation_requested: false,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: None,
        assignment: None,
    };
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };

    let error = match resolver.worker_for_claimed(&claimed).await {
        Ok(_) => panic!("scope mismatch must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("outside its execution scope"));
    assert!(
        host.session_environment("thread-scope-mismatch")
            .await
            .is_none(),
        "scope rejection happens before sandbox creation"
    );
}

#[tokio::test]
async fn malformed_resource_manifest_is_rejected_before_sandbox_creation() {
    // Test design. Causes: C1 a scope-matched claim carries malformed manifest
    // JSON. Effects: E1 resolution rejects it; E2 sandbox creation and Resource
    // projection remain untouched. Constraint/Invariant: manifest validation
    // precedes every live effect. Decision rule: exercise malformed input and
    // require fail-closed zero creation.
    let storage = tempfile::tempdir().expect("storage");
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let run = RunId("run-malformed-resources".to_string());
    let request =
        awaken_run_ingress::RunDispatch::new(test_activation("thread-malformed-resources", &run.0))
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from("workspace-a"),
            ))
            .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                "workspace-a",
                "not-json",
            ));
    let claimed = awaken_run_ingress::Claimed {
        request,
        lease: awaken_run_ingress::Lease {
            run_id: run,
            owner: "worker-a".to_string(),
            expires_ms: 100,
            epoch: 1,
        },
        credential_bindings: Vec::new(),
        cancellation_requested: false,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: None,
        assignment: None,
    };
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };

    let error = match resolver.worker_for_claimed(&claimed).await {
        Ok(_) => panic!("malformed manifest must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("invalid Session resource manifest")
    );
    assert!(
        host.session_environment("thread-malformed-resources")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn live_file_replacement_requires_the_exact_resource_transition() {
    let storage = tempfile::tempdir().expect("storage");
    let mut raw_host =
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
    // Cause/effect decision table. Causes: C1 the Session is physically empty;
    // C2 the complete Dispatch projection supplies the exact Empty->desired
    // transition; C3 a following desired-only manifest is exact/different; C4
    // the aggregate supplies the exact previous->desired transition; C5 an
    // Environment is resident. Effects: E1 the canonical projection stages
    // cold requirements and exact desired-only replay does not publish an
    // active manifest; E2 physical create publishes generation 0; E3 a
    // desired-only replacement fails with no logical/physical change; E4 the
    // exact transition removes the File before publishing generation 2.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | F1 | T | yes | exact | n/a | F | E1 |
    // | F2 | T | yes | exact | yes | T | E2 |
    // | F3 | F | n/a | different | no | T | E3 |
    // | F4 | F | n/a | n/a | yes | T | E4 |
    // Workdir remains correctly ineligible for F1's immutability requirement.
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            storage.path().join("sandboxes"),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let host = Arc::new(raw_host);
    let managed = managed_test_host(host.clone());
    let bytes = b"frozen worker input".to_vec();
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            "workspace-a",
            "input.bin".into(),
            "application/octet-stream".into(),
            &bytes,
        )
        .await
        .expect("create file")
        .id;
    let manifest = awaken_session_contract::SessionResourceManifest::new(
        "workspace-a",
        awaken_session_contract::ResolvedSessionResources::try_new(
            vec![awaken_session_contract::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                source: awaken_session_contract::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                },
                mount_path: "/uploads/input.bin".to_string(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );

    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "thread-cold-resource",
        frozen_projection_for_manifest(
            ordinary_frozen_projection().baseline.environment,
            &manifest,
        ),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("F1 install the complete projection and proven Empty->desired transition");
    host.install_dispatched_resources("thread-cold-resource", &manifest, None)
        .await
        .expect("F1 revalidate the exact desired manifest without republishing");
    assert!(
        host.thread_resource_manifest("thread-cold-resource")
            .is_none(),
        "F1 desired-only staging is not completion evidence"
    );
    let activation = test_activation("thread-cold-resource", "run-cold-resource");
    host.ctx_for_snapshot(
        "thread-cold-resource",
        Some("agent-a"),
        Some(activation.snapshot),
    )
    .await
    .expect("open environment after resource install");

    let mount = host
        .sandbox_spec("thread-cold-resource")
        .mounts
        .into_iter()
        .find(|mount| mount.mount_id == file_id)
        .expect("frozen File mount");
    assert_eq!(mount.mount_path, "/mnt/session/uploads/uploads/input.bin");
    assert_eq!(
        mount.access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );
    let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = mount.source
    else {
        panic!("frozen File uses the binary-safe carried source")
    };
    assert_eq!(contents, bytes);
    assert_eq!(
        host.thread_resource_manifest("thread-cold-resource"),
        Some(manifest.clone())
    );

    let cleared = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        2,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    let error = host
        .install_dispatched_resources("thread-cold-resource", &cleared, None)
        .await
        .expect_err("F3 desired-only replacement must fail closed");
    assert_eq!(error.code, "session_resource_transition_conflict", "F3");
    assert_eq!(
        host.thread_resource_manifest("thread-cold-resource"),
        Some(manifest.clone()),
        "F3 retains the active manifest"
    );
    assert!(
        !host
            .session_environment("thread-cold-resource")
            .await
            .expect("F3 resident environment")
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .expect("F3 list live files")
            .is_empty(),
        "F3 does not remove the live File"
    );

    let transition =
        awaken_session_contract::SessionResourceTransition::new(manifest.clone(), cleared.clone())
            .expect("F4 exact aggregate transition");
    host.apply_dispatched_resource_transition("thread-cold-resource", &transition, None)
        .await
        .expect("F4 exact live replacement");
    assert!(
        host.session_environment("thread-cold-resource")
            .await
            .expect("resident environment")
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .expect("list live files")
            .is_empty(),
        "F4 removes the stale physical projection"
    );
    assert!(
        host.sandbox_spec("thread-cold-resource").mounts.is_empty(),
        "F4 publishes the empty staged projection"
    );
    assert_eq!(
        host.thread_resource_manifest("thread-cold-resource"),
        Some(cleared),
        "F4 publishes generation 2 only after realization"
    );
}

#[tokio::test]
async fn desired_only_dispatch_reuses_the_exact_projected_transition() {
    // Producer/consumer cause/effect table: C1 the complete-projection producer
    // has installed an exact rev1→rev1 or rev1→rev2 transition; C2 active is
    // absent or equals the transition's previous generation; C3 the carried
    // desired-only manifest is exact or differs only by revision. Effects: E1
    // the consumer reuses the existing transition without publishing desired
    // as active; E2 a mismatch rejects before changing any correlated Resource
    // projection.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | R1 | rev1→rev1 | absent | exact rev1 | E1, active absent |
    // | R2 | rev1→rev2 | active rev1 | exact rev2 | E1, active rev1 |
    // | R3 | rev1→rev1 | absent | rev2 | E2 |
    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let managed = managed_test_host(host.clone());
    let revision_one = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        1,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    let revision_two = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        2,
        awaken_session_contract::ResolvedSessionResources::default(),
    );

    for thread in ["exact-cold-transition", "mismatched-cold-transition"] {
        let mut projection = frozen_projection_for_manifest(deferred_environment(), &revision_one);
        projection.previous_resource_manifest = Some(revision_one.clone());
        awaken_session_contract::SessionRuntime::install_session_projection(
            &managed,
            thread,
            projection,
            awaken_session_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .unwrap_or_else(|error| panic!("{thread}: complete projection producer: {error}"));
        assert!(
            host.thread_resource_manifest(thread).is_none(),
            "{thread}: C2 active truth remains absent"
        );
    }
    let active_thread = "active-previous-transition";
    host.register_thread_resource_manifest(active_thread, revision_one.clone());
    let mut active_projection =
        frozen_projection_for_manifest(deferred_environment(), &revision_two);
    active_projection.previous_resource_manifest = Some(revision_one.clone());
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        active_thread,
        active_projection,
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R2 complete projection producer");

    let exact_before = host
        .session_slots
        .read("exact-cold-transition", |slot| {
            slot.resource_transition.clone()
        })
        .flatten()
        .expect("R1 exact transition projection");
    assert_eq!(exact_before.previous(), &revision_one, "R1/C1 previous");
    assert_eq!(exact_before.desired(), &revision_one, "R1/C1 desired");
    host.install_dispatched_resources("exact-cold-transition", &revision_one, None)
        .await
        .expect("R1/E1 exact desired-only consumer");
    assert_eq!(
        host.session_slots
            .read("exact-cold-transition", |slot| slot
                .resource_transition
                .clone())
            .flatten(),
        Some(exact_before),
        "R1/E1 retains the exact producer transition"
    );
    assert!(
        host.thread_resource_manifest("exact-cold-transition")
            .is_none(),
        "R1/E1 staging does not publish active truth"
    );

    let active_transition = host
        .session_slots
        .read(active_thread, |slot| slot.resource_transition.clone())
        .flatten()
        .expect("R2 exact transition projection");
    assert_eq!(
        active_transition.previous(),
        &revision_one,
        "R2/C1 previous"
    );
    assert_eq!(active_transition.desired(), &revision_two, "R2/C1 desired");
    host.install_dispatched_resources(active_thread, &revision_two, None)
        .await
        .expect("R2/E1 exact desired consumer with active previous");
    assert_eq!(
        host.session_slots
            .read(active_thread, |slot| slot.resource_transition.clone())
            .flatten(),
        Some(active_transition),
        "R2/E1 retains the exact producer transition"
    );
    assert_eq!(
        host.thread_resource_manifest(active_thread),
        Some(revision_one.clone()),
        "R2/E1 staging does not publish desired as active"
    );

    let mismatched_thread = "mismatched-cold-transition";
    let before_mismatch = host
        .session_slots
        .read(mismatched_thread, |slot| {
            (
                slot.resource_transition.clone(),
                slot.staged_resource_effect_key.clone(),
                slot.workspace.clone(),
                slot.manifest.clone(),
            )
        })
        .expect("R3 producer slot");
    let error = host
        .install_dispatched_resources(mismatched_thread, &revision_two, None)
        .await
        .expect_err("R3/E2 mismatched desired-only consumer");
    assert_eq!(error.code, "session_resource_transition_conflict", "R3/E2");
    assert_eq!(
        host.session_slots.read(mismatched_thread, |slot| {
            (
                slot.resource_transition.clone(),
                slot.staged_resource_effect_key.clone(),
                slot.workspace.clone(),
                slot.manifest.clone(),
            )
        }),
        Some(before_mismatch),
        "R3/E2 rejects without mutating correlated Resource state"
    );
}

/// Desired-only versus exact-transition cause/effect decision table.
/// Causes: C0 a complete Dispatch projection supplies the initial exact
/// Empty->desired transition; C1 an active process-local generation exists; C2
/// desired-only input is exact/different; C3 the aggregate supplies exact
/// previous+desired.
/// Effects: E1 exact input revalidates cold requirements without republishing;
/// E2 every desired-only change rejects and retains the active generation; E3
/// exact transition advances physical/logical state. Rules:
/// R0=C0+!C1+exact=>E1; R1=C1+exact=>E1; R2=C1+different+!C3=>E2;
/// R3=C1+different+C3=>E3. Revision ordering and Workspace alone never
/// substitute for the aggregate transition.
#[tokio::test]
async fn desired_only_worker_requires_an_exact_transition_for_every_change() {
    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let managed = managed_test_host(host.clone());
    let claim = awaken_run_ingress::RunClaim {
        run_id: RunId("run-generation-fence".into()),
        owner: "worker-generation-fence".into(),
        epoch: 1,
    };
    let revision_four = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        4,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "thread-generation-fence",
        frozen_projection_for_manifest(
            ordinary_frozen_projection().baseline.environment,
            &revision_four,
        ),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install the complete initial projection and exact transition");
    host.install_dispatched_resources("thread-generation-fence", &revision_four, None)
        .await
        .expect("revalidate the exact desired manifest without republishing");
    host.ctx_for_snapshot(
        "thread-generation-fence",
        Some("agent-a"),
        Some(test_activation("thread-generation-fence", "open-generation-fence").snapshot),
    )
    .await
    .expect("physically publish initial generation");

    host.install_dispatched_resources("thread-generation-fence", &revision_four, Some(&claim))
        .await
        .expect("R1 exact replay revalidation");

    for rejected in [
        awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            3,
            awaken_session_contract::ResolvedSessionResources::default(),
        ),
        awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            4,
            awaken_session_contract::ResolvedSessionResources::try_new(
                Vec::new(),
                vec![awaken_session_contract::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "different-same-generation".into(),
                    version: 1,
                    bundle_sha256: "sha256-different".into(),
                }],
            )
            .unwrap(),
        ),
        awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-b",
            5,
            awaken_session_contract::ResolvedSessionResources::default(),
        ),
    ] {
        let result = host
            .install_dispatched_resources("thread-generation-fence", &rejected, Some(&claim))
            .await;
        assert!(
            result.is_err(),
            "R2 rejects desired-only replacement: {rejected:?}"
        );
        assert_eq!(
            host.thread_resource_manifest("thread-generation-fence"),
            Some(revision_four.clone())
        );
    }

    let revision_five = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        5,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    host.install_dispatched_resources("thread-generation-fence", &revision_five, Some(&claim))
        .await
        .expect_err("R2 newer desired-only generation still lacks previous");
    let transition = awaken_session_contract::SessionResourceTransition::new(
        revision_four,
        revision_five.clone(),
    )
    .expect("R3 exact transition");
    host.apply_dispatched_resource_transition("thread-generation-fence", &transition, Some(&claim))
        .await
        .expect("R3 exact transition advances generation");
    assert_eq!(
        host.thread_resource_manifest("thread-generation-fence"),
        Some(revision_five),
        "R3 publishes only after exact physical transition"
    );
}

#[tokio::test]
async fn complete_projection_resource_guard_excludes_desired_only_staging() {
    // Lock cause/effect table. C1 the complete projection owner holds the
    // `resource_projection` suffix guard; C2 desired-only cold staging races;
    // C3 the guard is released. Effects: E1 C1+C2 blocks before requirements,
    // transition, or active-manifest publication; E2 C2+C3 stages the proven
    // Empty->desired command and still leaves active completion absent.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | L1 | T | T | F | E1 |
    // | L2 | F | T | T | E2 |
    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let thread = "resource-projection-guard";
    let manifest = awaken_session_contract::SessionResourceManifest::new(
        host.local_workspace(),
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    let resource_projection = host
        .session_slots
        .update(thread, |slot| slot.resource_projection.clone());
    let guard = resource_projection.lock().await;
    let mut staging = tokio::spawn({
        let host = host.clone();
        let manifest = manifest.clone();
        async move {
            host.install_dispatched_resources(thread, &manifest, None)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut staging)
            .await
            .is_err(),
        "L1 desired-only staging must wait for the complete projection guard"
    );
    assert!(
        host.session_slots
            .read(thread, |slot| {
                slot.resource_transition.is_none()
                    && slot.staged_resource_effect_key.is_none()
                    && slot.manifest.is_none()
            })
            .unwrap_or(false),
        "L1 no correlated Resource fact is published while the guard is held"
    );

    drop(guard);
    staging
        .await
        .expect("L2 staging task")
        .expect("L2 cold staging succeeds after guard release");
    assert!(
        host.session_slots
            .read(thread, |slot| {
                slot.resource_transition.is_some()
                    && slot.staged_resource_effect_key.is_some()
                    && slot.manifest.is_none()
            })
            .unwrap_or(false),
        "L2 cold requirements and command stage without active completion"
    );
}

#[tokio::test]
async fn exact_prevalidated_staging_rejects_foreign_active_and_defers_bound_adoption() {
    // Exact-staging cause/effect table. C1 resident active is previous/desired/
    // foreign; C2 no Environment is resident; C3 a current or prospective
    // durable binding exists; C4 desired File bytes are unavailable. Effects:
    // E1 foreign active rejects before any File read or slot write; E2 C2+C3
    // defers all compilation until adoption; E3 previous/desired without C3
    // may compile (covered by the live replacement and cold File tests).
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | P1 | foreign | any | any | T | E1 |
    // | P2 | none | T | T | T | E2 |
    // | P3 | previous/desired | any | F | F | E3 |
    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let missing_resources = awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::new("missing-file-binding"),
            source: awaken_session_contract::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from("missing-file"),
            },
            mount_path: "/missing.txt".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .expect("valid missing-File projection");
    let previous = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        1,
        Default::default(),
    );
    let desired = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        2,
        missing_resources,
    );
    let transition =
        awaken_session_contract::SessionResourceTransition::new(previous.clone(), desired.clone())
            .expect("exact transition");

    let foreign_thread = "foreign-active-resource";
    let foreign = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        9,
        Default::default(),
    );
    host.register_thread_resource_manifest(foreign_thread, foreign.clone());
    host.session_slots.update(foreign_thread, |slot| {
        slot.workspace = Some("sentinel-workspace".into());
        slot.staged_resource_effect_key = Some("sentinel-key".into());
    });
    let resource_projection = host
        .session_slots
        .update(foreign_thread, |slot| slot.resource_projection.clone());
    let _guard = resource_projection.lock().await;
    let error = host
        .stage_prevalidated_dispatched_resource_transition_under_resource_projection(
            foreign_thread,
            &transition,
            None,
            false,
        )
        .await
        .expect_err("P1 foreign active generation must fail closed");
    assert_eq!(error.code, "session_resource_transition_conflict", "P1");
    assert!(
        host.session_slots
            .read(foreign_thread, |slot| {
                slot.manifest.as_ref() == Some(&foreign)
                    && slot.workspace.as_deref() == Some("sentinel-workspace")
                    && slot.staged_resource_effect_key.as_deref() == Some("sentinel-key")
                    && slot.resources.mounts.is_empty()
            })
            .unwrap_or(false),
        "P1 rejects before compilation or any correlated slot write"
    );
    drop(_guard);

    let bound_thread = "prospective-bound-resource";
    let resource_projection = host
        .session_slots
        .update(bound_thread, |slot| slot.resource_projection.clone());
    let _guard = resource_projection.lock().await;
    host.stage_prevalidated_dispatched_resource_transition_under_resource_projection(
        bound_thread,
        &transition,
        None,
        true,
    )
    .await
    .expect("P2 prospective binding defers missing File compilation");
    assert!(
        host.session_slots
            .read(bound_thread, |slot| {
                slot.manifest.is_none()
                    && slot.resource_transition.is_none()
                    && slot.staged_resource_effect_key.is_none()
                    && slot.resources.mounts.is_empty()
            })
            .unwrap_or(false),
        "P2 adoption remains the first Resource effect edge"
    );
}

/// Cause/effect decision table for Worker-side File staging:
/// | Rule | Worker File source | exact claim | local File DB entry | Effect |
/// |---|---|---|---|---|
/// | W1 | remote adapter | present | absent | stage returned digest/bytes; later runtime validation does not reopen a local File database |
/// | W2 | remote adapter | absent | absent | fail closed; no mount |
#[tokio::test]
async fn cold_worker_file_staging_uses_only_the_claim_bound_source() {
    struct ClaimFileSource;

    #[async_trait::async_trait]
    impl crate::FileContentSource<awaken_run_ingress::RunClaim> for ClaimFileSource {
        async fn read(
            &self,
            workspace_id: &str,
            file_id: &str,
            _purpose: &awaken_resource_contract::FileReadPurpose,
            claim: Option<&awaken_run_ingress::RunClaim>,
        ) -> Result<
            Option<awaken_resource_contract::ResolvedFileContent>,
            awaken_resource_contract::FileContentSourceError,
        > {
            let Some(claim) = claim else {
                return Ok(None);
            };
            if workspace_id != "workspace-remote"
                || file_id != "file-remote"
                || claim.run_id.0 != "run-remote"
                || claim.owner != "worker-remote"
                || claim.epoch != 7
            {
                return Ok(None);
            }
            let bytes = b"remote immutable File".to_vec();
            Ok(Some(awaken_resource_contract::ResolvedFileContent {
                file_id: file_id.into(),
                content_id: awaken_resource_contract::content_id(&bytes),
                filename: "remote.txt".into(),
                media_type: "text/plain".into(),
                bytes,
            }))
        }
    }

    let host = Arc::new(
        SharedHost::new(Arc::new(AdoptionModel), "stub")
            .with_file_content_source(Arc::new(ClaimFileSource)),
    );
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let manifest = awaken_session_contract::SessionResourceManifest::new(
        "workspace-remote",
        awaken_session_contract::ResolvedSessionResources::try_new(
            vec![awaken_session_contract::ResolvedInput {
                binding_id: awaken_resource_contract::BindingId::new("remote-file-binding"),
                source: awaken_session_contract::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file-remote"),
                },
                mount_path: "/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );
    let claim = awaken_run_ingress::RunClaim {
        run_id: RunId("run-remote".into()),
        owner: "worker-remote".into(),
        epoch: 7,
    };

    host.install_dispatched_resources("remote-file-ok", &manifest, Some(&claim))
        .await
        .expect("W1");
    let mount = host
        .sandbox_spec("remote-file-ok")
        .mounts
        .into_iter()
        .find(|mount| mount.mount_id == "file-remote")
        .expect("W1 exact mount");
    assert!(
        matches!(
            mount.source,
            awaken_provisioning_contract::MountSource::InlineBytes { ref contents, .. }
                if contents == b"remote immutable File"
        ),
        "W1"
    );
    assert!(
        host.thread_resource_manifest("remote-file-ok").is_none(),
        "W1 desired-only staging does not publish active completion"
    );
    managed
        .validate_thread_resource_bindings("remote-file-ok")
        .await
        .expect("W1 has no redundant local File catalog check");

    let error = host
        .install_dispatched_resources("remote-file-no-claim", &manifest, None)
        .await
        .expect_err("W2");
    assert!(error.to_string().contains("not found"), "W2: {error}");
    assert!(
        host.sandbox_spec("remote-file-no-claim").mounts.is_empty(),
        "W2"
    );
}

#[tokio::test]
async fn sandbox_binding_is_validated_before_provider_adoption() {
    let storage = tempfile::tempdir().expect("storage");
    let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
    assert!(
        host.adopt_bound_session_environment(
            "thread-a",
            Some("not-json"),
            &host.session_provider,
            None,
            false,
        )
        .await
        .is_err()
    );

    let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
        "local", "thread-b",
    ))
    .unwrap();
    assert!(
        host.adopt_bound_session_environment(
            "thread-a",
            Some(&wrong),
            &host.session_provider,
            None,
            false,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn recovery_mode_controls_the_production_adoption_seam() {
    // Fixture rule: C0 an executable Session first owns the canonical Runtime
    // composition and exact empty Resource transition; E0 its durable handle
    // can then exercise continuity/rebuild adoption without a partial slot path.
    //
    // Adoption cause/effect decision table:
    // | rule | provider observation | recovery policy | resident | effect |
    // | A1 | Ready exact handle | continuity | absent | adopt exact binding |
    // | A2 | definitively unavailable | rebuild | absent | RebuildRequired |
    // | A3 | definitively unavailable | continuity | absent | fail, zero mutation |
    // The dead-resident companion test covers A2 with a resident Arc: only the
    // exact observed Arc is discarded before rebuild is projected. Parameter
    // aggregation must preserve these existing typed observations and effects.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-adoption";
    let first =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let projection = empty_frozen_projection(first.local_workspace(), eager_environment());
    let encoded = available_local_environment_binding(&first, thread, &projection).await;
    let handle: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(&encoded).expect("A1 exact ready handle");
    drop(first);

    let replacement =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let _replacement_managed = managed_test_host(replacement.clone());
    replacement
        .install_frozen_session_projection(thread, projection, None, true, None)
        .await
        .expect("A1 install exact consumer projection");
    let run_id = RunId("run-adoption".into());
    let adopted = adopt_bound_sandbox(
        &replacement,
        Some(&encoded),
        thread,
        &run_id,
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("continuity mode adopts the durable handle");
    assert_eq!(
        adopted,
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready
    );
    assert_eq!(
        replacement
            .session_environment_handle(thread)
            .await
            .expect("published adopted handle"),
        handle
    );

    let missing_thread = "thread-missing";
    let missing_projection = super::test_support::empty_frozen_projection(
        replacement.local_workspace(),
        super::test_support::eager_environment(),
    );
    let missing = super::test_support::unavailable_local_environment_binding(
        &replacement,
        missing_thread,
        &missing_projection,
    )
    .await;
    let adopted = adopt_bound_sandbox(
        &replacement,
        Some(&missing),
        missing_thread,
        &run_id,
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .expect("rebuild mode may replace a missing sandbox from committed truth");
    assert_eq!(
        adopted,
        crate::host::session::SessionEnvironmentAdoptionDisposition::RebuildRequired
    );
    assert!(
        adopt_bound_sandbox(
            &replacement,
            Some(&missing),
            missing_thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .is_err(),
        "continuity mode fails closed when the bound sandbox is gone"
    );
}

#[tokio::test]
async fn cold_adoption_uses_the_provider_owned_by_frozen_model_provisioning() {
    // Cause/effect graph: C1=HostExecutor/Provider model; C2=BackendOwned
    // model; C3=durable handle exists only below the selected provider root.
    // Effects: E1=adopt from configured Session root; E2=adopt from trusted
    // host-identity root; E3=missing selected root fails continuity.
    //
    // Decision table: A1 C1+C3(configured)=>E1 (covered by
    // recovery_mode_controls...); A2 C2+C3(trusted)=>E2; A3 either model
    // with its selected root absent=>E3 (covered by that test's missing row).
    let storage = tempfile::tempdir().expect("storage");
    let trusted_root = storage.path().join("trusted-local-sandboxes");
    let thread = "thread-backend-owned-adoption";
    let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        awaken_runtime_contract::resolved::ModelBinding::new("provider", "", "acp:codex"),
        awaken_runtime_contract::CredentialRef {
            id: "local".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "test-acp-adapter-v1",
        "test-acp-capability-v1",
        Default::default(),
    )
    .expect("A2 valid BackendOwned candidate");
    let provisioning = candidate.provisioning().clone();
    let mut activation = test_activation(thread, "fixture-backend-owned-adoption");
    activation.snapshot.resolved_spec.model_binding = candidate;

    let mut first = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
    first.backend_owned_session_provider =
        Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
    let first = Arc::new(first);
    let projection = empty_frozen_projection_for_snapshot(
        first.local_workspace(),
        eager_environment(),
        &activation.snapshot,
    );
    let encoded = available_local_environment_binding(&first, thread, &projection).await;
    let handle: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(&encoded).expect("A2 exact ready handle");
    drop(first);
    assert!(
        trusted_root.join(thread).exists(),
        "A2 trusted root owns the durable environment"
    );
    assert!(
        !storage.path().join("sandboxes").join(thread).exists(),
        "A2 ordinary provider root cannot satisfy this handle"
    );

    let mut replacement =
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
    replacement.backend_owned_session_provider =
        Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
    let replacement = Arc::new(replacement);
    let _replacement_managed = managed_test_host(replacement.clone());
    replacement
        .install_frozen_session_projection(thread, projection, None, true, None)
        .await
        .expect("A2 install exact consumer projection");
    let adopted = adopt_bound_sandbox(
        &replacement,
        Some(&encoded),
        thread,
        &RunId("run-backend-owned-adoption".into()),
        &provisioning,
        awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("A2 adopt from the provisioning-selected provider");
    assert_eq!(
        adopted,
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready,
        "A2"
    );
    assert_eq!(
        replacement
            .session_environment_handle(thread)
            .await
            .expect("A2 adopted"),
        handle,
        "A2"
    );
}

#[tokio::test]
async fn a_resident_environment_is_reused_without_a_second_adoption() {
    // R1: canonical Runtime composition + direct empty transition creates the
    // resident Environment; matching binding adoption reuses that exact owner.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-resident-adoption";
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let _managed = managed_test_host(host.clone());
    let ctx = host.ctx_for(thread, None).await.expect("resident session");
    let handle = ctx.env.as_ref().expect("eager environment").handle();
    let encoded = serde_json::to_string(&handle).unwrap();

    let adopted = adopt_bound_sandbox(
        &host,
        Some(&encoded),
        thread,
        &RunId("resident-run".into()),
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("resident handle is already adopted");

    assert_eq!(
        adopted,
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready,
        "the lifecycle owner publishes no duplicate wrapper"
    );
    assert_eq!(host.session_environment_handle(thread).await, Some(handle));
}

#[tokio::test]
async fn a_dead_resident_environment_fails_continuity_and_is_fenced_before_rebuild() {
    // R1/R2: a canonically created resident whose provider state disappears is
    // retained by continuity mode and discarded only by explicit rebuild.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-dead-resident";
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let _managed = managed_test_host(host.clone());
    let ctx = host.ctx_for(thread, None).await.expect("resident session");
    let handle = ctx.env.as_ref().expect("eager environment").handle();
    let encoded = serde_json::to_string(&handle).unwrap();
    std::fs::remove_dir_all(storage.path().join("sandboxes").join(thread))
        .expect("terminate local sandbox out of band");

    let run = RunId("dead-resident-run".into());
    assert!(
        adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &run,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .is_err(),
        "continuity never silently replaces a dead resident sandbox"
    );
    assert_eq!(
        host.session_environment_handle(thread).await,
        Some(handle.clone()),
        "a failed continuity check does not mutate the owner registry"
    );

    let adopted = adopt_bound_sandbox(
        &host,
        Some(&encoded),
        thread,
        &run,
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .expect("explicit rebuild may discard the dead resident environment");
    assert_eq!(
        adopted,
        crate::host::session::SessionEnvironmentAdoptionDisposition::RebuildRequired
    );
    assert!(host.session_environment(thread).await.is_none());
    assert!(
        !host
            .session_slots
            .read(thread, |slot| slot.runtime.is_some())
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn a_stale_binding_cannot_evict_a_different_resident_environment() {
    // R1: canonical resident + stale full-handle binding => reject adoption and
    // retain both the exact Environment owner and Runtime.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-binding-fence";
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let _managed = managed_test_host(host.clone());
    let ctx = host.ctx_for(thread, None).await.expect("resident session");
    let resident = ctx.env.as_ref().expect("eager environment").handle();
    let stale = awaken_provisioning_contract::SandboxHandle::new(
        resident.provider_kind(),
        resident.sandbox_id.clone(),
    );
    let encoded = serde_json::to_string(&stale).unwrap();

    assert!(
        adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &RunId("stale-binding-run".into()),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .is_err(),
        "rebuild mode cannot override the full-handle fence"
    );
    assert_eq!(
        host.session_environment_handle(thread).await,
        Some(resident)
    );
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.is_some())
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn an_aba_replacement_with_the_same_handle_survives_stale_discard() {
    // Cause/effect rule: C1 canonical V2 creation and first adoption publish
    // owner A; C2 spec-aware re-adoption yields owner B with the same handle;
    // C3 stale discard still names A. E1 exact Arc identity fences C3 and B
    // remains Resident. A legacy V1 fixture cannot prove C2 and is excluded.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-environment-aba";
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let _managed = managed_test_host(host.clone());
    let projection = empty_frozen_projection(host.local_workspace(), eager_environment());
    let binding = available_local_environment_binding(&host, thread, &projection).await;
    adopt_bound_sandbox(
        &host,
        Some(&binding),
        thread,
        &RunId("aba-fixture-run".into()),
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("C1 publish first exact owner");
    let observed = host
        .session_environment(thread)
        .await
        .expect("observed owner");
    let spec = host.sandbox_spec_for_resolved_resources_and_provider(
        thread,
        &projection.resources,
        &host.session_provider,
    );
    let spec = host
        .validate_session_environment_capabilities(&host.session_provider, &spec)
        .expect("C2 exact adoption spec");
    let replacement = Arc::new(
        host.session_provider
            .adopt(&spec, &observed.handle())
            .await
            .expect("same-handle replacement"),
    );
    assert_eq!(observed.handle(), replacement.handle());
    assert!(!Arc::ptr_eq(&observed, &replacement));

    host.install_test_resident_session_environment(thread, replacement.clone());

    assert!(
        !host.discard_session_environment(thread, &observed).await,
        "object identity fences a stale observer even when the handle is reused"
    );
    let current = host
        .session_environment(thread)
        .await
        .expect("replacement kept");
    assert!(Arc::ptr_eq(&current, &replacement));
}

#[tokio::test]
async fn cancellation_resolution_does_not_touch_an_invalid_sandbox_binding() {
    // Test design. Causes: C1 a cancellation-only claim carries an invalid
    // opaque sandbox binding. Effects: E1 cancellation resolves without
    // parsing/adopting that binding; E2 no sandbox side effect occurs.
    // Constraint/Invariant: terminal control does not require execution
    // realization. Decision rule: exercise C1 and require control-only success.
    let storage = tempfile::tempdir().expect("storage");
    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    let thread = ThreadId("thread-control-only".to_string());
    let run = RunId("run-control-only".to_string());
    let fingerprint = CatalogFingerprint("control-only-catalog".to_string());
    let activation = RunActivation::new(
        run.clone(),
        thread.clone(),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("control-only-snapshot".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("control-only-agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: "test".to_string(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        Vec::new(),
    );
    let claimed = awaken_run_ingress::Claimed {
        request: awaken_run_ingress::RunDispatch::new(activation),
        lease: awaken_run_ingress::Lease {
            run_id: run,
            owner: "control-owner".to_string(),
            expires_ms: 100,
            epoch: 2,
        },
        credential_bindings: Vec::new(),
        cancellation_requested: true,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: Some("this is deliberately not a sandbox handle".to_string()),
        assignment: None,
    };
    let resolver = HostWorkerResolver {
        host: Arc::downgrade(&host),
    };

    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("terminal control bypasses sandbox decoding/adoption");
    assert!(host.session_environment(&thread.0).await.is_none());
    assert!(
        !host
            .session_slots
            .read(&thread.0, |slot| slot.runtime.is_some())
            .unwrap_or(false)
    );
}
