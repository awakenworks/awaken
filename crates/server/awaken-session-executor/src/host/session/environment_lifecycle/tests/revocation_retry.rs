// @formal evidence session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Revocation failures, retained retry inputs, and cancelled adoption retries.

use super::*;
use awaken_session_configuration_contract::mcp as session_mcp;
use awaken_session_configuration_contract::resource as session_resource;

async fn install_active_worker_relay(
    host: &SharedHost,
    thread: &str,
) -> (awaken_session_mcp::McpRelay, session_mcp::McpGenerationRef) {
    let generation = crate::test_mcp::mcp_fence(thread, "docs", 1, "runtime-1", 1, u64::MAX);
    let request = session_mcp::StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let server = awaken_session_mcp::McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test/sse".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("relay-secret")),
            refresh: None,
        },
    };
    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    assert!(host.mcp.install_relay(relay.clone()));
    relay.set_route(&generation, &server);
    host.insert_mcp_projection(awaken_session_mcp::McpGenerationProjection {
        receipt: session_mcp::McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: Some(
                awaken_credential_contract::CredentialRealizationKind::WorkerRelay,
            ),
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(server),
        native_wiring: None,
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: awaken_session_mcp::McpProjectionState::Active,
    })
    .unwrap();
    (relay, generation)
}

fn retry_input_mount() -> awaken_provisioning_contract::MountRequirement {
    awaken_provisioning_contract::MountRequirement {
        mount_id: "retry-input".into(),
        source: awaken_provisioning_contract::MountSource::InlineBytes {
            contents: b"retry".to_vec(),
            content_hash: None,
        },
        mount_path: ".mnt/retry-input".into(),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::Session,
        required: true,
    }
}

#[tokio::test]
async fn conflicting_revocation_preserves_active_worker_relay_until_compatible_drain() {
    // Cause/effect decision table: C1 an exact Bound owner is already Retiring
    // for the highest-priority Terminal cause; C2 its WorkerRelay generation is
    // Active with an open exact Route; C3 lower-priority revocation arrives;
    // C4 the canonical exact MCP drain is later invoked compatibly. Effects:
    // E1 C1+C2+C3 returns Err with the same Retiring owner; E2 exact Route,
    // open fence, and Active projection are unmodified and still admit calls;
    // E3 C4 alone closes/removes the Route, marks Removed, and returns proof.
    // Rules: R1=C1+C2+C3=>E1+E2; R2=R1+C4=>E3. Retirement admission therefore
    // precedes every MCP mutation; an errored revocation cannot consume retry
    // authority from either existing owner.
    let thread = "conflicting-revocation-retains-relay";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.install_test_resident_session_environment(thread, environment);
    let (relay, mcp_generation) = install_active_worker_relay(&host, thread).await;
    let route_before = relay.route_url(&mcp_generation).unwrap();
    let terminal = host
        .realizer
        .retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-effect".into(),
            },
            RetirementSelection::Current,
        )
        .unwrap()
        .unwrap();

    let error = host
        .revoke_all_session_realizations()
        .await
        .expect_err("R1 conflicting revocation must fail before MCP drain");
    assert!(error.to_string().contains("lower-priority"), "R1/E1");
    assert!(
        matches!(
            host.installed_sessions.realizations()
                .read_realization(thread, |r| r.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current))
                if current.exact_matches(&terminal.owner)
        ),
        "R1/E1 exact Terminal owner retained"
    );
    assert!(
        host.mcp_projection(&mcp_generation)
            .is_some_and(|projection| projection.state
                == awaken_session_mcp::McpProjectionState::Active
                && projection.server.is_some()),
        "R1/E2 Active MCP owner unchanged"
    );
    assert_eq!(
        relay.route_url(&mcp_generation).as_deref(),
        Some(route_before.as_str()),
        "R1/E2 exact Route retained"
    );
    let still_open = relay
        .route_call_fence(&mcp_generation)
        .unwrap()
        .try_enter()
        .expect("R1/E2 route fence remains open");
    drop(still_open);

    let proof = host
        .drain_mcp_projections(thread, std::slice::from_ref(&mcp_generation))
        .await
        .expect("R2 compatible exact MCP drain");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&mcp_generation),
        "R2/E3"
    );
    assert!(relay.route_url(&mcp_generation).is_none(), "R2/E3");
    assert_eq!(
        host.mcp_projection(&mcp_generation).unwrap().state,
        awaken_session_mcp::McpProjectionState::Removed,
        "R2/E3"
    );
}

