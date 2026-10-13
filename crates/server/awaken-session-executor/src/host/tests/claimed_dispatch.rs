// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Claimed snapshots, durable dispatch freezing, inference holders, and sandbox adoption.

use super::*;

#[tokio::test]
async fn ctx_for_carries_one_self_consistent_snapshot_on_any_claiming_node() {
    use awaken_runtime_contract::resolver::RunResolver;

    // A durable run is driven by whichever pool node claims it (ADR-0019). Its
    // session config is already the complete execution authority and resolves
    // without manufacturing a second node-local catalog object.
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    let ctx = host
        .ctx_for("t-catalog", None)
        .await
        .expect("session builds");
    let resolved = ctx.runtime.resolve(&ctx.config).expect("snapshot resolves");
    assert_eq!(resolved.snapshot_id, ctx.config.id);
}

#[tokio::test]
async fn claimed_inference_only_grader_keeps_execution_authority_without_a_sandbox() {
    // J4 causes: a claim-fenced Native toolless snapshot, explicit Workspace,
    // and no declared Session/Environment/Resources. Effects: one execution
    // context carries the exact publication and Scope, but no physical Sandbox.
    // This is the Worker half of the Judge proof; using CoordinationEnvelope
    // instead would improperly drop execution authority to avoid realization.
    let published = awaken_ext_goal::grader::default_judge_agent(
        "stub",
        "claim-judge",
        awaken_ext_goal::grader::DEFAULT_JUDGE_INSTRUCTIONS,
    )
    .expect("literal model reference");
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    let thread = "claim-inference-only-judge";
    host.register_thread_workspace(thread, "workspace-judge");
    let identity = RuntimePublicationIdentity::from_publications(&published, &[], "stub");
    let publications = Arc::new(
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([published.clone()])
            .unwrap(),
    );
    let context = host
        .ctx_for_claimed_snapshot(
            thread,
            Some("claim-judge"),
            published,
            ClaimedRuntimeInput {
                identity: identity.clone(),
                publications,
                effective_model_ref: "stub".into(),
            },
        )
        .await
        .expect("claim-fenced inference context");
    assert_eq!(
        context.purpose,
        crate::host::session_ctx::RuntimeContextPurpose::Execution
    );
    assert_eq!(
        context.runtime_publication_identity.as_ref(),
        Some(&identity)
    );
    assert!(host.realizer.session_environment(thread).await.is_none());
    assert_eq!(
        context
            .context()
            .execution_scope
            .as_ref()
            .map(|scope| scope.0.as_str()),
        Some("workspace-judge")
    );
}

#[tokio::test]
async fn claimed_snapshot_is_the_worker_session_authority() {
    // P1 exact claimed publication + identical Session projection + no
    // semantic override => preserve the COMPLETE snapshot and its source
    // fingerprint. P2 different node defaults cannot rehash or replace it.
    // Changed overlays own their positive witnesses in the canonical projector
    // table; an unconditional expected.recompute_fingerprint hid this no-op law.
    let published = awaken_runtime_contract::ExecutableAgentSnapshot::builder("published-agent")
        .model(test_model_binding())
        .instructions("published instructions")
        .fingerprint("sha256:published")
        .plugin_config([(
            "permission".to_string(),
            serde_json::json!({"default": "deny", "rules": []}),
        )])
        .build();
    let roots = ManagedSessionRootFixture::new().await;
    let (host, managed) = dispatch_test_host(
        roots
            .configure(crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&host);
    // P1 also requires actual Managed Root/lease authority before physical I/O;
    // the explicit publication remains byte-exact, not resolved from a catalog.
    let projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            &published,
        );
    let mut projection = projection;
    projection.agent_publication = Some(published.clone());
    roots
        .install_projection(&managed, "t-published-claim", projection)
        .await
        .unwrap();

    let ctx = host
        .ctx_for_snapshot(
            "t-published-claim",
            Some("published-agent"),
            Some(published.clone()),
        )
        .await
        .expect("worker session builds from claimed snapshot");

    assert_eq!(ctx.config, published);
}

