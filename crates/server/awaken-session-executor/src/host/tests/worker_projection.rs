// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
// @formal evidence session_run.reservation_never_executes
//! Worker runtime projection: frozen baselines, reservations, and coordination contexts.

use super::*;

#[tokio::test]
async fn missing_projection_admits_only_the_direct_empty_revision_zero_noop() {
    struct Rule {
        id: &'static str,
        previous_revision: u64,
        desired_revision: u64,
        previous_nonempty: bool,
        desired_nonempty: bool,
        claimed: bool,
        runtime_projection: bool,
        accepted: bool,
    }

    // Missing-projection cause/effect decision table:
    // C1 no dispatch claim; C2 no Managed dispatch marker; C3 no frozen
    // baseline; C4 previous == desired; C5 desired revision is zero; C6 both
    // endpoint Resource sets are empty; C7 a frozen Runtime projection exists.
    // E1 admits the direct no-op or projected claimed Run into the canonical
    // apply path; E2 rejects before File/Skill/Repository compilation.
    // C2+C3 are fixed true for every row because an installed Managed marker or
    // baseline already uses the ordinary projection branch. Explicit fixture
    // branches materialize each C6 endpoint without obscuring its empty/nonempty
    // cause behind boolean combinators.
    //
    // | Rule | C1 | C4 | C5 | C6 | C7 | Effect |
    // | D1 | T | T | T | T | F | E1 |
    // | D2 | F | T | T | T | F | E2 |
    // | D3 | T | F | F | T | F | E2 |
    // | D4 | T | T | F | T | F | E2 |
    // | D5 | T | T | T | F | F | E2 |
    // | D6 | T | F | T | F | F | E2 |
    // | D7 | F | T | T | T | T | E1 |
    let rules = [
        Rule {
            id: "D1-direct-empty-rev0-noop",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            runtime_projection: false,
            accepted: true,
        },
        Rule {
            id: "D2-claimed",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: true,
            runtime_projection: false,
            accepted: false,
        },
        Rule {
            id: "D3-advancing-empty",
            previous_revision: 0,
            desired_revision: 1,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            runtime_projection: false,
            accepted: false,
        },
        Rule {
            id: "D4-nonzero-noop",
            previous_revision: 1,
            desired_revision: 1,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            runtime_projection: false,
            accepted: false,
        },
        Rule {
            id: "D5-nonempty-noop",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: true,
            desired_nonempty: true,
            claimed: false,
            runtime_projection: false,
            accepted: false,
        },
        Rule {
            id: "D6-nonempty-replacement",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: true,
            claimed: false,
            runtime_projection: false,
            accepted: false,
        },
        Rule {
            id: "D7-claimed-with-runtime-projection",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: true,
            runtime_projection: true,
            accepted: true,
        },
    ];

    let nonempty = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: "guard-must-not-read-this-file".into(),
        mount_path: "/guarded.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    for rule in rules {
        let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
        let _runtime = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let thread = format!("direct-resource-guard-{}", rule.id);
        let previous = if rule.previous_nonempty {
            nonempty.clone()
        } else {
            Default::default()
        };
        let desired = if rule.desired_nonempty {
            nonempty.clone()
        } else {
            Default::default()
        };
        let transition = resource_transition(
            host.local_workspace(),
            rule.previous_revision,
            previous,
            rule.desired_revision,
            desired,
        );
        let claim = rule.claimed.then(|| awaken_dispatch::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(format!("run-{}", rule.id)),
            owner: "claimed-worker".into(),
            epoch: 1,
        });
        if rule.runtime_projection {
            host.install_environment_projection(&thread, &on_tool_use_environment())
                .unwrap_or_else(|error| panic!("{} installs C7: {error}", rule.id));
        }
        assert_eq!(
            host.session_slots
                .read(&thread, |slot| (
                    slot.baseline.is_some(),
                    slot.session_dispatch
                ))
                .unwrap_or((false, false)),
            (false, false),
            "{} keeps C2+C3 fixed",
            rule.id
        );

        let result = host
            .apply_dispatched_resource_transition(&thread, &transition, claim.as_ref(), None)
            .await;
        if rule.accepted {
            result.unwrap_or_else(|error| panic!("{} must admit E1: {error}", rule.id));
        } else {
            let error = result.expect_err("all non-direct rows must fail closed");
            assert_eq!(
                error.code, "session_resource_projection_not_staged",
                "{} must reject at E2 before compiling the synthetic File",
                rule.id
            );
        }
    }
}