#[tokio::test]
async fn failed_revocation_retains_provider_mount_and_pending_owner_for_retry() {
    // Cause/effect table: C1 durable pending binding has no local Arc; C2 exact
    // frozen provider and SandboxSpec mount inputs exist; C3 provider adoption
    // cannot be completed; C4 revocation is attempted before ordinary runtime
    // authority has been released;
    // C5 an exact WorkerRelay Route and Active MCP projection already exist.
    // Effects: E1 return Err; E2 retain pending binding; E3 retain the exact
    // provider publication and mount/environment projection needed by the next
    // terminal/revocation retry; E4 retain the open Route/fence and Active MCP
    // owner without an early drain. Rule R19/R20: C1+C2+C3+C4+C5=>E1+E2+E3+E4.
    let thread = "revocation-retains-retry-authority";
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    host.installed_sessions.replace_frozen_with(thread, |next| {
        next.published_snapshot = Some(snapshot.clone())
    });
    host.realizer.staging.register_thread_resources(
        thread,
        crate::staged_resources::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    let environment_projection = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: thread.into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "revocation-environment".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::None,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    host.install_environment_projection(thread, &environment_projection)
        .expect("C2 Environment projection");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "not-a-sandbox-handle".into(),
            effect_id: Some("durable-effect".into()),
            generation: Some(generation(thread)),
            idle_since_unix_ms: None,
        },
    )
    .expect("C1 pending owner");
    let (relay, mcp_generation) = install_active_worker_relay(&host, thread).await;
    let route_before = relay.route_url(&mcp_generation).unwrap();
    let spec_before = host.realizer.sandbox_spec(thread);
    let lifecycle = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    assert!(
        host.retire_session_environment_for_revocation(thread)
            .await
            .is_err(),
        "R19/E1"
    );
    assert_eq!(
        host.realizer.durable_environment_binding(thread).as_deref(),
        Some("not-a-sandbox-handle"),
        "R20/E2"
    );
    assert_eq!(
        host.realizer.sandbox_spec(thread),
        spec_before,
        "R20/E3 mounts/projection"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .published_snapshot(thread)
            .map(|snapshot| snapshot.id),
        Some(snapshot.id),
        "R20/E3 provider publication"
    );
    assert!(
        host.mcp_projection(&mcp_generation)
            .is_some_and(|projection| projection.state
                == awaken_session_mcp::McpProjectionState::Active
                && projection.server.is_some()),
        "R20/E4 Active MCP owner retained"
    );
    assert_eq!(
        relay.route_url(&mcp_generation).as_deref(),
        Some(route_before.as_str()),
        "R20/E4 exact Route retained"
    );
    let still_open = relay
        .route_call_fence(&mcp_generation)
        .unwrap()
        .try_enter()
        .expect("R20/E4 route fence remains open");
    drop(still_open);
}

#[tokio::test]
async fn live_revocation_retains_retiring_owner_and_its_retry_inputs() {
    // Cause/effect rule: C1 revocation selects one exact Resident Arc; C2 stop
    // succeeds but status remains live; C3 ordinary runtime projections are
    // revoked; C4 Environment quiescence already closed MCP admission. Effects:
    // E1 retain the exact hidden Revocation Retiring owner; E2 retain its frozen
    // provider publication and SandboxSpec mount inputs for terminal/revocation
    // retry; E3 preserve the exact closed fence so revocation cannot masquerade
    // as an authoritative restore/source-disposal reopen. Only an exact
    // Terminated observation may clear C1, so C2 is not a successful physical
    // cleanup; only the canonical durable projection proof may clear C4.
    // C5 a live realization lease exists before stop => E4 retained owner reset
    // preserves Closed(lease), never Uninstalled/Live or direct-assertion fallback.
    let thread = "revocation-retiring-retry-authority";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    let lease = session_resource::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    host.install_session_realization_lease(thread, lease.clone())
        .unwrap();
    host.installed_sessions.replace_frozen_with(thread, |next| {
        next.published_snapshot = Some(snapshot.clone())
    });
    host.realizer.staging.register_thread_resources(
        thread,
        crate::staged_resources::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    host.install_test_resident_session_environment(thread, environment.clone());
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        1,
        u64::MAX,
        "environment",
        "image",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let fence = awaken_session_mcp::McpQuiescenceAdmissionFence::new(
        &operation,
        "source-effect",
        &serde_json::to_string(&environment.lifecycle().handle()).unwrap(),
        &generation,
    );
    host.installed_sessions
        .realizations()
        .close_mcp_realization_admission(thread, fence.clone())
        .unwrap();
    let spec_before = host.realizer.sandbox_spec(thread);
    let lifecycle = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    host.retire_session_environment_for_revocation(thread)
        .await
        .expect("C2 live revocation");
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(thread, |r| r.authority.clone()),
        Some(crate::installed_session::SessionRealizationAuthority::Closed(Some(lease.clone()))),
        "E4"
    );
    assert!(
        host.install_session_realization_lease(thread, lease)
            .is_err()
    );
    assert!(
        host.installed_sessions.realization_leases().is_empty(),
        "E4 replay stays closed"
    );
    assert!(
        matches!(
            host.installed_sessions.realizations()
                .read_realization(thread, |r| r.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(owned),
            })) if Arc::ptr_eq(&owned.environment, &environment)
        ),
        "E1"
    );
    assert_eq!(
        host.realizer.sandbox_spec(thread),
        spec_before,
        "E2 SandboxSpec"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .published_snapshot(thread)
            .map(|current| current.id),
        Some(snapshot.id),
        "E2 provider publication"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(thread, |r| r.mcp.quiescence_fence().cloned()),
        Some(Some(fence)),
        "E3 revocation is not a quiescence-fence reopen authority"
    );
}