#[tokio::test]
async fn background_task_fails_closed_on_a_database_less_worker() {
    // Cause/effect decision table:
    // R1 Native + co-located owner + selected BackgroundTask -> construct the
    // ordinary Runtime context; R2 Native + database-less Worker + selected
    // BackgroundTask -> reject before tool advertisement or Environment side
    // effects. ACP is the existing independent fail-closed partition. Until a
    // claim-fenced completion-attention transport exists, accepting R2 could
    // strand the process-local completion on a different Worker.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("background-agent")
        .model(test_model_binding())
        .plugins([awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string()])
        .plugin_config([(
            awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string(),
            serde_json::json!({"tools": []}),
        )])
        .build();

    let (local, _local_managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    local.register_thread_agent_projection("background-local", "background-agent");
    local
        .ctx_for_snapshot(
            "background-local",
            Some("background-agent"),
            Some(snapshot.clone()),
        )
        .await
        .expect("R1 co-located Native context");

    let worker = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_authority(crate::UpstreamAuthority::default()))
    .build()
    .expect("validated Host dependencies");
    let error = match worker
        .ctx_for_snapshot(
            "background-worker",
            Some("background-agent"),
            Some(snapshot),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("R2 database-less Worker must reject BackgroundTask"),
    };
    assert_eq!(error.kind, HostErrorKind::BadRequest, "R2 classification");
    assert!(
        error.message.contains("co-located Session application"),
        "R2 actionable boundary: {error}"
    );
    assert!(
        worker
            .realizer
            .session_environment("background-worker")
            .await
            .is_none(),
        "R2 no Environment side effect"
    );
}

#[tokio::test]
async fn outbound_a2a_never_materializes_or_owns_a_local_environment() {
    // Cause/effect graph: C1=remote A2A backend; C2=local Environment input.
    // Effects: E1=A2A-only IO context has no Environment and no Hand;
    // E2=A2A plus a local mount is rejected before provisioning; E3=adding a
    // Native fallback makes the candidate set local-capable and retains an
    // Environment. Constraint: inbound A2A is only a protocol adapter and does
    // not alter this backend rule. Decision table: R1 C1&&!C2 -> no Environment;
    // R2 C1&&C2 -> BadRequest; R3 C1+Native fallback -> one Environment/Hand.
    // All three use one real Root Store/sink; R3's physical effect cannot borrow
    // Dispatch-only configuration as an execution grant. R1/R2 retain zero I/O.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("remote-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "remote-agent",
            "",
            "a2a:https://agent.example.test",
        ))
        .build();
    let roots = ManagedSessionRootFixture::new().await;
    let (host, managed) = dispatch_test_host(
        roots
            .configure(crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&host);
    let mut projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            &snapshot,
        );
    projection.agent_publication = Some(snapshot.clone());
    roots
        .install_projection(&managed, "a2a-io-only", projection)
        .await
        .unwrap();
    let ctx = host
        .ctx_for_snapshot("a2a-io-only", Some("remote-agent"), Some(snapshot.clone()))
        .await
        .expect("R1 A2A context");
    assert!(ctx.env.is_none(), "R1 Environment");
    assert!(ctx.attempt_context.tool_executor.is_none(), "R1 Hand");
    assert!(
        host.realizer
            .session_environment("a2a-io-only")
            .await
            .is_none(),
        "R1 owner"
    );

    let mut projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            &snapshot,
        );
    projection.agent_publication = Some(snapshot.clone());
    roots
        .install_projection(&managed, "a2a-with-mount", projection)
        .await
        .unwrap();
    host.realizer.staging.register_thread_resources(
        "a2a-with-mount",
        crate::staged_resources::StagedResources {
            mounts: vec![awaken_provisioning_contract::MountRequirement {
                mount_id: "input".into(),
                source: awaken_provisioning_contract::MountSource::File {
                    file_id: "file".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/input".into(),
                access: awaken_provisioning_contract::MountAccess::ReadOnly,
                lifetime: awaken_provisioning_contract::MountLifetime::Session,
                required: true,
            }],
            ..Default::default()
        },
    );
    let error = match host
        .ctx_for_snapshot("a2a-with-mount", Some("remote-agent"), Some(snapshot))
        .await
    {
        Ok(_) => panic!("R2 local input was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind, crate::host::HostErrorKind::BadRequest, "R2");
    assert!(
        host.realizer
            .session_environment("a2a-with-mount")
            .await
            .is_none(),
        "R2"
    );

    let mixed = awaken_runtime_contract::ExecutableAgentSnapshot::builder("mixed-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "remote-agent",
            "",
            "a2a:https://agent.example.test",
        ))
        .model_candidates([awaken_runtime_contract::resolved::ModelBinding::new(
            "native-fallback",
            "native-model",
            "native",
        )])
        .build();
    let mut projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            &mixed,
        );
    projection.agent_publication = Some(mixed.clone());
    roots
        .install_projection(&managed, "a2a-native-fallback", projection)
        .await
        .unwrap();
    let ctx = host
        .ctx_for_snapshot("a2a-native-fallback", Some("mixed-agent"), Some(mixed))
        .await
        .expect("R3 mixed candidate context");
    assert!(ctx.env.is_some(), "R3 Environment");
    assert!(ctx.attempt_context.tool_executor.is_some(), "R3 Hand");
}

