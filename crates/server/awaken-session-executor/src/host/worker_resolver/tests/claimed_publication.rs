//! Claimed root Workers: delivery independence and the exact frozen publication
//! closure a cold or cached Worker executes from.

use super::*;

#[tokio::test]
async fn claimed_root_worker_is_independent_from_foreground_delivery_durability() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 foreground delivery is direct or durable; C2
    // this process is a local execution owner or coordinator-only; C3 a root
    // continuation has already been accepted into the dispatch queue; C4 the
    // complete co-located Session projection precedes both context and claim;
    // C5 the explicit fresh SQLite Root and current assignment are installed
    // through the configured production binding sink before first Environment
    // I/O. Direct foreground delivery does not select an Unowned grant.
    // Effects: E1 direct foreground remains non-durable; E2 a local direct
    // Session exposes and executes through its canonical claimed Worker; E3
    // durable foreground and claim resolution share the exact Worker `Arc`;
    // E4 coordinator-only resolution rejects before constructing a context.
    // Constraint: child-affinity construction is distinct and remains owned
    // by `ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker`.
    //
    // | Rule | Foreground | Local owner | Claimed root | Effect |
    // |---|---|---|---|---|
    // | R1 | direct | yes | yes, C4+C5 | E1+E2 |
    // | R2 | durable | yes | no | E3 |
    // | R3 | durable | no | yes | E4 |
    use awaken_dispatch::WorkerResolver as _;

    let root_fixture = ManagedSessionRootFixture::new().await;
    let ephemeral = root_fixture
        .configure(adoption_dependencies().await)
        .build()
        .expect("R1 validated Root-backed Host dependencies");
    let ephemeral_managed = managed_test_host(ephemeral.clone());
    let session_id = "ephemeral-report-root";
    root_fixture
        .install_projection(
            &ephemeral_managed,
            session_id,
            empty_frozen_projection(
                ephemeral.run_assembly.local_workspace(),
                eager_environment(),
            ),
        )
        .await
        .expect("R1 install complete co-located Session projection");
    let direct = ephemeral
        .run_assembly
        .ctx_for(session_id, None)
        .await
        .expect("R1 direct context");
    assert!(!direct.delivery.is_durable(), "R1/E1");

    let child_run = RunId("reported-child-run".into());
    let report_id = MessageId::agent_thread_report(&child_run);
    ephemeral
        .coordination
        .continue_session_agent_report(
            awaken_session_execution_contract::SessionAgentReportContinuation {
                session_id: session_id.into(),
                source_thread_id: ThreadId("reported-child-thread".into()),
                source_run_id: child_run,
                session_activity_epoch: 7,
                message: Message::text(report_id.clone(), Role::User, "child report"),
            },
        )
        .await
        .expect("R1 queue root continuation");
    let ephemeral_store = ephemeral
        .run_assembly
        .authority
        .dispatch_binding()
        .map(|binding| binding.queue())
        .expect("R1 dispatch authority");
    let now = awaken_time::system_clock().now().as_epoch_ms();
    let claimed = ephemeral_store
        .claim(
            ephemeral.run_assembly.dispatch_owner(),
            awaken_dispatch_execution::DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("R1 claim root continuation")
        .expect("R1 root continuation is runnable");
    let ephemeral_resolver =
        crate::host::session_worker_resolver(&ephemeral.run_assembly, &ephemeral.dispatch)
            .expect("Host dispatch resolver");
    let worker = ephemeral_resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("R1/E2 resolve direct Session claim");
    let resident = ephemeral
        .run_assembly
        .installed_sessions
        .read_run(session_id, |run| run.context.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("R1 resident Session context");
    assert!(Arc::ptr_eq(&worker, &resident.claimed_worker), "R1/E2");
    let (_, state) = worker
        .drive_claimed(
            claimed,
            Arc::new(awaken_lease_testkit::ManualClock::new(now)),
        )
        .await
        .expect("R1/E2 execute root continuation")
        .expect("R1/E2 root continuation settles");
    assert!(matches!(state, RunState::Ended(_)), "R1/E2");
    assert!(
        ephemeral
            .thread_reads
            .committed_messages(session_id)
            .await
            .expect("R1 committed transcript")
            .iter()
            .any(|message| message.id == report_id),
        "R1/E2 report reached the root Run"
    );

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let durable_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("R2 dispatch store"),
    );
    let durable = adoption_host_over(durable_store).await;
    let _durable_managed = managed_test_host(durable.clone());
    let durable_thread = ThreadId("durable-claimed-root".into());
    let durable_ctx = durable
        .run_assembly
        .ctx_for(&durable_thread.0, None)
        .await
        .expect("R2 durable context");
    assert!(durable_ctx.delivery.is_durable(), "R2 precondition");
    let durable_resolver =
        crate::host::session_worker_resolver(&durable.run_assembly, &durable.dispatch)
            .expect("Host dispatch resolver");
    let resolved = durable_resolver
        .worker_for(&durable_thread, None)
        .await
        .expect("R2 resolve resident durable worker");
    assert!(
        Arc::ptr_eq(&resolved, &durable_ctx.claimed_worker),
        "R2/E3 resolver reuses the same Worker Arc"
    );

    let coordinator_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("R3 dispatch store"),
    );
    let coordinator = (adoption_dependencies()
        .await
        .with_ephemeral_coordinator_dispatch(coordinator_store.clone()))
    .build()
    .expect("validated Host dependencies");
    let coordinator_thread = "coordinator-only-claimed-root";
    let coordinator_claim = claim(
        &coordinator_store,
        coordinator_thread,
        "coordinator-only-run",
        "remote-worker",
        now,
        &clock,
    )
    .await;
    let coordinator_resolver =
        crate::host::session_worker_resolver(&coordinator.run_assembly, &coordinator.dispatch)
            .expect("Host dispatch resolver");
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
            .run_assembly
            .installed_sessions
            .read_run(coordinator_thread, |run| run.context.is_none())
            .expect("installed Sessions")
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
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata,
    };

    let now = awaken_time::system_clock().now().as_epoch_ms();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    // D1 receiver stays Managed: the existing real Root owns its live lease
    // and first Create CAS; ordinary Queue Work carries the returned Resource
    // generation. It still supplies the only Agent/delegation publication.
    let root_fixture = ManagedSessionRootFixture::new().await;
    let host = root_fixture
        .configure(
            adoption_dependencies()
                .await
                .with_ephemeral_dispatch(store.clone()),
        )
        .build()
        .expect("validated Root-backed Host dependencies");
    let managed = managed_test_host(host.clone());
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");
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
    let runtime = ingress::SessionRuntimeEnvelope::new(
        awaken_session_configuration_contract::SessionRuntimeProjection {
            environment: deferred_environment(),
            tools: Some(Default::default()),
            mcp_stages: Some(Vec::new()),
        }
        .encode()
        .expect("encode complete ordinary Runtime projection"),
    );
    let complete_activation = delegated("cold-delegation", "run-cold-delegation");
    let projection = root_fixture
        .install_projection(
            &managed,
            "cold-delegation",
            empty_frozen_projection_for_snapshot(
                host.run_assembly.local_workspace(),
                deferred_environment(),
                &complete_activation.snapshot,
            ),
        )
        .await
        .expect("D1 install complete cold Runtime projection");
    let transition = projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )
        .expect("D1 exact Store-read initial Resource transition");

    store
        .enqueue(
            ingress::RunDispatch::new(complete_activation)
                .with_agent_publications(vec![child])
                .with_session_runtime(runtime.clone())
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(projection.workspace_id.as_str()),
                ))
                .with_session_resources(
                    awaken_dispatch_execution::session_envelope::resource_envelope(
                        transition.desired(),
                    )
                    .expect("D1 carry the Root's prepared Resource generation"),
                ),
        )
        .await
        .expect("enqueue D1");
    clock.set(now);
    let complete = store
        .claim("worker-a", 1_000, &Default::default())
        .await
        .expect("claim D1")
        .expect("D1 available");
    resolver
        .worker_for_claimed(&complete)
        .await
        .expect("D1/E1 claimed publication constructs delegation");

    let missing_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("missing dispatch store"),
    );
    let missing_host = adoption_host_over(missing_store.clone()).await;
    let _missing_managed = missing_host.clone();
    let missing_resolver =
        crate::host::session_worker_resolver(&missing_host.run_assembly, &missing_host.dispatch)
            .expect("Host dispatch resolver");
    missing_store
        .enqueue(with_empty_session_resources(
            ingress::RunDispatch::new(delegated(
                "cold-delegation-missing",
                "run-cold-delegation-missing",
            ))
            .with_session_runtime(runtime),
            missing_host.run_assembly.local_workspace(),
        ))
        .await
        .expect("enqueue D2");
    clock.set(now);
    let missing = missing_store
        .claim("worker-a", 1_000, &Default::default())
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
    // Cause/effect graph: C0 the explicit fresh Managed Root owns the baseline,
    // exact initial Resource transition, current assignment, and frozen model
    // route before warmth. Its production binding sink is configured before
    // Host construction; the returned desired Resource revision is carried by
    // the real Queue instead of assuming the pre-Create fixture's revision 0;
    // C1 an unclaimed warm context exists; C2 a claim
    // carries the same root but a different unversioned child publication;
    // C3 run/owner/lease epoch change while the publication closure and
    // effective model remain identical; C4 only the per-run effective model
    // changes. Effects: E1 C1+C2 rebuilds instead of inheriting the warm
    // catalog source; E2 the cache records exactly the frozen root/non-root
    // publications and effective fallback model captured by Runtime plugins;
    // E3 C3 reuses that Runtime; E4 C4 rebuilds it. Constraints: claim and
    // lease identity remain attempt context, while publication/model inputs
    // are Runtime construction inputs. Rules K1=C0+C1+C2=>E1+E2,
    // K2=C0+C3=>E3, K3=C0+C4=>E4.
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
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    let root_fixture = ManagedSessionRootFixture::new().await;
    let host = root_fixture
        .configure(
            adoption_dependencies()
                .await
                .with_ephemeral_dispatch(store.clone())
                .with_agent_publications(Arc::new(catalog)),
        )
        .build()
        .expect("validated Root-backed Host dependencies");
    let managed = managed_test_host(host.clone());
    let mut projection = empty_frozen_projection_for_snapshot(
        host.run_assembly.local_workspace(),
        deferred_environment(),
        &root,
    );
    projection.agent_publication = Some(root.clone());
    let projection = root_fixture
        .install_projection(&managed, "claimed-cache", projection)
        .await
        .expect("K1 install complete warm projection");
    let warm = host
        .run_assembly
        .ctx_for_snapshot("claimed-cache", Some("agent-a"), Some(root))
        .await
        .expect("K1 warm unclaimed context");
    let transition = projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )
        .expect("K1 exact Store-read initial Resource transition");
    let request = ingress::RunDispatch::new(activation)
        .with_agent_publications(vec![child_v2])
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(projection.workspace_id.as_str()),
        ))
        .with_session_resources(
            awaken_dispatch_execution::session_envelope::resource_envelope(transition.desired())
                .expect("K1 carry the exact Root's prepared Resource generation"),
        );
    store.enqueue(request).await.expect("enqueue K1");
    let now = awaken_time::system_clock().now().as_epoch_ms();
    clock.set(now);
    let claimed = store
        .claim("worker-a", 30_000, &Default::default())
        .await
        .expect("claim K1")
        .expect("K1 available");
    let expected_identity = RuntimePublicationIdentity::from_publications(
        &claimed.request.activation.snapshot,
        &claimed.request.agent_publications,
        claimed.request.activation.effective_model_ref(),
    );
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");
    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("K1 claimed resolve");
    let claimed_ctx = host
        .run_assembly
        .installed_sessions
        .read_run("claimed-cache", |run| run.context.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("K1 claimed context resident");
    assert!(!Arc::ptr_eq(&warm, &claimed_ctx), "K1/E1");
    assert_eq!(
        claimed_ctx.runtime_publication_identity.as_ref(),
        Some(&expected_identity),
        "K1/E2"
    );
    clock.set(now + 31_000);
    let reclaimed = store
        .claim("worker-b", 30_000, &Default::default())
        .await
        .expect("K2 reclaim")
        .expect("K2 expired claim available");
    resolver
        .worker_for_claimed(&reclaimed)
        .await
        .expect("K2 same publications under a different claim");
    let retried = host
        .run_assembly
        .installed_sessions
        .read_run("claimed-cache", |run| run.context.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("K2 context resident");
    assert!(Arc::ptr_eq(&claimed_ctx, &retried), "K2/E3");

    store
        .settle(
            reclaimed.lease.run_id(),
            reclaimed.lease.epoch(),
            ingress::DispatchOutcome::Done,
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
    clock.set(now + 32_000);
    let changed_model = store
        .claim("worker-c", 30_000, &Default::default())
        .await
        .expect("claim K3")
        .expect("K3 dispatch available");
    resolver
        .worker_for_claimed(&changed_model)
        .await
        .expect("K3 effective-model change");
    let model_changed_ctx = host
        .run_assembly
        .installed_sessions
        .read_run("claimed-cache", |run| run.context.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("K3 context resident");
    assert!(!Arc::ptr_eq(&retried, &model_changed_ctx), "K3/E4");
}