#[tokio::test]
async fn cancelled_adoption_retries_the_same_hidden_candidate_without_readopting() {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, BlockingBindingSink, eager_environment,
        empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
    };

    // Cause/effect decision table: C1 a frozen provider adopts one durable
    // binding; C2 the Future is cancelled inside durable persistence after
    // provider return; C3 the authorized caller retries. Effects: E1 provider
    // return is captured immediately as one hidden Candidate Arc; E2 C2
    // retains that Arc and releases the lifecycle guard;
    // E3 C3 persists and publishes the same Arc without another provider
    // adoption or by-value owner. Rule R19/R20: C1+C2=>E1+E2;
    // C1+C2+C3=>E3. Fixture constraint: both process incarnations install the
    // production DispatchSessionRuntime before constructing executable context.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-adoption-retry";
    let activation = test_activation(thread, "run-adoption-retry-publication");
    let lease = session_resource::SessionRealizationLease {
        owner: "worker-adoption-retry".into(),
        runtime_incarnation: "worker-adoption-retry:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    let mut projection = empty_frozen_projection_for_snapshot(
        "workspace",
        eager_environment(),
        &activation.snapshot,
    );
    projection.agent_publication = Some(activation.snapshot.clone());
    let first = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    let first_managed = managed_test_host(first.clone());
    let first_sink = Arc::new(BlockingBindingSink::blocked());
    first_sink.unblock();
    first_managed.install_test_environment_binding_sink(first_sink);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &first_managed,
            thread,
            projection.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: crate::installed_session::test_observed_lease(lease.clone()),
                prepare_session: true,
            },
        ),
    )
    .await
    .expect("first Realization projection must not block")
    .expect("install first complete Realization projection");
    let first_ctx = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        first.ctx_for_snapshot(thread, Some("agent-a"), Some(activation.snapshot.clone())),
    )
    .await
    .expect("first Session creation must not block")
    .expect("first Session");
    let handle = first_ctx
        .env
        .as_ref()
        .expect("first Environment")
        .lifecycle()
        .handle();
    let binding = serde_json::to_string(&handle).unwrap();
    // The source projection must carry the first sink's actual generation;
    // inventing another generation hid invalid adoption in the old publisher.
    let source_identity = first
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| match &r.environment_owner {
            SessionEnvironmentOwner::Resident(owned) => owned.identity.clone(),
            _ => panic!("first committed resident"),
        })
        .unwrap();
    let BoundSessionEnvironmentIdentity::Durable {
        effect_id,
        generation,
    } = source_identity
    else {
        panic!("first sink must return generated identity");
    };
    drop(first_ctx);
    drop(first_managed);
    drop(first);

    let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    let managed = managed_test_host(host.clone());
    let sink = Arc::new(BlockingBindingSink::blocked());
    managed.install_test_environment_binding_sink(sink.clone());
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &managed,
            thread,
            projection,
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: crate::installed_session::test_observed_lease(lease),
                prepare_session: true,
            },
        ),
    )
    .await
    .expect("retry Realization projection must not block")
    .expect("install retry complete Realization projection");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: Some(effect_id),
            generation: Some(generation),
            idle_since_unix_ms: None,
        },
    )
    .expect("durable pending adoption");

    let adoption = {
        let host = host.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            host.adopt_bound_session_environment(
                thread,
                Some(&binding),
                &host.realizer.env.session_provider,
                None,
                false,
            )
            .await
        })
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), sink.entered.notified())
        .await
        .is_err()
    {
        if adoption.is_finished() {
            panic!(
                "R19/C1 adoption failed before durable persistence: {:?}",
                adoption.await
            );
        }
        panic!("R19/C1 adoption did not reach durable persistence");
    }
    let candidate = host
        .prepared_session_environment(thread)
        .expect("R19/E1 hidden Candidate before cancellation");
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "R19/E1"
    );
    adoption.abort();
    let cancellation = match adoption.await {
        Err(error) => error,
        Ok(_) => panic!("R19/C2 adoption unexpectedly completed"),
    };
    assert!(cancellation.is_cancelled(), "R19/C2");
    let retained = host
        .prepared_session_environment(thread)
        .expect("R19/E2 hidden Candidate retained");
    assert!(candidate.exact_matches(&retained), "R19/E2");

    sink.unblock();
    let disposition = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        host.adopt_bound_session_environment(
            thread,
            Some(&binding),
            &host.realizer.env.session_provider,
            None,
            false,
        ),
    )
    .await
    .expect("R20/E3 retry must not re-adopt or block on provider ownership")
    .expect("R20/E3 retry");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::Ready,
        "R20/E3 no by-value duplicate or rebuild path"
    );
    let resident = host
        .realizer
        .session_environment(thread)
        .await
        .expect("R20/E3 published Resident");
    assert!(Arc::ptr_eq(&candidate.environment, &resident), "R20/E3");
    assert_eq!(sink.calls(), 2, "R19/R20 sink retries");
}