/// Session Event recovery selects durable trace provenance before constructing
/// the existing dispatch envelope; it never consults the recovery span.
#[tokio::test]
async fn session_event_dispatch_uses_the_explicit_persisted_trace_source() {
    // Cause/effect graph: C1 dispatch decoration receives the ordinary ambient
    // trace, a persisted Event trace, or an explicitly absent persisted trace;
    // C2 the execution payload is otherwise identical. Effects: E1 each present
    // source is copied exactly; E2 persisted None remains None and never falls
    // back to the supervisor's ambient trace; E3 trace choice does not change
    // canonical dispatch/replay identity.
    //
    // | Rule | Explicit source | Simulated ambient | Effect |
    // | T1 | ambient parent | same parent | E1 |
    // | T2 | persisted parent | different ambient | E1+E3 |
    // | T3 | persisted None | present ambient | E2+E3 |
    // Constraint/invariant: ordinary admission is the only caller that captures
    // ambient context. Session Event reservation always calls this explicit seam,
    // so recovery cannot manufacture a parent when root provenance stores None.
    const AMBIENT: &str = "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01";
    const PERSISTED: &str = "00-11111111111111111111111111111111-2222222222222222-01";
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
        .build()
        .expect("validated Host dependencies");
    let activation = || {
        awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id("run-explicit-trace-source".into()),
            awaken_agent_contract::agent::thread::Id("thread-explicit-trace-source".into()),
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(test_model_binding())
                .fingerprint("sha256:explicit-trace-source")
                .build(),
            Vec::new(),
        )
    };

    let ordinary = host
        .resolved_dispatch_with_traceparent(activation(), Some(AMBIENT.into()))
        .expect("T1 ordinary ambient source");
    let recovered_present = host
        .resolved_dispatch_with_traceparent(activation(), Some(PERSISTED.into()))
        .expect("T2 persisted source");
    let recovered_absent = host
        .resolved_dispatch_with_traceparent(activation(), None)
        .expect("T3 persisted absence");

    assert_eq!(ordinary.traceparent.as_deref(), Some(AMBIENT), "T1/E1");
    assert_eq!(
        recovered_present.traceparent.as_deref(),
        Some(PERSISTED),
        "T2/E1"
    );
    assert_eq!(recovered_absent.traceparent, None, "T3/E2");
    assert!(
        ordinary.same_canonical_dispatch(&recovered_present)
            && ordinary.same_canonical_dispatch(&recovered_absent),
        "T2+T3/E3"
    );
}