#[tokio::test]
async fn control_frozen_baseline_is_the_only_worker_runtime_projection() {
    use awaken_provisioning_contract::{
        EnvValue, EnvVar, EnvVisibility, MountAccess, MountLifetime, MountRequirement, MountSource,
    };

    fn projection(
        prompt: &str,
        with_environment_inputs: bool,
    ) -> awaken_session_realization_contract::FrozenSessionProjection {
        let mount = MountRequirement {
            mount_id: "flow-workspace".into(),
            source: MountSource::Inline {
                contents: "project".into(),
            },
            mount_path: "/workspace/project.txt".into(),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
            required: true,
        };
        let env = EnvVar {
            name: "FLOW_PROJECT".into(),
            value: EnvValue::Inline {
                value: "project-a".into(),
            },
            visibility: EnvVisibility::Process,
        };
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let mounts = with_environment_inputs
            .then_some(mount)
            .into_iter()
            .collect();
        let env = with_environment_inputs.then_some(env).into_iter().collect();
        let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_environment_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint:
                        awaken_session_configuration_contract::EnvironmentFingerprint(
                            "env-fingerprint".into(),
                        ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: if with_environment_inputs {
                        awaken_session_configuration_contract::SessionNetworkPolicy::None
                    } else {
                        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted
                    },
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement:
                    awaken_session_configuration_contract::SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts,
                env,
                prompts: vec![prompt.into()],
                transcript_prefix: None,
            },
        );
        awaken_session_realization_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_types::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 7,
            resources: awaken_session_resource_contract::ResolvedSessionResources::default(),
            previous_resource_manifest: Some(
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace",
                    7,
                    awaken_session_resource_contract::ResolvedSessionResources::default(),
                ),
            ),
            mcp: Vec::new(),
            tools: Default::default(),
            request_context: Vec::new(),
        }
    }

    #[derive(Clone, Default)]
    struct PromptRecorder(Arc<Mutex<Vec<ChatRequest>>>);

    #[derive(Default)]
    struct RepositoryClaimRecorder(Mutex<Vec<Option<awaken_dispatch::RunClaim>>>);

    struct FailingRepositoryVerifier;

    #[async_trait::async_trait]
    impl crate::RepositoryBindingVerifier<awaken_dispatch::ResourceOperationFence>
        for RepositoryClaimRecorder
    {
        async fn verify(
            &self,
            _workspace_id: &str,
            _repository_id: &str,
            _config_version: awaken_resource_contract::ConfigVersion,
            authority: Option<&awaken_dispatch::ResourceOperationFence>,
        ) -> Result<
            awaken_resource_contract::RepositoryTransport,
            awaken_resource_contract::RepositoryBindingVerifierError,
        > {
            self.0
                .lock()
                .unwrap()
                .push(authority.and_then(|authority| match authority {
                    awaken_dispatch::ResourceOperationFence::Run(claim) => Some(claim.clone()),
                    _ => None,
                }));
            Ok(awaken_resource_contract::RepositoryTransport::Direct)
        }
    }

    #[async_trait::async_trait]
    impl crate::RepositoryBindingVerifier<awaken_dispatch::ResourceOperationFence>
        for FailingRepositoryVerifier
    {
        async fn verify(
            &self,
            _workspace_id: &str,
            _repository_id: &str,
            _config_version: awaken_resource_contract::ConfigVersion,
            _authority: Option<&awaken_dispatch::ResourceOperationFence>,
        ) -> Result<
            awaken_resource_contract::RepositoryTransport,
            awaken_resource_contract::RepositoryBindingVerifierError,
        > {
            Err(
                awaken_resource_contract::RepositoryBindingVerifierError::Denied {
                    detail: "injected binding failure".into(),
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl LlmExecutor for PromptRecorder {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.lock().unwrap().push(request);
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    // Test-runtime stack cause/effect decision table. C0 the ordinary libtest
    // thread uses its default stack; C1 every independent projection scenario
    // lives in one outer async state machine; C2 the same scenarios execute in
    // their original order as bounded inner futures; C3 the primary Host,
    // Managed adapter, recorder, and claim recorder remain shared. Effects: E0
    // C0+C1 overflows before the behavioral assertions can finish; E1 C0+C2+C3
    // retains every assertion and state dependency while bounding live future
    // state to one scenario. The larger-stack run is diagnostic evidence only,
    // never a test or product runtime requirement.
    //
    // | Rule | Default stack | Segmented | Shared primary state | Effect |
    // | S1 | yes | no  | yes | E0 (red baseline) |
    // | S2 | yes | yes | yes | E1 (all rules complete) |
    async {
        // Co-located Native baseline installation has the same immutable binding
        // rules as the claimed Worker projection. Decision table:
        // B1 valid first install without an early admission marker -> accept
        // and classify from the Control-frozen baseline; B2 identical replay ->
        // idempotent; B3 empty fingerprint -> reject; B4 different fingerprint
        // -> reject; B5 Environment already realized -> reject rather than run
        // without the frozen mounts/env/prompts; B6 a metadata-only Coordinator
        // reservation envelope precedes the first complete baseline under
        // local realization or Coordinator Dispatch. Effects:
        // E1 B1 excludes the direct live-authored Skill source on a cold Worker;
        // E2 B2 preserves the same projection; E3 B3-B5 publish no conflicting
        // projection; E4 B6 installs the baseline and evicts only the stale
        // coordination envelope so a later File attachment can be realized.
        let baseline_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let _baseline_runtime =
            crate::ManagedHost::new(baseline_host.clone()).install_dispatch_session_runtime();
        let first_projection = projection("baseline-a", true);
        baseline_host
            .install_frozen_session_projection(
                "local-baseline",
                first_projection.clone(),
                None,
                true,
                None,
            )
            .await
            .expect("B1 valid baseline installs");
        assert!(
            baseline_host
                .session_slots
                .read("local-baseline", |slot| {
                    slot.baseline.is_some() && slot.session_dispatch
                })
                .unwrap_or(false),
            "B1/E1 frozen baseline and Managed execution marker publish atomically on a cold Worker"
        );
        assert_eq!(
            baseline_host.session_skill_source_roots(
                "local-baseline",
                None,
                crate::skills::MANAGED_SKILLS_SUBDIR,
            ),
            None,
            "B1/E1 no direct live-authored Skill source is registered"
        );
        baseline_host
            .install_frozen_session_projection(
                "local-baseline",
                first_projection.clone(),
                None,
                true,
                None,
            )
            .await
            .expect("B2 same baseline is idempotent");

        let mut empty = first_projection.clone();
        empty.baseline.fingerprint.0.clear();
        assert!(
            baseline_host
                .install_frozen_session_projection("empty-baseline", empty, None, true, None)
                .await
                .unwrap_err()
                .message
                .contains("fingerprint must not be empty"),
            "B3"
        );
        let conflicting = projection("baseline-b", true);
        assert!(
            baseline_host
                .install_frozen_session_projection("local-baseline", conflicting, None, true, None,)
                .await
                .unwrap_err()
                .message
                .contains("different frozen Session baseline"),
            "B4"
        );
        baseline_host
            .ctx_for("realized-before-baseline", None)
            .await
            .expect("realize the negative-case Environment");
        assert!(
            baseline_host
                .install_frozen_session_projection(
                    "realized-before-baseline",
                    first_projection,
                    None,
                    true,
                    None,
                )
                .await
                .unwrap_err()
                .message
                .contains("realized before its frozen Session baseline"),
            "B5"
        );
        install_test_session_application(&baseline_host);
        baseline_host
            .session_slots
            .update("coordinator-before-baseline", |slot| {
                slot.session_dispatch = true;
            });
        let coordinator = baseline_host
            .ctx_for_session_coordination("coordinator-before-baseline", None)
            .await
            .expect("B6 metadata-only reservation context");
        assert!(
            coordinator.is_coordination_envelope(),
            "B6/E4 no execution owner"
        );
        assert!(coordinator.env.is_none(), "B6/E4 no physical Environment");
        baseline_host
            .install_frozen_session_projection(
                "coordinator-before-baseline",
                projection("baseline-a", true),
                None,
                true,
                None,
            )
            .await
            .expect("B6 complete projection replaces the cold reservation envelope");
        assert!(
            baseline_host
                .session_slots
                .read("coordinator-before-baseline", |slot| {
                    slot.baseline.is_some() && slot.runtime.is_none()
                })
                .unwrap_or(false),
            "B6/E4 baseline is installed and stale coordination cache is gone"
        );
        baseline_host
            .session_slots
            .update("dispatch-before-baseline", |slot| {
                slot.session_dispatch = true;
            });
        let dispatch_coordinator = baseline_host
            .ctx_for_session_coordination("dispatch-before-baseline", None)
            .await
            .expect("B6 Coordinator opened a reservation before Resource amendment");
        assert!(dispatch_coordinator.is_coordination_envelope(), "B6/E4");
        baseline_host
            .install_dispatch_frozen_session_projection(
                "dispatch-before-baseline",
                projection("baseline-a", true),
            )
            .await
            .expect("B6 Dispatch installs one complete baseline after reservation");
        assert!(
            baseline_host
                .session_slots
                .read("dispatch-before-baseline", |slot| {
                    slot.baseline.is_some() && slot.runtime.is_none()
                })
                .unwrap_or(false),
            "B6/E4 Dispatch also clears the stale reservation envelope"
        );
    }
    .await;

    async {
        // Fault-injection rule B6: any fallible Resource verification fails before
        // publishing the complete logical projection. No baseline, workspace,
        // Agent, manifest, prompt, or lease fragment may survive independently. An
        // empty process-local coordination slot is not Session truth and may remain
        // to serialize a concurrent retry.
        let failed_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let _failed_runtime = crate::ManagedHost::new(failed_host.clone())
            .with_repository_binding_verifier(Arc::new(FailingRepositoryVerifier))
            .install_dispatch_session_runtime();
        let mut rejected = projection("must not publish", false);
        rejected.resources = effective_repository(
            "rejected-repository",
            "https://example.invalid/rejected.git",
            "/workspace/rejected",
            None,
        );
        assert!(
            failed_host
                .install_frozen_session_projection("failed-projection", rejected, None, true, None)
                .await
                .unwrap_err()
                .message
                .contains("injected binding failure"),
            "B6 injected fault"
        );
        assert!(
            failed_host
                .session_slots
                .read("failed-projection", |slot| {
                    slot.baseline.is_none()
                        && slot.workspace.is_none()
                        && slot.agent_id.is_none()
                        && slot.manifest.is_none()
                        && slot.resources.mounts.is_empty()
                        && slot.resources.prompts.is_empty()
                        && slot.resources.resource_prompts.is_empty()
                        && slot.request_context.is_empty()
                        && slot.realization_authority.lease().is_none()
                        && !slot.session_dispatch
                })
                .unwrap_or(true),
            "B6 failed preparation cannot publish partial Session truth"
        );
    }
    .await;

    let recorder = PromptRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(SharedHost::new(Arc::new(recorder), "stub"));
    install_test_session_application(&host);
    let repository_claims = Arc::new(RepositoryClaimRecorder::default());
    let managed = crate::ManagedHost::new(host.clone())
        .with_repository_binding_verifier(repository_claims.clone())
        .install_dispatch_session_runtime();

    async {
        // Complete-install and unattempted Resource-amendment cause/effect table.
        // C0 the application calls the sole complete-projection port in Dispatch
        // mode; C1 a complete frozen dispatch projection already names generation
        // 7; C2 the Session aggregate
        // amends its still-unattempted desired content at generation 7; C3 the
        // caller is the Coordinator Dispatch installer or a claimed Worker.
        // Effects: E0 the one call publishes baseline, the exact staged Resource
        // transition, and the Managed execution marker together (there is no
        // second SessionInit port), while active completion remains absent; E1 the
        // Coordinator replaces its disposable transition and preserves generation
        // 7; E2 Worker rejects the same content change and leaves the old staged
        // transition intact. A newer generation and exact replay remain covered by
        // the projection-owner decision table.
        //
        // | Rule | C0 | C1 | C2 | C3 | Effect |
        // | A0 | T | F | F | Coordinator Dispatch | E0 |
        // | A1 | T | T | T | Worker claim | E2 |
        // | A2 | T | T | T | Coordinator Dispatch | E1 |
        let amendment_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let amendment_managed = crate::ManagedHost::new(amendment_host.clone())
            .with_repository_binding_verifier(Arc::new(RepositoryClaimRecorder::default()))
            .install_dispatch_session_runtime();
        let initial_dispatch = projection("authority amendment", false);
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &amendment_managed,
            "authority-amendment",
            initial_dispatch.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("A1 initial dispatch projection");
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.baseline.is_some()
                        && slot.session_dispatch
                        && slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().revision == initial_dispatch.resource_revision
                                && transition.desired().resources == initial_dispatch.resources
                        })
                })
                .unwrap_or(false),
            "A0/E0 complete projection stages one transition without forging physical completion"
        );
        let mut amended_dispatch = initial_dispatch;
        amended_dispatch.resources = effective_repository(
            "authority-amendment-repository",
            "https://example.invalid/authority-amendment.git",
            "/workspace/authority-amendment",
            None,
        );
        let amendment_claim = awaken_dispatch::RunClaim {
            run_id: RunId("authority-amendment-run".into()),
            owner: "claimed-worker".into(),
            epoch: 1,
        };
        assert!(
            amendment_host
                .install_frozen_session_projection(
                    "authority-amendment",
                    amended_dispatch.clone(),
                    Some(&amendment_claim),
                    true,
                    None,
                )
                .await
                .unwrap_err()
                .message
                .contains("cannot replace its current Session Resource projection"),
            "A1/E2 claimed Worker remains fenced"
        );
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().resources
                                == awaken_session_resource_contract::ResolvedSessionResources::default()
                        })
                })
                .unwrap_or(false),
            "A1/E2 rejection is side-effect free"
        );
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &amendment_managed,
            "authority-amendment",
            amended_dispatch.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("A2 authority amendment");
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired()
                                == &awaken_session_resource_contract::SessionResourceManifest::at_revision(
                                    amended_dispatch.workspace_id.clone(),
                                    amended_dispatch.resource_revision,
                                    amended_dispatch.resources.clone(),
                                )
                        })
                })
                .unwrap_or(false),
            "A2/E1 exact same-generation content replaces only the staged projection"
        );
    }
    .await;

    async {
        // Prospective-layout decision rule: F1 a cold projection's Repository is
        // nested below a mount in the same not-yet-resident frozen baseline. F1 must
        // fail before the baseline, Environment, Resource manifest, Skill loader, or
        // Repository verifier is touched. Reading only the current slot would miss
        // this first-install combination.
        let mut conflicting = projection("prospective conflict", true);
        conflicting.resources = effective_resources(vec![TestInput {
            kind: "github_repository".into(),
            id: "https://github.com/awaken/prospective-conflict.git".into(),
            mount_path: "/workspace/project.txt/repository".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
            initial_branch: None,
            initial_commit: None,
        }]);
        let error = host
            .install_frozen_session_projection(
                "prospective-layout-conflict",
                conflicting,
                None,
                true,
                None,
            )
            .await
            .expect_err("F1 rejects the complete prospective layout");
        assert!(error.message.contains("overlaps"), "F1: {error:?}");
        assert!(
            host.thread_resource_manifest("prospective-layout-conflict")
                .is_none(),
            "F1 no Resource projection"
        );
        assert!(
            host.session_slots
                .read("prospective-layout-conflict", |slot| {
                    slot.baseline.is_none() && slot.environment_projection.is_none()
                })
                .unwrap_or(true),
            "F1 no partial frozen projection"
        );
        assert!(
            repository_claims.0.lock().unwrap().is_empty(),
            "F1 no Repository verifier effect"
        );
    }
    .await;

    async {
        let frozen = projection("Use the bound Flow project.", true);
        host.install_frozen_session_projection("flow-thread", frozen.clone(), None, true, None)
            .await
            .expect("first frozen projection installs");
        host.install_frozen_session_projection("flow-thread", frozen, None, true, None)
            .await
            .expect("same frozen fingerprint is idempotent");
    }
    .await;

    // Branch request-context cause/effect graph: C1 the frozen baseline is
    // already resident; C2 its immutable transcript prefix is materialized only
    // on the later claim-fenced projection; C3 an identical projection replays;
    // C4 the branch-only binding sink persists and returns one exact Store-read
    // Resident before any later projection. E1 C1+C2+C4 invalidates only the
    // stale SessionCtx and rebuilds with the exact request-only messages; E2
    // C1+C2+C3+C4 retains that rebuilt context; E3 C4 persists exactly once and
    // every later projection carries that returned Resident. FMECA: updating
    // the slot directly leaves a resident ACP/Native attempt context stale,
    // while replaying an Unmaterialized aggregate over its live owner violates
    // the durable binding fence.
    //
    // | Rule | C1 | C2 changed | C3 replay | Effect |
    // | B1   | T  | T          | F         | E1+E3  |
    // | B2   | T  | F          | T         | E2+E3  |
    let branch_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
    install_test_session_application(&branch_host);
    let committed_environment = Arc::new(Mutex::new(None));
    let branch_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&branch_host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: Some("branch-thread".into()),
        committed_environment: Some(committed_environment.clone()),
    });
    let branch_managed =
        crate::ManagedHost::new(branch_host.clone()).install_dispatch_session_runtime();
    branch_managed
        .realization()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .install_environment_binding_sink(branch_sink.clone());
    let mut branch = projection("branch prompt", false);
    branch_host
        .install_frozen_session_projection("branch-thread", branch.clone(), None, true, None)
        .await
        .expect("B1 baseline without materialized prefix");
    let stale = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B1 resident context");
    branch.revision.0 += 1;
    branch.environment = committed_environment
        .lock()
        .unwrap()
        .clone()
        .expect("B1 Store-read Resident returned by the binding sink");
    assert_eq!(branch_sink.calls.load(Ordering::SeqCst), 1, "B1/E3");
    branch.request_context = vec![Message::text(
        MessageId("source-prefix".into()),
        Role::Assistant,
        "E2E_SOURCE_ONLY_exact",
    )];
    branch_host
        .install_frozen_session_projection("branch-thread", branch.clone(), None, true, None)
        .await
        .expect("B1 late materialized prefix");
    let rebuilt = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B1 rebuilt context");
    assert!(
        !Arc::ptr_eq(&stale, &rebuilt),
        "B1/E1 stale context retired"
    );
    assert_eq!(
        rebuilt.attempt_context.request_context, branch.request_context,
        "B1/E1 exact request-only prefix"
    );
    branch_host
        .install_frozen_session_projection("branch-thread", branch, None, true, None)
        .await
        .expect("B2 identical replay");
    let replayed = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B2 resident context");
    assert!(
        Arc::ptr_eq(&rebuilt, &replayed),
        "B2/E2 no redundant rebuild"
    );
    assert_eq!(branch_sink.calls.load(Ordering::SeqCst), 1, "B2/E3");

    {
        // Frozen-projection Resource-generation cause/effect decision table.
        // C1=projection has an explicit non-legacy Resource generation;
        // C2=resources are non-default and must be staged before Environment
        // creation. E1=the Runtime's exact transition preserves that generation;
        // E2=active completion stays absent until the physical transition; E3=the
        // staged sandbox requirements remain available. R1 C1+C2=>E1,E2,E3.
        assert!(
            host.session_slots
                .read("flow-thread", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().revision == 7
                                && transition.desired().resources
                                    == awaken_session_resource_contract::ResolvedSessionResources::default()
                        })
                })
                .unwrap_or(false),
            "R1 stages the exact SessionResourceState generation without forging completion"
        );

        let spec = host.sandbox_spec("flow-thread");
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.env.len(), 1);
        assert_eq!(
            spec.network,
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            "Workdir does not advertise strict network isolation"
        );
        assert!(
            spec.deny_tool_egress,
            "the Workdir tool wrapper retains the frozen deny intent"
        );
        assert_eq!(
            host.thread_session_prompts("flow-thread"),
            vec!["Use the bound Flow project."]
        );
    }

    async {
        // Cause graph: C1 frozen Session prompt; C2 current attempt context absent;
        // C3 activation already carries the exact explicit System message. Effects:
        // E1 project exactly one request-only context message; E2 leave Thread input
        // unchanged; E3 deduplicate equal explicit input without deleting it.
        //
        // | Rule | C1 | C2 | C3 | E |
        // | P1   | 0  | *  | *  | 0 |
        // | P2   | 1  | 1  | 0  | 1 |
        // | P3   | 1  | 1  | 1  | 0 (deduplicate) |
        // | P4   | 1  | later Run | 0 | E1 again, never from history |
        host.run(None, "no-baseline", user("P1")).await.expect("P1");
        host.install_frozen_session_projection(
            "prompt-thread",
            projection("Use the bound Flow project.", false),
            None,
            true,
            None,
        )
        .await
        .expect("P2/P4 projection");
        host.run_thread_extension_after_admission(None, "prompt-thread", user("P2"))
            .await
            .expect("P2");
        host.run_thread_extension_after_admission(None, "prompt-thread", user("P4"))
            .await
            .expect("P4");
        host.install_frozen_session_projection(
            "deduplicated",
            projection("exact prompt", false),
            None,
            true,
            None,
        )
        .await
        .expect("P3 projection");
        host.run_thread_extension_after_admission(
            None,
            "deduplicated",
            vec![
                Message::text(
                    MessageId("system-existing".into()),
                    Role::System,
                    "exact prompt",
                ),
                Message::text(MessageId("user-existing".into()), Role::User, "P3"),
            ],
        )
        .await
        .expect("P3");
        {
            let requests = observed.lock().unwrap();
            let prompt_count =
                |request: &ChatRequest, prompt: &str| {
                    request
                .messages
                .iter()
                .filter(|message| message.role == Role::System)
                .flat_map(|message| message.content.iter())
                .filter(|content| matches!(content, ContentBlock::Text { text } if text == prompt))
                .count()
                };
            assert_eq!(
                prompt_count(&requests[0], "Use the bound Flow project."),
                0,
                "P1"
            );
            assert_eq!(
                prompt_count(&requests[1], "Use the bound Flow project."),
                1,
                "P2"
            );
            assert_eq!(
                prompt_count(&requests[2], "Use the bound Flow project."),
                1,
                "P4 fresh request context is projected exactly once"
            );
            assert_eq!(prompt_count(&requests[3], "exact prompt"), 1, "P3");
        }
        assert!(
            managed
                .thread_reads()
                .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                    "ThreadReadRuntime is unsupported by this runtime"
                ))
                .expect("runtime role")
                .committed_messages("prompt-thread")
                .await
                .expect("prompt Thread truth")
                .iter()
                .all(|message| message.text_content() != "Use the bound Flow project."),
            "E2 derived Session context must never enter a Thread delta"
        );
    }
    .await;

    async {
        // Request-context decision rule C4: a frozen projection carries one
        // materialized source prefix. Effect E4: the model sees it before current
        // input while the target's committed Thread contains neither a copied source
        // message nor any second transcript authority.
        let mut contextual = projection("", false);
        contextual.request_context = vec![Message::text(
            MessageId("source-prefix".into()),
            Role::User,
            "prior branch context",
        )];
        host.install_frozen_session_projection("context-thread", contextual, None, true, None)
            .await
            .expect("C4 projection");
        host.run_thread_extension_after_admission(
            None,
            "context-thread",
            user("current branch input"),
        )
        .await
        .expect("C4 run");
        let request = observed
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("C4 request");
        let user_text = request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            user_text,
            ["prior branch context", "current branch input"],
            "C4/E4"
        );
        assert!(
            managed
                .thread_reads()
                .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                    "ThreadReadRuntime is unsupported by this runtime"
                ))
                .expect("runtime role")
                .committed_messages("context-thread")
                .await
                .expect("C4 committed truth")
                .iter()
                .all(|message| message.id.0 != "source-prefix"),
            "C4/E4 request context is not copied into target truth"
        );
    }
    .await;

    async {
        let replacement = projection("different", true);
        assert!(
            host.install_frozen_session_projection("flow-thread", replacement, None, true, None)
                .await
                .is_err(),
            "a bound Session cannot switch frozen baselines"
        );
    }
    .await;

    async {
        // Frozen-config / live-claim cause/effect decision table:
        // | Rule | Baseline/config | Claim epoch | Effect |
        // |---|---|---|---|
        // | C1 | first exact projection | 1 | install and verify with epoch 1 |
        // | C2 | exact projection replay | 2 | retain config, reverify with epoch 2 |
        // | C3 | different baseline | any | reject before replacing config |
        let mut repository_projection = projection("repository claim", false);
        repository_projection.resources = effective_repository(
            "repository-claim",
            "https://example.invalid/repository.git",
            "/workspace/repository",
            None,
        );
        let claim = |epoch| awaken_dispatch::RunClaim {
            run_id: RunId("repository-claim-run".into()),
            owner: "repository-worker".into(),
            epoch,
        };
        host.install_frozen_session_projection(
            "repository-claim-thread",
            repository_projection.clone(),
            Some(&claim(1)),
            true,
            None,
        )
        .await
        .expect("C1 first claim");
        host.install_frozen_session_projection(
            "repository-claim-thread",
            repository_projection,
            Some(&claim(2)),
            true,
            None,
        )
        .await
        .expect("C2 replacement claim");
        assert_eq!(
            repository_claims
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|claim| claim.as_ref().map(|claim| claim.epoch))
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)],
            "C2 must not retain the stale claim from C1"
        );
    }
    .await;
}