#[tokio::test]
async fn newer_projection_replaces_only_a_descriptor_and_preserves_every_physical_owner() {
    // C1 Vacant/pending/Candidate/Resident/Retiring; C2 old/equal vs admitted
    // newer root; C3 foreign physical binding B. R1 pending+newer accepts B;
    // R2 pending without newer proof rejects; R3 every physical phase rejects B
    // even with newer root. Revocation, discard, terminal and checkpoint each
    // retain their exact owner: root ordering never substitutes for closure.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let env = environment(&provider, "owner-A").await;
    let bound = BoundSessionEnvironment {
        identity: identity("owner-A"),
        binding: serde_json::to_string(&env.lifecycle().handle()).unwrap(),
        environment: env.clone(),
    };
    for newer in [false, true] {
        let mut pending =
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                identity: bound.identity.clone(),
                binding: bound.binding.clone(),
            });
        assert_eq!(
            pending
                .install_projection_with_resource_reservation(
                    ProjectedEnvironmentOwner::AwaitingAdoption {
                        identity: identity("owner-B"),
                        binding: "B".into()
                    },
                    newer,
                    false,
                )
                .is_ok(),
            newer,
            "R1/R2"
        );
        let mut owners = vec![
            SessionEnvironmentOwner::Resident(bound.clone()),
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                UnboundSessionEnvironment {
                    intent: None,
                    origin: UnboundSessionEnvironmentOrigin::Adoption,
                    binding: bound.binding.clone(),
                    environment: env.clone(),
                },
            )),
        ];
        for cause in [
            SessionEnvironmentRetirementCause::RealizationRevocation,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-A".into(),
            },
            SessionEnvironmentRetirementCause::CheckpointSource {
                operation: awaken_session_state_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    &awaken_session_types::SessionId::from("owner-A"),
                    "suspend",
                    &generation("owner-A"),
                    1,
                    None,
                    None,
                ),
                generation: generation("owner-A"),
            },
        ] {
            owners.push(SessionEnvironmentOwner::Retiring(
                RetiringSessionEnvironment {
                    cause,
                    owned: RetiringEnvironmentOwner::Bound(bound.clone()),
                },
            ));
        }
        for mut owner in owners {
            let phase = std::mem::discriminant(&owner);
            assert!(
                owner
                    .install_projection_with_resource_reservation(
                        ProjectedEnvironmentOwner::AwaitingAdoption {
                            identity: identity("owner-B"),
                            binding: "B".into()
                        },
                        newer,
                        false,
                    )
                    .is_err(),
                "R3"
            );
            assert_eq!(std::mem::discriminant(&owner), phase);
            let retained = match &owner {
                SessionEnvironmentOwner::Resident(o) => &o.environment,
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(o)) => {
                    &o.environment
                }
                SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                    owned: RetiringEnvironmentOwner::Bound(o),
                    ..
                }) => &o.environment,
                _ => panic!("R3 unexpected phase"),
            };
            assert!(Arc::ptr_eq(retained, &env), "R3 exact Arc retained");
        }
    }
}