/// Dispatch Resource selection stays on the one Session-slot authority for both
/// ordinary and Session-root durable requests.
#[tokio::test]
async fn durable_dispatch_carries_the_frozen_session_resource_manifest_and_scope() {
    // Cause/effect graph: C1 the slot is/is-not a prepared Session; C2 the
    // canonical root/ordinary decorator is selected; C3 an aggregate-staged
    // Resource transition exists; C4 an active manifest exists; C5 an
    // Environment runtime projection exists independently of Session admission.
    // Effects: E1 a non-Session projection carries active even with C5; E2
    // every prepared Session projection carries desired, independently of root
    // affinity; E3 desired works before active exists and wins over stale
    // active; E4 a legacy Session without a transition falls back to active;
    // E5 dispatch decoration never publishes or replaces active; E6 runtime
    // projection presence and exact root Session affinity follow C5 and C1+C2,
    // respectively. Scope and Worker capability derive from the exact selected
    // manifest.
    //
    // | Rule | C1 | Root | C3   | C4   | C5 | Carried | Runtime | Affinity | Active after |
    // | R1   | F  | T    | none | A    | T  | A       | some    | none     | A            |
    // | R2   | T  | F    | A->B | A    | F  | B       | none    | none     | A            |
    // | R3   | T  | T    | 0->B | none | F  | B       | none    | thread   | none         |
    // | R4   | T  | T    | A->B | A    | F  | B       | none    | thread   | A            |
    // | R5   | T  | T    | none | A    | F  | A       | none    | thread   | A            |
    // | R6   | T  | T    | A->B | A    | T  | B       | some    | thread   | A            |
    // Constraint: C1, C3, C4, runtime projection, and the selected manifest are
    // read under one Session-slot lock, so no dispatch can mix generations.
    for (
        rule,
        prepared_session,
        root_decorator,
        runtime_projection,
        active_revision,
        desired_revision,
        expected_revision,
    ) in [
        ("R1", false, true, true, Some(4), None, 4),
        ("R2", true, false, false, Some(4), Some(7), 7),
        ("R3", true, true, false, None, Some(7), 7),
        ("R4", true, true, false, Some(4), Some(7), 7),
        ("R5", true, true, false, Some(4), None, 4),
        ("R6", true, true, true, Some(4), Some(7), 7),
    ] {
        let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
            .build()
            .expect("validated Host dependencies");
        let thread = format!("t-dispatch-resources-{rule}");
        let resources = |generation: &str| {
            effective_resources(vec![TestInput {
                kind: "file".into(),
                id: format!("file-{generation}-{rule}"),
                mount_path: format!("/workspace/{generation}-{rule}"),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: Some(format!("{generation} instructions for {rule}")),
                initial_branch: None,
                initial_commit: None,
            }])
        };
        let active = active_revision.map(|revision| {
            crate::session_resource::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                resources("active"),
            )
        });
        let desired = desired_revision.map(|revision| {
            crate::session_resource::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                resources("desired"),
            )
        });
        let transition = desired.clone().map(|desired| {
            crate::session_resource::SessionResourceTransition::new(
                active.clone().unwrap_or_else(|| {
                    crate::session_resource::SessionResourceManifest::at_revision(
                        "workspace-a",
                        0,
                        resources("previous"),
                    )
                }),
                desired,
            )
            .expect("test transition belongs to one Workspace")
        });
        let expected = desired
            .or_else(|| active.clone())
            .expect("selected manifest");
        host.installed_sessions.update(&thread, |slot| {
            slot.run.session_dispatch = prepared_session;
            slot.realization.manifest = active.clone();
            slot.realization.resource_transition = transition;
            slot.replace_frozen_with(|next| {
                next.environment_snapshot = runtime_projection.then(|| {
                    session_environment(
                        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                        serde_json::json!({}),
                    )
                })
            });
        });
        let activation = awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id(format!("run-dispatch-resources-{rule}")),
            awaken_agent_contract::agent::thread::Id(thread.clone()),
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(test_model_binding())
                .fingerprint(format!("sha256:dispatch-resources-{rule}"))
                .build(),
            Vec::new(),
        );

        let dispatch = if root_decorator {
            host.resolved_dispatch(activation)
        } else {
            host.resolved_thread_extension_dispatch(activation)
        }
        .unwrap_or_else(|error| panic!("{rule} decorate durable dispatch: {error}"));
        let carried = dispatch
            .session_resources
            .as_ref()
            .unwrap_or_else(|| panic!("{rule} resource envelope"))
            .decode_with(crate::session_resource::SessionResourceManifest::decode_resources)
            .unwrap_or_else(|error| panic!("{rule} decode resource envelope: {error}"));

        assert_eq!(carried.revision, expected_revision, "{rule} selection");
        assert_eq!(carried, expected, "{rule} exact generation and payload");
        assert_eq!(
            dispatch.execution_scope,
            Some(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from("workspace-a")
            )),
            "{rule} scope",
        );
        assert!(
            dispatch
                .placement
                .required_capabilities
                .contains(awaken_dispatch_contract::SESSION_RESOURCES_CAPABILITY),
            "{rule} selected manifest requires an eligible Worker",
        );
        assert_eq!(
            dispatch.session_runtime.is_some(),
            runtime_projection,
            "{rule}/E6 runtime projection presence",
        );
        assert_eq!(
            dispatch.placement.session_mcp_credentials,
            runtime_projection.then(Vec::new),
            "{rule}/E6 the Session publishes its MCP credential requirements with its projection",
        );
        assert_eq!(
            dispatch.session_thread_id,
            (prepared_session && root_decorator).then(|| ThreadId(thread.clone())),
            "{rule}/E6 exact Session affinity",
        );
        assert_eq!(
            host.realizer.staging.thread_resource_manifest(&thread),
            active,
            "{rule}/E5 dispatch staging must not publish active",
        );
    }
}