#[tokio::test]
async fn managed_user_run_reservation_precedes_physical_environment_realization() {
    use awaken_dispatch::DispatchQueue as _;

    // Cause/effect graph: C1 a cold Managed Session has no Runtime or
    // Environment; C2 its complete command is exact or operation/capabilities
    // change; C3 durable phase is Reserved, Activated, Recovery, or Completed;
    // C4 only trace context changes. Effects: E1 persist one unclaimable
    // reservation; E2 create no Environment; E3 evict the envelope-only Runtime
    // so the claimed Worker must rebuild from frozen dispatch truth; E4 freeze
    // application restrictions and current command fingerprint in that row; E5
    // exact/C4 retries report the phase; E6 command changes conflict against
    // both live and completed durable evidence as BadRequest.
    // Constraint: physical realization remains in the existing claimed Worker
    // path; reservation introduces no second executor or store.
    //
    // | Rule | Command | Phase | Effects |
    // |---|---|---|---|
    // | R1 | exact | Reserved/Activated/Recovery/Completed | E1-E5 |
    // | R2 | operation changed | Reserved/Completed | E6 |
    // | R3 | capabilities empty to present | Reserved/Completed | E6 |
    // | R4 | trace changed | Reserved | E5 |
    let thread = "cold-session-reservation";
    let run_id = RunId("cold-session-reservation-run".into());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::AnyDispatchStore::open_sqlite_in_memory(clock.clone())
            .expect("reservation dispatch"),
    );
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone()));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    let command = awaken_session_execution_contract::AdmitSessionRun {
        session_id: thread.into(),
        agent_id: "assistant".into(),
        operation_id: "cold-session-reservation-op".into(),
        run_id: run_id.clone(),
        messages: vec![Message::new(
            MessageId::session_event_input(thread, "cold-session-reservation-op"),
            Role::User,
            vec![ContentBlock::text("reserve before realization")],
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: awaken_session_execution_contract::SessionRunExecutionRequirements {
            tool_capability_narrowing:
                awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
            required_worker_capabilities: std::collections::BTreeSet::from([
                "application:test-session-envelope/v1".to_string(),
            ]),
        },
        replacement: awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
    };
    let expected_fingerprint =
        awaken_session_execution_contract::SessionRunCommandFingerprint::current(&command);

    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(command.clone())
            .await
            .expect("R1 reservation"),
        awaken_session_execution_contract::SessionRunReservation::Reserved,
        "R1/E1"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(command.clone())
            .await
            .expect("R1 exact Reserved replay"),
        awaken_session_execution_contract::SessionRunReservation::AlreadyReserved,
        "R1/E5 Reserved"
    );
    let mut retraced = command.clone();
    retraced.traceparent = Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into());
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(retraced)
            .await
            .expect("R4 trace-only replay"),
        awaken_session_execution_contract::SessionRunReservation::AlreadyReserved,
        "R4/E5"
    );
    let mut changed_operation = command.clone();
    changed_operation.operation_id = "cold-session-reservation-other-op".into();
    assert!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(changed_operation)
            .await
            .is_err(),
        "R2/E6 operation change conflicts"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .session_run_state(
                &SessionId::from(thread),
                &RunId("cold-session-reservation-run".into())
            )
            .await
            .expect("R1 read pre-claim state"),
        None,
        "R1/E2 recovery read observes committed Thread truth without realization"
    );
    let rows = dispatch
        .list_dispatches()
        .await
        .expect("R1 inspect dispatch");
    assert_eq!(rows.len(), 1, "R1/E1 exact reservation");
    assert_eq!(
        rows[0].state,
        awaken_dispatch::DispatchState::Reserved,
        "R1/E1 remains unclaimable before activity activation"
    );
    assert!(host.session_environment(thread).await.is_none(), "R1/E2");
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
            .is_none(),
        "R1/E3"
    );

    let mut empty_capabilities = command.clone();
    empty_capabilities.run_id = RunId("cold-session-empty-capabilities-run".into());
    empty_capabilities.operation_id = "cold-session-empty-capabilities-op".into();
    empty_capabilities.messages = vec![Message::new(
        MessageId::session_event_input(thread, "cold-session-empty-capabilities-op"),
        Role::User,
        vec![ContentBlock::text("empty capability command")],
    )];
    empty_capabilities
        .execution_requirements
        .required_worker_capabilities
        .clear();
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(empty_capabilities.clone())
            .await
            .expect("R3 reserve empty capabilities"),
        awaken_session_execution_contract::SessionRunReservation::Reserved,
        "R3 precondition"
    );
    let mut added_capability = empty_capabilities.clone();
    added_capability
        .execution_requirements
        .required_worker_capabilities
        .insert("application:added-after-reservation/v1".into());
    assert!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(added_capability)
            .await
            .is_err(),
        "R3/E6 empty-to-present capability change conflicts"
    );
    let mut recovery_command = command.clone();
    recovery_command.run_id = RunId("cold-session-recovery-run".into());
    recovery_command.operation_id = "cold-session-recovery-op".into();
    recovery_command.messages = vec![Message::new(
        MessageId::session_event_input(thread, "cold-session-recovery-op"),
        Role::User,
        vec![ContentBlock::text("recover reserved command")],
    )];
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(recovery_command.clone())
            .await
            .expect("R1 reserve recovery probe"),
        awaken_session_execution_contract::SessionRunReservation::Reserved,
        "R1/E5 Recovery precondition"
    );
    clock.set(u64::MAX);
    let recovery_claim = dispatch
        .claim_run(
            &recovery_command.run_id,
            "reservation-recovery-owner",
            1_000,
            &Default::default(),
        )
        .await
        .expect("R1 claim expired recovery probe")
        .expect("R1 expired reservation is repairable");
    assert!(
        recovery_claim.session_activity_admission_required,
        "R1 Recovery claim stays admission-only"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(recovery_command)
            .await
            .expect("R1 exact Recovery replay"),
        awaken_session_execution_contract::SessionRunReservation::RecoveryClaimed,
        "R1/E5 Recovery"
    );
    assert_eq!(
        dispatch
            .resolve_claimed_session_run_reservation(
                &awaken_dispatch::RunClaim::from(&recovery_claim.lease),
                awaken_dispatch::SessionRunReservationResolution::Rejected,
            )
            .await
            .expect("R1 remove recovery probe"),
        awaken_dispatch::SettleOutcome::Applied,
        "R1 Recovery cleanup"
    );

    assert_eq!(
        dispatch
            .activate_session_run_reservation(
                &RunId("cold-session-reservation-run".into()),
                &ThreadId(thread.into()),
                1,
            )
            .await
            .expect("R1 activate exact reservation"),
        awaken_dispatch::SessionRunReservationActivation::Activated,
        "R1/E4 inspection uses the ordinary post-activity transition"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(command.clone())
            .await
            .expect("R1 exact Activated replay"),
        awaken_session_execution_contract::SessionRunReservation::AlreadyActivated {
            session_activity_epoch: 1,
        },
        "R1/E5 Activated"
    );
    let mut manifest = awaken_dispatch::WorkerManifest::default();
    // R1 assumes an executable current Worker; protocol rejection must not
    // mask the reservation/physical-realization ordering being exercised.
    manifest.runtime_protocol.min = 1;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest
        .capabilities
        .insert("application:test-session-envelope/v1".into());
    let capability_fingerprint = manifest
        .fingerprint()
        .expect("R1 deterministic Worker capability fingerprint");
    let worker = awaken_dispatch::WorkerSnapshot {
        identity: awaken_dispatch::WorkerIdentity::new("inspection-worker", "boot-1", 1),
        state: awaken_dispatch::WorkerState::Ready,
        manifest,
        capability_fingerprint,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        expires_at_ms: 10_000,
    };
    clock.set(0);
    let claimed = dispatch
        .claim_compatible(&worker, 1_000)
        .await
        .expect("R1 inspect executable dispatch")
        .expect("R1 exact executable dispatch");
    assert_eq!(
        claimed.request.activation.tool_capability_narrowing,
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
        "R1/E4 application restrictions only narrow Session tool authority"
    );
    assert_eq!(
        claimed.request.placement.location,
        awaken_dispatch::ExecutionLocation::RemoteRequired,
        "R1/E4 application protocol cannot fall back to a generic local executor"
    );
    assert!(
        claimed
            .request
            .placement
            .required_capabilities
            .contains("application:test-session-envelope/v1"),
        "R1/E4 capability is frozen in the one durable dispatch"
    );
    assert_eq!(
        claimed.request.session_command_fingerprint.as_ref(),
        Some(&expected_fingerprint),
        "R1/E4 fingerprint is computed from the raw command before projection"
    );
    assert_eq!(
        dispatch
            .settle(
                &run_id,
                claimed.lease.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("R1 settle Managed reservation"),
        awaken_dispatch::SettleOutcome::Applied,
        "R1 Completed precondition"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(command)
            .await
            .expect("R1 exact Completed replay"),
        awaken_session_execution_contract::SessionRunReservation::Completed,
        "R1/E5 Completed"
    );
    let completion = dispatch
        .completion_events_after(0, usize::MAX)
        .await
        .expect("R1 completion fingerprint query")
        .into_iter()
        .find(|completion| completion.run_id == run_id)
        .expect("R1 Managed completion exists");
    assert_eq!(
        completion.request_fingerprint.as_deref(),
        Some(expected_fingerprint.as_str()),
        "R1/E4 completion retains the current compact identity"
    );

    assert_eq!(
        dispatch
            .activate_session_run_reservation(
                &empty_capabilities.run_id,
                &ThreadId(thread.into()),
                2,
            )
            .await
            .expect("R3 activate empty-capability completion probe"),
        awaken_dispatch::SessionRunReservationActivation::Activated,
        "R3 Completed precondition"
    );
    clock.set(0);
    let empty_capabilities_claim = dispatch
        .claim_run(
            &empty_capabilities.run_id,
            "empty-capability-completion-worker",
            1_000,
            &Default::default(),
        )
        .await
        .expect("R3 claim empty-capability completion probe")
        .expect("R3 activated empty-capability probe is claimable");
    assert_eq!(
        dispatch
            .settle(
                &empty_capabilities.run_id,
                empty_capabilities_claim.lease.epoch,
                awaken_dispatch::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("R3 settle empty-capability completion probe"),
        awaken_dispatch::SettleOutcome::Applied,
        "R3 Completed precondition"
    );
    assert_eq!(
        managed
            .coordination()
            .ok_or_else(|| awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime"
            ))
            .expect("runtime role")
            .reserve_session_run(empty_capabilities.clone())
            .await
            .expect("R3 exact empty-capability Completed replay"),
        awaken_session_execution_contract::SessionRunReservation::Completed,
        "R3/E5 Completed"
    );
    let mut completed_changed_operation = empty_capabilities.clone();
    completed_changed_operation.operation_id =
        "cold-session-empty-capabilities-completed-other-op".into();
    let completed_operation_error = managed
        .coordination()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .reserve_session_run(completed_changed_operation)
        .await
        .expect_err("R2 completed operation change conflicts");
    assert_eq!(
        completed_operation_error.kind,
        awaken_session_error_contract::RunErrorKind::BadRequest,
        "R2/E6 Completed"
    );
    let mut completed_added_capability = empty_capabilities;
    completed_added_capability
        .execution_requirements
        .required_worker_capabilities
        .insert("application:added-after-completion/v1".into());
    let completed_capability_error = managed
        .coordination()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "CoordinationRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .reserve_session_run(completed_added_capability)
        .await
        .expect_err("R3 completed empty-to-present capability change conflicts");
    assert_eq!(
        completed_capability_error.kind,
        awaken_session_error_contract::RunErrorKind::BadRequest,
        "R3/E6 Completed"
    );
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn acp_execution_rebuilds_an_envelope_only_reservation_context() {
    // Test design: acp_execution_rebuilds_an_envelope_only_reservation_context
    // Cause/effect graph: C1 an ACP publication; C2 reservation preflight leaves
    // an envelope-only cached context; C3 execution wins the race before the
    // reservation caller evicts it; C4 the admitted Namespace provider preserves
    // one path for arbitrary ACP processes; C5 the direct live-authored Skill
    // source exists while initially empty and therefore requires the canonical
    // ACP tool-export adapter; C6 the publication selects one Hand descriptor;
    // C7 it also selects one ClientExecuted descriptor.
    // Effects: E1 execution replaces that context; E2 the replacement owns a
    // physical Environment; E3 the coordination envelope performs no ACP MCP
    // export; E4 execution exports the exact descriptor/executor set once.
    // Decision table: reservation+C1-C6=>E3; execution+C1-C6=>E1+E2+E4;
    // ACP+C1-C3+split-Workdir=>fail before effects (owned by
    // `projected_acp_rejects_a_provider_with_split_tool_and_process_paths`).
    // Every execution request rebuilds a coordination envelope; Native
    // on-tool-use/A2A may remain physically deferred only after that rebuild.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new("", "", "acp:opencode"),
            ),
        )
        .tool(
            crate::config::hand_tool_descriptors()
                .into_iter()
                .find(|descriptor| descriptor.id == "read")
                .expect("canonical read descriptor"),
        )
        .tool(crate::config::client_tool_descriptor("desktop_search"))
        .build();
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("one exact ACP publication");
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]),
    ));
    let sandbox_root = tempfile::tempdir().expect("Namespace fixture root");
    let exporter = Arc::new(crate::acp_tool_export::RecordingAcpToolExporter::default());
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_agent_publications(Arc::new(publications))
        .with_acp(Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(
            source,
        )))
        .with_acp_tool_exporter(exporter.clone());
    raw_host.env.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root.path(),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let (host, _managed) = dispatch_test_host(raw_host);

    let reserved = host
        .ctx_for_session_coordination("acp-reservation-race", Some("assistant"))
        .await
        .expect("reservation preflight");
    assert!(reserved.env.is_none(), "C1+C2: reservation is effect-free");
    assert!(exporter.0.lock().unwrap().is_empty(), "E3 no ACP export");

    let executable = host
        .ctx_for("acp-reservation-race", Some("assistant"))
        .await
        .expect("C3 execution rebuild");
    assert!(
        !Arc::ptr_eq(&reserved, &executable),
        "E1: execution cannot reuse the envelope-only ACP context"
    );
    assert!(
        executable.env.is_some(),
        "E2: exact ACP routing is paired with its Session Environment"
    );
    assert_eq!(
        exporter.0.lock().unwrap().as_slice(),
        &[(
            "awaken_session".into(),
            vec!["read".into(), "desktop_search".into()],
        )],
        "E4 exact execution-owned Hand and ClientExecuted export"
    );
}