/// Coordinator-to-Worker publication cause/effect/FMECA design. C1 a parent
/// activation references a published child; C2 the Coordinator owns the sole
/// executable catalog. E1 `resolved_dispatch` embeds the exact child snapshot;
/// E2 no mutable catalog handle crosses the queue. Rule A1=C1+C2=>E1+E2.
/// FMECA: omitting the snapshot strands every remote delegation (high severity,
/// repeated claim failure); the assertion detects the omission at admission and
/// the cold-Worker decision-table test verifies the receiving half.
#[tokio::test]
async fn durable_dispatch_freezes_the_delegation_publication_closure() {
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::AgentId;

    let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
        .model(test_model_binding())
        .fingerprint("researcher-current")
        .build();
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([child.clone()])
            .expect("A1 publication source");
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default")
        .await
        .with_agent_publications(Arc::new(publications)))
    .build()
    .expect("validated Host dependencies");
    let parent = awaken_runtime_contract::ExecutableAgentSnapshot::builder("coordinator")
        .model(test_model_binding())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-publication-closure".into()),
        awaken_agent_contract::agent::thread::Id("thread-publication-closure".into()),
        parent,
        Vec::new(),
    );

    let dispatch = host.resolved_dispatch(activation).expect("A1 admission");
    assert_eq!(dispatch.agent_publications, vec![child], "A1/E1+E2");
}

#[tokio::test]
async fn durable_dispatch_marks_only_a_prepared_root_session_for_worker_realization() {
    // Cause/effect graph: C1 the request entered the Session application port;
    // C2 only a Resource manifest exists; C3 a child Run is parent-mediated.
    // Effects: E1 the root dispatch names its already-frozen Session; E2 an ordinary
    // resource-bearing Run remains ordinary; E3 a child retains the parent
    // Session pointer. C1 and C2 are mutually exclusive test fixtures here; C3 is owned by
    // `child_dispatch_reuses_publication_pinned_model_candidates`.
    //
    // | Rule | Session API | Resources only | Child | session_thread_id |
    // | R1   | yes            | any            | no    | root thread       |
    // | R2   | no             | yes            | no    | none              |
    // | R3   | n/a            | any            | yes   | parent thread     |
    //
    // This test owns R1. The adjacent resource-envelope test owns R2 and the
    // existing child-dispatch test owns R3, avoiding a parallel child builder.
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
        .build()
        .expect("validated Host dependencies");
    let thread = "prepared-root-session";
    host.installed_sessions
        .update_run(thread, |run| run.session_dispatch = true);
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-prepared-root-session".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(test_model_binding())
            .fingerprint("sha256:prepared-root-session")
            .build(),
        Vec::new(),
    );

    let dispatch = host
        .resolved_dispatch(activation)
        .expect("decorate prepared Session dispatch");

    assert_eq!(
        dispatch.session_thread_id,
        Some(awaken_agent_contract::agent::thread::Id(thread.into())),
        "R1/E1"
    );
}

#[tokio::test]
async fn thread_extension_dispatch_reuses_frozen_inputs_without_session_admission() {
    // Cause/effect graph: C1 a prepared Session marks its public root Runs for
    // Session admission; C2 the Outcome bounded context owns an ordinary Run on
    // that same Thread. Effects: E1 C1 uses root Session affinity; E2 C1+C2
    // preserves the ordinary full-dispatch shape while reusing the canonical
    // decorator. Decision table: public root => E1 (adjacent test); Outcome
    // extension => E2. A structural self-affinity must never be inferred for C2.
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
        .build()
        .expect("validated Host dependencies");
    let thread = "prepared-outcome-thread";
    host.installed_sessions
        .update_run(thread, |run| run.session_dispatch = true);
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("outcome-worker-run".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(test_model_binding())
            .fingerprint("sha256:outcome-worker")
            .build(),
        Vec::new(),
    );

    let dispatch = host
        .resolved_thread_extension_dispatch(activation)
        .expect("decorate Outcome-owned ordinary Run");

    assert_eq!(dispatch.session_thread_id, None, "C2/E2");
    assert_eq!(
        dispatch.admission_shape(),
        awaken_dispatch_contract::DispatchAdmissionShape::OrdinaryRoot,
        "C2/E2 remains eligible for generic full-dispatch admission",
    );
}