#[tokio::test]
async fn coordinator_reservation_does_not_adopt_a_worker_owned_environment() {
    // Cause/effect decision table: C1 the durable Session projection names a
    // Resident Environment; C2 this Coordinator has no adopted physical owner;
    // C3 the caller is reservation or execution. Effects: E1 reservation builds
    // only the immutable dispatch envelope and creates/adopts no Environment;
    // E2 execution remains fail-closed until the claiming Worker adopts the
    // exact binding. Rules: R1=C1+C2+reservation=>E1;
    // R2=C1+C2+execution=>E2; R3=C1+C2+Outcome prepare/read=>E1. Outcome
    // orchestration is a Coordinator operation even after a Worker has used
    // its durable Environment; preparing/querying it cannot require adoption.
    // R4=C1+C2+child-report/replay=>one self-affine dispatch, E1;
    // R5=C1+C2+invalid report provenance=>reject without another dispatch.
    // The same context constructor and existing dispatch identity own these rules, so
    // this test does not introduce a second topology or recovery mechanism.
    let thread = "worker-owned-reservation";
    let dispatch = Arc::new(
        awaken_dispatch::AnyDispatchStore::open_sqlite_in_memory(Arc::new(
            awaken_lease_testkit::ManualClock::new(0),
        ))
        .expect("report continuation queue"),
    );
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone()));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    managed
        .install_complete_test_session(
            thread,
            awaken_session_configuration_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("install Worker-owned Session projection");
    host.install_session_environment_owner_projection(
        thread,
        host.local_workspace(),
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "worker-owned-binding".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .expect("project exact durable Worker binding");

    let reserved = host
        .ctx_for_session_coordination(thread, Some("assistant"))
        .await
        .expect("R1 reservation does not require local adoption");
    assert!(reserved.env.is_none(), "R1/E1 no physical Environment");
    assert!(host.session_environment(thread).await.is_none(), "R1/E1");

    host.prepare_outcome(thread, "worker-owned-outcome", "deliver", "rubric", 2)
        .await
        .expect("R3 Outcome preparation uses the coordination envelope");
    assert!(
        host.committed_outcome_projection(thread, "worker-owned-outcome")
            .await
            .expect("R3 nonterminal Outcome read needs no physical Environment")
            .is_none()
    );
    assert!(host.session_environment(thread).await.is_none(), "R3/E1");

    let child_run = RunId("worker-owned-child-report".into());
    let report = awaken_session_execution_contract::SessionAgentReportContinuation {
        session_id: thread.into(),
        source_thread_id: ThreadId("worker-owned-child".into()),
        source_run_id: child_run.clone(),
        session_activity_epoch: 7,
        message: Message::text(
            MessageId::agent_thread_report(&child_run),
            Role::User,
            "report files are ready",
        ),
    };
    // Eviction also exercises cold Coordinator reconstruction, rather than
    // accidentally succeeding through a previously cached envelope.
    host.evict_session_for_rebuild(thread).await;
    for _ in 0..2 {
        host.continue_session_agent_report(report.clone())
            .await
            .expect("R4 report admission needs no local Environment");
    }
    let queued = dispatch.list_dispatches().await.expect("R4 queue");
    assert_eq!(queued.len(), 1, "R4 exact replay has one dispatch");
    assert_eq!(queued[0].thread_id.0, thread, "R4 primary target");
    assert_eq!(queued[0].session_thread_id, Some(ThreadId(thread.into())));
    assert_eq!(queued[0].session_activity_epoch, Some(7));
    assert!(host.session_environment(thread).await.is_none(), "R4/E1");
    let mut invalid = report;
    invalid.message.role = Role::Assistant;
    assert!(host.continue_session_agent_report(invalid).await.is_err());
    assert_eq!(dispatch.list_dispatches().await.unwrap().len(), 1, "R5");

    host.evict_session_for_rebuild(thread).await;
    let error = match host
        .ctx_for_snapshot(thread, Some("assistant"), Some(reserved.config.clone()))
        .await
    {
        Ok(_) => panic!("R2 execution must await exact Worker adoption"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("durable Environment was not adopted before context construction"),
        "R2/E2: {error:?}"
    );
}

#[tokio::test]
async fn rebuilt_coordination_context_never_borrows_a_retained_environment() {
    // Cause/effect graph: C1 execution created one resident Environment; C2 its
    // rebuildable Runtime cache is evicted while the independent Environment
    // owner survives; C3 coordination or execution reconstructs the context.
    // Effects: E1 coordination omits the resident Environment without replacing
    // or disposing it; E2 subsequent execution reuses that exact Environment.
    // Constraint: the Session Environment owner remains the sole physical source
    // of truth. Rules: R1=C1+C2+coordination=>E1;
    // R2=C1+C2+execution=>E2.
    let thread = "retained-environment-coordination";
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let _managed = install_test_dispatch_runtime(&host);

    let executable = host
        .ctx_for(thread, None)
        .await
        .expect("C1 create executable context");
    let retained = executable.env.clone().expect("C1 resident Environment");
    host.evict_session_for_rebuild(thread).await;

    let coordination = host
        .ctx_for_session_coordination(thread, None)
        .await
        .expect("R1 rebuild coordination context");
    assert!(coordination.is_coordination_envelope(), "R1/E1 purpose");
    assert!(coordination.env.is_none(), "R1/E1 no physical borrow");
    assert!(
        Arc::ptr_eq(
            &retained,
            &host
                .session_environment(thread)
                .await
                .expect("R1/E1 physical owner survives"),
        ),
        "R1/E1 exact owner is retained independently"
    );

    let rebuilt = host
        .ctx_for(thread, None)
        .await
        .expect("R2 rebuild executable context");
    assert!(
        Arc::ptr_eq(
            &retained,
            rebuilt.env.as_ref().expect("R2/E2 Environment reused"),
        ),
        "R2/E2 exact retained owner"
    );
}

#[tokio::test]
async fn claimed_inference_only_context_keeps_scope_without_a_resource_transition() {
    // Cause/effect table: C1 a complete tool-free Native snapshot; C2 no
    // Session/Environment/Resource projection; C3 a registered claim's exact
    // publication identity. R1 C1+C2+C3 -> context without a physical Sandbox
    // or fabricated Resource transition, retaining Execution purpose; R2 dispatch still carries Workspace
    // scope even without resource bytes. The live template Outcome E2E verifies
    // the same path through the remote Worker and official Managed API.
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "model"));
    install_test_session_application(&host);
    let _managed = install_test_dispatch_runtime(&host);
    let thread = "inference-only-claimed";
    host.register_thread_workspace(thread, "workspace-inference");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("judge")
        .model(test_model_binding())
        .build();
    let publication =
        crate::agent_catalog::exact_run_publication_source(&snapshot, &[], "workspace-inference")
            .unwrap();
    let attempt = crate::host::session_ctx::ClaimedRuntimeInput {
        identity: crate::host::session_ctx::RuntimePublicationIdentity::from_publications(
            &snapshot,
            &[],
            "model",
        ),
        publications: publication,
        effective_model_ref: "model".into(),
    };
    let ctx = host
        .ctx_for_claimed_snapshot(thread, Some("judge"), snapshot.clone(), attempt)
        .await
        .expect("R1 no Resource transition is needed for inference");
    assert_eq!(
        ctx.purpose,
        crate::host::session_ctx::RuntimeContextPurpose::Execution,
        "R1 omitting a Sandbox must not downgrade the claim to coordination"
    );
    assert!(ctx.env.is_none(), "R1 no physical environment");
    assert!(host.session_environment(thread).await.is_none());
    let dispatch = host
        .resolved_dispatch(awaken_runtime_contract::RunActivation::new(
            RunId("inference-only-run".into()),
            ThreadId(thread.into()),
            snapshot,
            Vec::new(),
        ))
        .unwrap();
    assert!(
        dispatch.session_resources.is_none(),
        "R2 no fabricated resources"
    );
    assert_eq!(
        dispatch.execution_scope,
        Some(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-inference"),
        )),
        "R2 exact non-default Workspace survives dispatch"
    );
}