#[tokio::test]
async fn cold_host_inference_holder_follows_the_candidate_backend_decision_table() {
    // Cause graph: C1=credential-bearing candidate; C2=Native; C3=ACP;
    // C4=mixed boundaries; C5=publication freezes one common holder;
    // C6=prepared Environment requests a different holder; C7=closed Remote
    // coordinate (exact A2A backend with no local model reference); C8=the
    // candidate policies have no common holder. The immutable publication
    // decision feeds root, child, direct, and dispatch paths. An exact
    // publication holder dominates the Environment fallback; a boundary-only
    // decision must agree with that Environment.
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | result                   |
    // | R1   | F  | -  | -  | F  | F  | -  | F  | F  | no holder                |
    // | R2   | T  | T  | F  | F  | F  | F  | F  | F  | Worker holder            |
    // | R3   | T  | F  | T  | F  | F  | F  | F  | F  | Workload holder          |
    // | R4   | T  | T  | T  | T  | F  | -  | F  | F  | reject                   |
    // | R5   | T  | -  | -  | -  | T  | F  | F  | F  | exact publication holder |
    // | R6   | T  | -  | -  | -  | T  | T  | F  | F  | exact publication holder |
    // | R7   | T  | T  | F  | F  | F  | T  | F  | F  | reject                   |
    // | R8   | T  | F  | F  | F  | F  | F  | T  | F  | Worker holder            |
    // | R9   | T  | T  | F  | F  | F  | F  | F  | T  | reject                   |
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default").await)
        .build()
        .expect("validated Host dependencies");
    let candidate_with_policy =
        |model: &str, backend: &str, policy: awaken_runtime_contract::CredentialExecutionPolicy| {
            let binding =
                awaken_runtime_contract::resolved::ModelBinding::new("provider", model, backend);
            let credential = Some(
                awaken_runtime_contract::CredentialAccess::new(
                    awaken_runtime_contract::CredentialRef {
                        id: format!("credential-{model}"),
                        revision: 1,
                    },
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                    awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                    policy,
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    "provider",
                )),
            );
            let endpoint = awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: "test".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            };
            if backend.starts_with("acp:") {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                    binding,
                    "provider@1",
                    "route@1",
                    "workspace-a",
                    credential,
                    endpoint,
                    awaken_runtime_contract::resolved::AcpExecutionProfile {
                        supports_tool_aliases: false,
                        capability_fingerprint: "sha256:test-capability".into(),
                        capability_adapter_version: "test".into(),
                        session_configuration: Default::default(),
                    },
                )
            } else {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
                    binding,
                    "provider@1",
                    "route@1",
                    "workspace-a",
                    credential,
                    endpoint,
                )
            }
            .expect("coherent holder-policy candidate")
        };
    let candidate = |model: &str, backend: &str| {
        candidate_with_policy(
            model,
            backend,
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )
    };
    let activation = |primary, fallbacks| {
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .resolved_model(primary)
            .build();
        snapshot.resolved_spec.model_candidates = fallbacks;
        awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id("run".into()),
            awaken_agent_contract::agent::thread::Id("cold-thread".into()),
            snapshot,
            Vec::new(),
        )
    };

    let anonymous = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new("host", "anonymous", "native"),
    );
    assert_eq!(
        host.inference_plaintext_holder(&activation(anonymous, Vec::new()))
            .unwrap(),
        None,
        "R1"
    );
    let native_activation = activation(candidate("native", "native"), Vec::new());
    assert_eq!(
        crate::host::self_hosted_inference_holder(&native_activation)
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "R2"
    );
    assert_eq!(
        host.inference_plaintext_holder(&native_activation).unwrap(),
        crate::host::self_hosted_inference_holder(&native_activation).unwrap(),
        "the public cold-start decision and Host fallback must remain identical"
    );
    assert_eq!(
        host.inference_plaintext_holder(&activation(candidate("acp", "acp:codex"), Vec::new()))
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Workload,
        "R3"
    );
    let remote = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "remote",
            "",
            "a2a:https://agent.example",
        ),
        awaken_tenancy::ScopeId::from("workspace-a"),
        Some(awaken_runtime_contract::CredentialAccess::new(
            awaken_runtime_contract::CredentialRef {
                id: "credential-remote".into(),
                revision: 1,
            },
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_runtime_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )),
        "sha256:card",
    )
    .expect("coherent remote candidate");
    assert_eq!(
        crate::host::self_hosted_inference_holder(&activation(remote, Vec::new()))
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "R8"
    );
    assert!(
        host.inference_plaintext_holder(&activation(
            candidate("native", "native"),
            vec![candidate("acp", "acp:codex")],
        ))
        .is_err(),
        "R4"
    );
    let platform_holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Platform,
        "awaken.cloud.egress-gateway",
    );
    let platform_candidate = candidate_with_policy(
        "platform",
        "hosted",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            platform_holder.clone(),
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let platform_activation = activation(platform_candidate, Vec::new());
    assert_eq!(
        crate::host::self_hosted_inference_holder(&platform_activation).unwrap(),
        Some(platform_holder.clone()),
        "R5"
    );
    host.install_environment_projection(
        &platform_activation.thread_id.0,
        &session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    )
    .expect("install a prepared self-hosted Environment profile");
    assert_eq!(
        host.inference_plaintext_holder(&platform_activation)
            .unwrap(),
        Some(platform_holder),
        "R6"
    );
    let mut conflicting_environment = session_environment(
        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    conflicting_environment.credential_realization =
        awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp();
    let conflicting_host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "host-default")
        .await)
        .build()
        .expect("validated Host dependencies");
    conflicting_host
        .install_environment_projection("cold-thread", &conflicting_environment)
        .expect("replace the prepared Environment profile");
    assert!(
        conflicting_host
            .inference_plaintext_holder(&native_activation)
            .is_err(),
        "R7"
    );

    let worker_candidate = candidate_with_policy(
        "worker-exact",
        "native",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
                .inference_holder,
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let workload_candidate = candidate_with_policy(
        "workload-exact",
        "native",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp()
                .inference_holder,
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let error = crate::host::self_hosted_inference_holder(&activation(
        worker_candidate,
        vec![workload_candidate],
    ))
    .expect_err("R8 disjoint exact holder policies must fail closed");
    assert!(
        error
            .to_string()
            .contains("no common credential plaintext holder"),
        "R9 exact rejection reason: {error}"
    );
}

#[tokio::test]
async fn replacement_host_adopts_the_dispatch_sandbox_from_a_stable_root() {
    let storage = tempfile::tempdir().expect("storage dir");
    let thread = "t-sandbox-recovery";
    // Same selected Control owner/incarnation across two Host components: the
    // Root and its first receipt survive, not a foreign Worker reassignment.
    let roots = ManagedSessionRootFixture::new().await;

    let (first, first_managed) = dispatch_test_host(
        roots
            .configure(
                crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                    .await
                    .with_store_dir(storage.path())
                    .await,
            )
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&first);
    let original_projection = roots
        .install_complete_test_session(
            &first_managed,
            thread,
            bare_session("assistant", first.local_workspace()),
        )
        .await
        .expect("install first complete Session projection");
    let first_ctx = first.ctx_for(thread, None).await.expect("first session");
    let handle = first_ctx
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();
    let marker = storage
        .path()
        .join("sandboxes")
        .join(thread)
        .join("recovery-marker");
    std::fs::write(&marker, b"survived").expect("write sandbox marker");
    drop(first_ctx);
    drop(first_managed);
    drop(first);

    let (replacement, replacement_managed) = dispatch_test_host(
        roots
            .configure(
                crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
                    .await
                    .with_store_dir(storage.path())
                    .await,
            )
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&replacement);
    // Recovery reads the existing authoritative Root. The replacement Host's
    // defaults are not a new creation request or an idempotency replay input.
    use awaken_session_repository_contract::SessionRepository as _;
    let stored = roots
        .repository
        .get(&awaken_session_types::SessionId::from(thread))
        .await
        .unwrap();
    roots
        .install_committed_projection(&replacement_managed, thread, original_projection, stored)
        .await
        .expect("restore the exact existing Root projection");
    let binding = serde_json::to_string(&handle).expect("encode durable handle");
    assert_eq!(
        replacement
            .adopt_bound_session_environment(
                thread,
                Some(&binding),
                &replacement.realizer.env.session_provider,
                None,
                false,
            )
            .await
            .expect("adopt durable handle"),
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let replacement_ctx = replacement
        .ctx_for(thread, None)
        .await
        .expect("replacement session");

    assert_eq!(
        replacement_ctx
            .env
            .as_ref()
            .expect("eager environment")
            .lifecycle()
            .handle(),
        handle
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"survived");
}

#[tokio::test]
async fn resident_session_accepts_only_an_adoption_of_its_exact_sandbox() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (host, managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&host);
    install_complete_test_session_for_realization(
        &managed,
        "t-resident-adoption",
        bare_session("assistant", host.local_workspace()),
    )
    .await
    .expect("install resident complete Session projection");
    let resident = host
        .ctx_for("t-resident-adoption", None)
        .await
        .expect("resident session");
    let resident_handle = resident
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();

    let resident_binding = serde_json::to_string(&resident_handle).unwrap();
    assert_eq!(
        host.adopt_bound_session_environment(
            "t-resident-adoption",
            Some(&resident_binding),
            &host.realizer.env.session_provider,
            None,
            false,
        )
        .await
        .expect("adopt resident sandbox"),
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let reused = host
        .ctx_for("t-resident-adoption", None)
        .await
        .expect("the exact resident sandbox is idempotently accepted");
    assert_eq!(
        reused
            .env
            .as_ref()
            .expect("reused Environment")
            .lifecycle()
            .handle(),
        resident_handle,
        "idempotent adoption may rebuild the cache but retains physical identity"
    );

    let foreign = host
        .realizer
        .env
        .session_provider
        .create(&host.realizer.sandbox_spec("t-foreign-resident"))
        .await
        .expect("foreign sandbox");
    let foreign_binding = serde_json::to_string(&foreign.lifecycle().handle()).unwrap();
    let error = match host
        .adopt_bound_session_environment(
            "t-resident-adoption",
            Some(&foreign_binding),
            &host.realizer.env.session_provider,
            None,
            false,
        )
        .await
    {
        Ok(_) => panic!("a resident session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "sandbox t-foreign-resident does not belong to Session t-resident-adoption"
    );
    assert_eq!(
        resident
            .env
            .as_ref()
            .expect("eager environment")
            .lifecycle()
            .handle(),
        resident_handle
    );
}

#[tokio::test]
async fn retained_session_accepts_only_an_adoption_of_its_exact_sandbox() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (host, managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies"),
    );
    install_test_session_application(&host);
    install_complete_test_session_for_realization(
        &managed,
        "t-retained-adoption",
        bare_session("assistant", host.local_workspace()),
    )
    .await
    .expect("install retained complete Session projection");
    let original = host
        .ctx_for("t-retained-adoption", None)
        .await
        .expect("initial session");
    let retained_handle = original
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();
    host.realizer
        .evict_session_for_rebuild("t-retained-adoption")
        .await;
    drop(original);

    let foreign = host
        .realizer
        .env
        .session_provider
        .create(&host.realizer.sandbox_spec("t-foreign-retained"))
        .await
        .expect("foreign sandbox");
    let foreign_binding = serde_json::to_string(&foreign.lifecycle().handle()).unwrap();
    let error = match host
        .adopt_bound_session_environment(
            "t-retained-adoption",
            Some(&foreign_binding),
            &host.realizer.env.session_provider,
            None,
            false,
        )
        .await
    {
        Ok(_) => panic!("a retained session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "sandbox t-foreign-retained does not belong to Session t-retained-adoption"
    );

    let retained_binding = serde_json::to_string(&retained_handle).unwrap();
    assert_eq!(
        host.adopt_bound_session_environment(
            "t-retained-adoption",
            Some(&retained_binding),
            &host.realizer.env.session_provider,
            None,
            false,
        )
        .await
        .expect("adopt retained sandbox"),
        crate::host::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let rebuilt = host
        .ctx_for("t-retained-adoption", None)
        .await
        .expect("the exact retained sandbox rebuilds the runtime context");
    assert_eq!(
        rebuilt
            .env
            .as_ref()
            .expect("eager environment")
            .lifecycle()
            .handle(),
        retained_handle
    );
    assert_eq!(
        host.session_environment_handle("t-retained-adoption").await,
        Some(retained_handle)
    );
}
