//! Repository credential realization and worker resource runtime survival.

use super::*;
use futures_util::FutureExt;

fn pinned_vault(
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> awaken_credential_materializer::PinnedCredentialMaterializer {
    awaken_credential_materializer::PinnedCredentialMaterializer::new(
        Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
            credentials,
            secrets,
            clock.clone(),
        )),
        clock,
    )
}

fn repository_credential_descriptor(url: &str) -> awaken_credential_contract::CredentialDescriptor {
    awaken_credential_contract::CredentialDescriptor::new(
        "git",
        awaken_credential_contract::CredentialMaterialDescriptor::structured(
            awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE,
            ["password", "username"],
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            crate::session_resource::repository_transport_credential_target(url)
                .expect("HTTPS Repository target"),
            crate::session_resource::repository_transport_credential_usage(),
        )],
    )
}

struct GatewayThenDirectRepositoryTransport(AtomicUsize);

#[async_trait::async_trait]
impl crate::RepositoryBindingVerifier<crate::ResourceOperationFence>
    for GatewayThenDirectRepositoryTransport
{
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _authority: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(
                awaken_resource_contract::RepositoryTransport::GatewayMediated {
                    remote_url: "https://gateway.internal/git/repo-platform".into(),
                    capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                        "repository-capability-initial",
                    )?,
                    expires_at_unix_ms: None,
                },
            )
        } else {
            Ok(awaken_resource_contract::RepositoryTransport::Direct)
        }
    }
}

#[derive(Default)]
struct RecordingRepositoryRealizer(
    Mutex<Vec<String>>,
    Mutex<
        Vec<(
            awaken_provisioning_contract::RepositoryRealizationPlan,
            awaken_provisioning_contract::RepositoryPublicationExpectation,
            Option<String>,
        )>,
    >,
);

#[async_trait::async_trait]
impl awaken_provisioning_contract::RepositoryRealizer for RecordingRepositoryRealizer {
    async fn realize_repository(
        &self,
        _plan: &awaken_provisioning_contract::RepositoryRealizationPlan,
        credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,

        _intent: awaken_provisioning_contract::RepositoryRealizationIntent,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        self.0.lock().unwrap().push(
            credential
                .map(|credential| credential.expose_password().to_owned())
                .unwrap_or_default(),
        );
        Ok(())
    }

    async fn publish_repository(
        &self,
        plan: &awaken_provisioning_contract::RepositoryRealizationPlan,
        expectation: &awaken_provisioning_contract::RepositoryPublicationExpectation,
        credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
    ) -> Result<
        awaken_provisioning_contract::RepositoryPublicationReceipt,
        awaken_provisioning_contract::RepositoryPublicationError,
    > {
        self.1.lock().unwrap().push((
            plan.clone(),
            expectation.clone(),
            credential.map(|value| value.expose_password().to_owned()),
        ));
        Ok(awaken_provisioning_contract::RepositoryPublicationReceipt::new(plan, expectation))
    }
}

/// Worker Repository effect cause graph: C1 dispatch Session Runtime installed
/// from the Managed adapter -> C2 exact secret-free pin stages -> C3 the Host
/// owns the canonical materializer at the Git edge -> E1 one operation-scoped
/// HTTP Basic value reaches the realizer. Missing C3 preserves E2 (the pin may
/// stage) but terminates before Git without another credential path.
///
/// | Rule | C1 | C2 | C3 | Stage result | Git effect |
/// |---|---|---|---|---|---|
/// | D1 | T | T | T | secret-free activation | exact transient material |
/// | D2 | T | T | F | same secret-free activation | reject, zero realizer calls |
/// P2/C12 extends these original rows with true installed Session poison after staging:
/// D3 configured Workload reads the current Workspace -> typed Poisoned denial, no effect;
/// D4 absent materializer -> original Internal before any Workspace read;
/// D5 anonymous clone -> no installed Session read and one anonymous effect despite poison.
/// No fake Session view or provider receipt is substituted for that mutex.
#[tokio::test]
async fn worker_dispatch_resource_runtime_survives_assembly_and_fails_closed() {
    for (rule, install_credentials) in [("D1", true), ("D2", false)] {
        let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
        let workspace = host.local_workspace().to_owned();
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let source = awaken_credential_vault::repo::enter_credential_described(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: workspace.clone(),
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(http_basic_material("git", "dispatch-repository-secret")),
                oauth_command: None,
            },
            repository_credential_descriptor("https://github.com/awaken/example.git"),
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("author dispatch Repository credential");
        let materializer = pinned_vault(credentials, secrets, awaken_time::system_clock());
        let host = (crate::host::with_test_resource_validation(if install_credentials {
            host.with_credential_materializer(materializer.clone())
        } else {
            host
        }))
        .build()
        .expect("validated Host dependencies");
        let managed = managed_with_resource_source(host.clone());
        let _managed = managed;

        let thread = format!("worker-dispatch-repository-{rule}");
        let manifest = crate::session_resource::SessionResourceManifest::new(
            host.run_assembly.local_workspace(),
            effective_repository(
                "repo-1",
                "https://github.com/awaken/example.git",
                "/workspace/repo",
                Some(source.id.0),
            ),
        );
        let result = host
            .realization_executor
            .install_dispatched_resources(&thread, &manifest, None)
            .await;
        result.unwrap_or_else(|error| panic!("{rule}: {error}"));
        assert_eq!(
            host.realization_executor
                .staging()
                .thread_repository_activations(&thread)
                .len(),
            1,
            "{rule}"
        );
        assert!(
            host.realization_executor
                .staging()
                .thread_repository_activations(&thread)[0]
                .credential_pin
                .is_some(),
            "{rule}: staging retains only the exact pin"
        );
        let realizer = RecordingRepositoryRealizer::default();
        let effect = host
            .realization_executor
            .staging()
            .realize_thread_repositories(&thread, host.run_assembly.local_workspace(), &realizer)
            .await;
        if install_credentials {
            effect.unwrap_or_else(|error| panic!("{rule}: {error}"));
            assert_eq!(
                *realizer.0.lock().unwrap(),
                vec!["dispatch-repository-secret".to_owned()],
                "{rule}"
            );
        } else {
            let error = effect
                .expect_err("D2 must reject at the Git edge")
                .to_string();
            assert!(
                error.contains("configured credential materializer"),
                "{rule}: {error}"
            );
            assert!(
                realizer.0.lock().unwrap().is_empty(),
                "{rule}: no Git realizer call after materialization failure"
            );
        }
        let staged = host
            .realization_executor
            .staging()
            .thread_resources_snapshot(&thread)
            .expect("installed Sessions");
        let prior_calls = realizer.0.lock().unwrap().len();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                host.run_assembly
                    .installed_sessions
                    .modify(&thread, |_| {
                        panic!("P2/C12 actual installed Session poison")
                    })
                    .expect("installed Sessions");
            }))
            .is_err()
        );
        let effect = std::panic::AssertUnwindSafe(
            host.realization_executor
                .staging()
                .realize_repository_activation(
                    &thread,
                    host.run_assembly.local_workspace(),
                    &staged.repositories[0],
                    &staged.binding_checks,
                    &realizer,
                    awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
                ),
        )
        .catch_unwind()
        .await;
        if install_credentials {
            let denied = effect
                .expect("D3 a poisoned installed Session denies without unwinding")
                .expect_err(
                    "D3 configured materializer still reads the poisoned installed Session",
                );
            assert!(
                matches!(
                    denied,
                    crate::realization_error::RealizationError::Poisoned(
                        crate::realization_error::RealizationLock::InstalledSessions
                    )
                ),
                "D3: {denied}"
            );
        } else {
            let error = effect
                .expect("D4 missing dependency precedes poison")
                .expect_err("D4 no materializer cannot reach Git");
            assert_eq!(
                error.disposition(),
                crate::realization_error::Disposition::Terminal,
                "D4"
            );
            assert_eq!(error.code(), "session_realization_invariant_violated", "D4");
            assert_eq!(
                error.to_string(),
                "repository credential requires a configured credential materializer",
                "D4"
            );
        }
        assert_eq!(
            realizer.0.lock().unwrap().len(),
            prior_calls,
            "D3/D4 no effect"
        );
        let mut anonymous = staged.repositories[0].clone();
        anonymous.credential_pin = None;
        host.realization_executor
            .staging()
            .realize_repository_activation(
                &thread,
                host.run_assembly.local_workspace(),
                &anonymous,
                &[],
                &realizer,
                awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
            )
            .await
            .expect("D5 anonymous clone never reads poisoned installed Session");
        assert_eq!(
            realizer.0.lock().unwrap().len(),
            prior_calls + 1,
            "D5 one effect"
        );
        assert_eq!(
            realizer.0.lock().unwrap().last().unwrap(),
            "",
            "D5 no material"
        );
    }
}

/// Platform-held Repository injection decision table:
///
/// | Rule | dispatch claim | verifier transport | local materializer | Effect |
/// |---|---|---|---|---|
/// | P1 | exact | Gateway mediated | absent | preserve source URL, stage Gateway transport + secret-free pin |
/// | P2 | exact | Direct | any | reject; never fall back to Worker plaintext |
/// | P3 | absent Coordinator staging | Direct | absent | retain exact check, defer Git activation without a claim |
/// | P4 | exact, delayed use | Gateway mediated again | absent | refresh capability at Git operation edge |
/// | P5 | exact, tampered source or target before use | Gateway mediated | absent | reject before verifier/I/O |
/// | P6 | exact, delayed use | changes to Direct | any | reject; zero direct fallback |
/// P2/C13 uses this same verifier/realizer fixture for protected upstream
/// publication. Causes T1 exact command+lease/Workspace/repo/config, T2 fresh
/// mediated capability, T3 verifier Indeterminate/Direct, T4 pin malformed,
/// T5 actual installed Session poisoned while the original verifier is gated. Effects:
/// T1+T2 -> one publish with refreshed capability + verified receipt;
/// T3/T4 -> original typed refusal + zero new publish; T5 -> original second
/// delayed Workspace read denies (typed Poisoned) after joined verifier, zero new publish.
/// C12 also preserves upstream missing-verifier-before-poison, while ordinary
/// Platform clone reads poison (a typed denial) before its missing-verifier check. These are
/// operation-adapter proofs; the original physical terminal tests own lifecycle
/// authorization/adoption and actual remote Git, without a fake receipt here.
#[tokio::test]
async fn platform_repository_credentials_are_gateway_mediated_without_fallback() {
    fn platform_resources() -> crate::session_resource::ResolvedSessionResources {
        let resources = effective_repository(
            "repo-platform",
            "https://github.com/awaken/example.git",
            "/workspace/repo",
            Some("credential-platform".into()),
        );
        let binding_id = resources.inputs()[0].binding_id.clone();
        let holder = awaken_agent_contract::credential::PlaintextHolder::new(
            awaken_agent_contract::credential::PlaintextBoundary::Platform,
            "awaken.platform.egress-gateway",
        );
        resources
            .update_input(&binding_id, |input| {
                let crate::session_resource::ResolvedInputSource::Repository {
                    credential: Some(credential),
                    ..
                } = &mut input.source
                else {
                    unreachable!()
                };
                credential.selected_plaintext_holder = holder.clone();
                credential.access.policy =
                    awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                        holder,
                        awaken_agent_contract::credential::ModelExposurePolicy::Forbidden,
                    );
            })
            .unwrap()
    }

    let claim = awaken_dispatch_contract::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run-platform-repository".into()),
        owner: "worker-platform".into(),
        epoch: 7,
    };
    let sequenced = Arc::new(SequencedRepositoryTransport::default());
    let host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_repository_binding_verifier(sequenced.clone()),
        ))
    .build()
    .expect("validated Host dependencies");

    let runtime = host.clone();
    let manifest = crate::session_resource::SessionResourceManifest::new(
        host.run_assembly.local_workspace(),
        platform_resources(),
    );
    host.realization_executor
        .install_dispatched_resources("platform-mediated", &manifest, Some(&claim))
        .await
        .expect("P1 Gateway mediation needs no Worker materializer");
    let activations = host
        .realization_executor
        .staging()
        .thread_repository_activations("platform-mediated");
    let activation = &activations[0];
    assert_eq!(
        activation.plan.source_remote_url, "https://github.com/awaken/example.git",
        "P1 frozen source remains canonical"
    );
    assert_eq!(
        activation.plan.transport_url, "https://gateway.internal/git/repo-platform",
        "P1 Gateway endpoint is effect-only transport"
    );
    assert_eq!(
        activation
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.as_str()),
        Some("credential-platform"),
        "P1 retains the secret-free pin, never the staged capability"
    );
    let resources = host
        .realization_executor
        .staging()
        .thread_resources_snapshot("platform-mediated")
        .expect("installed Sessions");
    let mut wrong_target = activation.clone();
    wrong_target
        .credential_pin
        .as_mut()
        .expect("P5 protected activation")
        .access
        .target = Some(
        crate::session_resource::repository_transport_credential_target(
            "https://gitlab.example.test/awaken/example.git",
        )
        .expect("P5 alternate HTTPS target"),
    );
    let invalid_realizer = RecordingRepositoryRealizer::default();
    let invalid = host
        .realization_executor
        .staging()
        .realize_repository_activation(
            "platform-mediated",
            host.run_assembly.local_workspace(),
            &wrong_target,
            &resources.binding_checks,
            &invalid_realizer,
            awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
        )
        .await
        .expect_err("P5 target mismatch rejects before Gateway refresh");
    assert!(invalid.to_string().contains("another HTTPS origin"), "P5");
    assert_eq!(sequenced.calls.load(Ordering::SeqCst), 1, "P5");
    assert!(invalid_realizer.0.lock().unwrap().is_empty(), "P5");

    let mut wrong_source = activation.clone();
    wrong_source
        .credential_pin
        .as_mut()
        .expect("P5 protected activation")
        .access
        .credential
        .id = "another-source".into();
    let invalid_source = host
        .realization_executor
        .staging()
        .realize_repository_activation(
            "platform-mediated",
            host.run_assembly.local_workspace(),
            &wrong_source,
            &resources.binding_checks,
            &invalid_realizer,
            awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
        )
        .await
        .expect_err("P5 source mismatch rejects against the authored binding");
    assert!(
        invalid_source
            .to_string()
            .contains("selects another source"),
        "P5"
    );
    assert_eq!(sequenced.calls.load(Ordering::SeqCst), 1, "P5");
    assert!(invalid_realizer.0.lock().unwrap().is_empty(), "P5");

    let realizer = RecordingRepositoryRealizer::default();
    host.realization_executor
        .staging()
        .realize_thread_repositories(
            "platform-mediated",
            host.run_assembly.local_workspace(),
            &realizer,
        )
        .await
        .expect("P4 refreshes at the Git operation edge");
    assert_eq!(sequenced.calls.load(Ordering::SeqCst), 2, "P4");
    assert_eq!(
        *realizer.0.lock().unwrap(),
        vec!["repository-capability-2".to_owned()],
        "P4 never reuses the capability staged before package or Sandbox preparation"
    );

    let expected_run = Some(crate::ResourceOperationFence::Run(claim.clone()));
    assert_eq!(
        *sequenced.seen.lock().unwrap(),
        vec![
            (
                host.run_assembly.local_workspace().to_owned(),
                "repo-platform".into(),
                awaken_resource_contract::ConfigVersion::INITIAL,
                expected_run.clone()
            ),
            (
                host.run_assembly.local_workspace().to_owned(),
                "repo-platform".into(),
                awaken_resource_contract::ConfigVersion::INITIAL,
                expected_run.clone()
            ),
        ],
        "P1/P4 exact original claim and resource domain, not a reminted fence",
    );
    let original_verifier: Arc<
        dyn crate::RepositoryBindingVerifier<crate::ResourceOperationFence>,
    > = sequenced.clone();
    assert!(
        Arc::ptr_eq(
            host.realization_executor
                .staging()
                .repository_binding_verifier()
                .unwrap(),
            &original_verifier
        ),
        "P2 same injected verifier Arc"
    );
    runtime
        .realization_executor
        .validate_thread_resource_bindings("platform-mediated")
        .await
        .expect("P2 installed binding uses same Stage verifier");
    assert_eq!(
        sequenced.calls.load(Ordering::SeqCst),
        3,
        "P2 one original binding verification"
    );
    assert_eq!(
        sequenced.seen.lock().unwrap().last().unwrap().3,
        expected_run,
        "P2 binding retains original claim"
    );
    assert_eq!(
        realizer.0.lock().unwrap().len(),
        1,
        "P2 validation is not another Git effect"
    );

    let downgrade = Arc::new(GatewayThenDirectRepositoryTransport(AtomicUsize::new(0)));
    let downgrade_host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_repository_binding_verifier(downgrade.clone()),
        ))
    .build()
    .expect("validated Host dependencies");

    let _downgrade_runtime = downgrade_host.clone();
    let downgrade_manifest = crate::session_resource::SessionResourceManifest::new(
        downgrade_host.run_assembly.local_workspace(),
        platform_resources(),
    );
    downgrade_host
        .realization_executor
        .install_dispatched_resources("platform-downgrade", &downgrade_manifest, Some(&claim))
        .await
        .expect("P6 initial Gateway route stages");
    let downgrade_realizer = RecordingRepositoryRealizer::default();
    let downgrade_error = downgrade_host
        .realization_executor
        .staging()
        .realize_thread_repositories(
            "platform-downgrade",
            downgrade_host.run_assembly.local_workspace(),
            &downgrade_realizer,
        )
        .await
        .expect_err("P6 refreshed Direct transport must not fall back");
    assert!(
        downgrade_error
            .to_string()
            .contains("cannot fall back to direct credentials"),
        "P6"
    );
    assert_eq!(downgrade.0.load(Ordering::SeqCst), 2, "P6");
    assert!(downgrade_realizer.0.lock().unwrap().is_empty(), "P6");

    let direct_host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default().with_repository_binding_verifier(
                Arc::new(FixedRepositoryTransport(
                    awaken_resource_contract::RepositoryTransport::Direct,
                )),
            ),
        ))
    .build()
    .expect("validated Host dependencies");
    let _direct_runtime = direct_host.clone();
    let direct_manifest = crate::session_resource::SessionResourceManifest::new(
        direct_host.run_assembly.local_workspace(),
        platform_resources(),
    );
    let denied = direct_host
        .realization_executor
        .install_dispatched_resources("platform-direct-denied", &direct_manifest, Some(&claim))
        .await
        .expect_err("P2 direct transport cannot receive Platform-held material");
    assert!(denied.message.contains("requires Gateway mediation"), "P2");
    assert!(
        direct_host
            .realization_executor
            .staging()
            .thread_repository_activations("platform-direct-denied")
            .is_empty(),
        "P2"
    );

    direct_host
        .realization_executor
        .install_dispatched_resources("platform-coordinator-stage", &direct_manifest, None)
        .await
        .expect("P3 unclaimed Coordinator stages only the exact Repository binding");
    assert!(
        direct_host
            .realization_executor
            .staging()
            .thread_repository_activations("platform-coordinator-stage")
            .is_empty(),
        "P3 no Direct Git activation or anonymous fallback before a Run claim"
    );
    let staged = direct_host
        .realization_executor
        .staging()
        .thread_resources_snapshot("platform-coordinator-stage")
        .expect("installed Sessions");
    assert_eq!(staged.binding_checks.len(), 1, "P3 exact binding retained");
    assert!(
        matches!(
            &staged.binding_checks[0],
            crate::staged_resources::ResourceBindingCheck::Repository {
                repository_id,
                credential_binding: Some(_),
                authority: None,
                ..
            } if repository_id == "repo-platform"
        ),
        "P3 immutable Repository identity and credential binding survive staging"
    );
    Box::pin(async {
        let remote_verifier = Arc::new(SequencedRepositoryTransport::default());
        let remote = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await
            .with_authority(crate::UpstreamAuthority::default())
            .with_session_execution(crate::SessionExecutionDependencies::default()
                .with_repository_binding_verifier(remote_verifier.clone())))
            .build().expect("C13 actual upstream mode");
        let session = "terminal-protected-platform";
        remote.run_assembly.installed_sessions.replace_frozen_with(session, |next| {
            next.workspace = Some("terminal-original-workspace".into());
        }).expect("installed Sessions");
        let expectation = awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/work".into(), commit: "a".repeat(40), expected_prior_commit: None,
        };
        let command = awaken_session_contract::SessionRepositoryPublicationCommand {
            session_id: session.into(), effect_id: "terminal-protected-platform-effect".into(),
            intent: awaken_session_contract::SessionRepositoryPublicationIntent {
                input: platform_resources().inputs()[0].clone(), expectation: expectation.clone(),
            },
        };
        let (lease, _) = terminal_recovery_effect(session, "terminal-protected-owner");
        let staged = remote.realization_executor.staging().stage_terminal_repository_publication_input(
            "terminal-original-workspace", &command, &lease,
        ).await.expect("C13 freeze exact protected publication plan");
        let repository = &staged.repositories[0];
        let fence = (command.clone(), lease.clone());
        let expected_fence = Some(crate::ResourceOperationFence::RepositoryPublication {
            command: Box::new(command.clone()), lease: lease.clone(),
        });
        let published = Arc::new(RecordingRepositoryRealizer::default());
        let receipt = remote.realization_executor.staging().publish_repository_activation(
            session, remote.run_assembly.local_workspace(), repository, &staged.binding_checks,
            published.as_ref(), &expectation, Some(&fence),
        ).await.expect("C13 fresh capability at protected Git publication");
        receipt.verify(&repository.plan, &expectation).expect("C13 exact verified receipt");
        assert_eq!(remote_verifier.calls.load(Ordering::SeqCst), 2, "C13 stage + operation refresh");
        assert_eq!(
            *remote_verifier.seen.lock().unwrap(),
            vec![
                ("terminal-original-workspace".into(), "repo-platform".into(), awaken_resource_contract::ConfigVersion::INITIAL, expected_fence.clone()),
                ("terminal-original-workspace".into(), "repo-platform".into(), awaken_resource_contract::ConfigVersion::INITIAL, expected_fence),
            ], "C13 exact publication command + lease replaces ordinary claim",
        );
        assert_eq!(*published.1.lock().unwrap(), vec![(repository.plan.clone(), expectation.clone(), Some("repository-capability-2".into()))], "C13 one publish, refreshed capability, exact plan/expectation");
        assert!(published.0.lock().unwrap().is_empty(), "C13 no clone path");
        assert_eq!(repository.credential_pin.as_ref().unwrap().access.credential.id, "credential-platform", "C13 retained pin is still secret-free");

        let mut renewed = lease.clone();
        renewed.expires_at_unix_ms += 1;
        let renewed_fence = (command.clone(), renewed.clone());
        *remote_verifier.next_error.lock().unwrap() = Some(awaken_resource_contract::RepositoryBindingVerifierError::Indeterminate { detail: "injected unknown authority".into() });
        let refused = remote.realization_executor.staging().publish_repository_activation(
            session, remote.run_assembly.local_workspace(), repository, &staged.binding_checks,
            published.as_ref(), &expectation, Some(&renewed_fence),
        ).await.expect_err("C13 unknown authority must not grant publication");
        match refused {
            crate::staged_resources::RepositoryPublicationActivationError::Failed(error) => {
                assert_eq!(error.disposition(), crate::realization_error::Disposition::Retry, "C13 unknown typed kind");
                assert_eq!(error.code(), "unavailable", "C13 original code");
                assert_eq!(error.to_string(), "Repository binding decision is indeterminate: injected unknown authority", "C13 original reason");
            }
            _ => panic!("C13 verifier failure is not a physical publication rejection"),
        }
        assert_eq!(remote_verifier.seen.lock().unwrap().last().unwrap().3, Some(crate::ResourceOperationFence::RepositoryPublication { command: Box::new(command.clone()), lease: renewed }), "C13 current lease forwarded exactly");
        assert_eq!(published.1.lock().unwrap().len(), 1, "C13 unknown zero new effects");

        let mut malformed = repository.clone();
        malformed.credential_pin.as_mut().unwrap().access.credential.id = "foreign-source".into();
        let refused = remote.realization_executor.staging().publish_repository_activation(
            session, remote.run_assembly.local_workspace(), &malformed, &staged.binding_checks,
            published.as_ref(), &expectation, Some(&fence),
        ).await.expect_err("C13 pin rejects before verifier");
        assert!(matches!(refused, crate::staged_resources::RepositoryPublicationActivationError::Failed(error)
            if error.disposition() == crate::realization_error::Disposition::Terminal && error.to_string() == "invalid Repository credential execution pin: Repository credential pin selects another source"));
        assert_eq!(remote_verifier.calls.load(Ordering::SeqCst), 3, "C13 malformed no refresh");
        assert_eq!(published.1.lock().unwrap().len(), 1, "C13 malformed zero effect");
        *remote_verifier.next_transport.lock().unwrap() = Some(awaken_resource_contract::RepositoryTransport::Direct);
        let refused = remote.realization_executor.staging().publish_repository_activation(
            session, remote.run_assembly.local_workspace(), repository, &staged.binding_checks,
            published.as_ref(), &expectation, Some(&fence),
        ).await.expect_err("C13 upstream direct downgrade rejects");
        assert!(matches!(refused, crate::staged_resources::RepositoryPublicationActivationError::Failed(error)
            if error.disposition() == crate::realization_error::Disposition::Terminal && error.to_string() == "Gateway-mediated Repository cannot fall back to direct credentials"));
        assert_eq!(remote_verifier.calls.load(Ordering::SeqCst), 4, "C13 one downgrade decision");
        assert_eq!(published.1.lock().unwrap().len(), 1, "C13 downgrade zero effect");

        // C12: upstream publication checks a missing verifier BEFORE looking up
        // Workspace; ordinary Platform clone performs that installed Session read FIRST.
        let missing = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await
            .with_authority(crate::UpstreamAuthority::default()))
            .build().expect("C12 upstream has no verifier");
        missing.run_assembly.installed_sessions.replace_frozen_with(session, |next| next.workspace = Some("terminal-original-workspace".into())).expect("installed Sessions");
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            missing.run_assembly.installed_sessions.modify(session, |_| panic!("C12 actual installed Session poison")).expect("installed Sessions");
        })).is_err());
        let refused = std::panic::AssertUnwindSafe(missing.realization_executor.staging().publish_repository_activation(
            session, missing.local_workspace(), repository, &staged.binding_checks,
            published.as_ref(), &expectation, Some(&fence),
        )).catch_unwind().await.expect("C12 missing verifier precedes poison").expect_err("C12 refused");
        assert!(matches!(refused, crate::staged_resources::RepositoryPublicationActivationError::Failed(error)
            if error.disposition() == crate::realization_error::Disposition::Terminal && error.to_string() == "terminal Repository publication has no binding verifier"));
        assert!(matches!(
            std::panic::AssertUnwindSafe(missing.realization_executor.staging().realize_repository_activation(
                session, missing.local_workspace(), repository, &staged.binding_checks,
                published.as_ref(), awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
            )).catch_unwind().await.expect("C12 a poisoned installed Session denies without unwinding"),
            Err(crate::realization_error::RealizationError::Poisoned(_))
        ), "C12 ordinary Platform poison precedes absent verifier");
        assert_eq!(published.1.lock().unwrap().len(), 1, "C12 zero publication");
        assert!(published.0.lock().unwrap().is_empty(), "C12 zero clone");

        // C13/T5: join the actual asynchronous verifier entry, poison only this
        // real installed Session, release/join its callback, then observe the required SECOND
        // Workspace read fail. A captured/precomputed Workspace would publish.
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *remote_verifier.next_gate.lock().unwrap() = Some((entered.clone(), release.clone()));
        let operation = tokio::spawn({
            let remote = remote.clone();
            let repository = repository.clone();
            let checks = staged.binding_checks.clone();
            let published = published.clone();
            let expectation = expectation.clone();
            let fence = fence.clone();
            async move {
                std::panic::AssertUnwindSafe(remote.realization_executor.staging().publish_repository_activation(
                    session, remote.run_assembly.local_workspace(), &repository, &checks,
                    published.as_ref(), &expectation, Some(&fence),
                )).catch_unwind().await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.expect("C13 actual verifier entry");
        assert_eq!(remote_verifier.calls.load(Ordering::SeqCst), 5, "C13 first read reached verifier");
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            remote.run_assembly.installed_sessions.modify(session, |_| panic!("C13 poison during original verifier")).expect("installed Sessions");
        })).is_err());
        release.notify_one();
        let refused = operation.await.expect("C13 callback and operation joined");
        assert!(
            matches!(
                refused.expect("C13 a poisoned installed Session denies without unwinding"),
                Err(crate::staged_resources::RepositoryPublicationActivationError::Failed(
                    crate::realization_error::RealizationError::Poisoned(_)
                ))
            ),
            "C13 second current Workspace read observes true installed Session poison"
        );
        assert_eq!(remote_verifier.calls.load(Ordering::SeqCst), 5, "C13 no second verifier");
        assert_eq!(published.1.lock().unwrap().len(), 1, "C13 gated poison zero new publication");
        assert!(published.0.lock().unwrap().is_empty(), "C13 gated poison no clone");
    }).await;
}

/// Repository realization cause graph:
/// C1 binding/pin cardinality exact -> C2 source id/usage exact -> C3 holder
/// allowed and model exposure forbidden -> C4 Sandbox holder exact -> C5 Git
/// effect opens the pinned active revision in the exact Workspace -> E1 one
/// ephemeral operation material reaches the realizer. C1-C4 reject at staging;
/// C5 rejects at each effect edge. Anonymous input bypasses C2-C5; the first
/// failed cause terminates without another credential or holder selection.
///
/// | Rule | Credential | C1 | C2 | C3 | C4 | C5 | Result |
/// |---|---|---|---|---|---|---|---|
/// | H1 | absent | T | - | - | - | - | anonymous |
/// | H2 | present | T | T | T | T | T | secret-free stage, exact effect material |
/// | H3 | present | F | - | - | - | - | reject missing pin |
/// | H4 | present | T | F | - | - | - | reject source mismatch |
/// | H5 | present | T | T | F | - | - | reject usage mismatch |
/// | H6 | absent | F | - | - | - | - | reject extra pin |
/// | H7 | present | T | T | F | - | - | reject unauthorized holder |
/// | H8 | present | T | T | F | - | - | reject virtual exposure |
/// | H9 | present | T | T | T | F | - | reject unsupported holder |
/// | H10 | present | T | T | T | T | F | stage pin, reject stale revision at effect |
/// | H11 | present | T | T | T | T | F | stage pin, reject inactive source at effect |
/// | H12 | present | T | T | T | T | F | stage pin, reject cross-Workspace at effect |
/// | H13 | present | T | T | T | T | scalar | stage pin, reject material kind at effect |
/// | H14 | present | T | T | T | T | active then disabled | first effect only; retry rejects |
#[tokio::test]
async fn repository_credential_realization_follows_the_decision_table() {
    use awaken_session_configuration_contract::SessionInit;

    #[derive(Clone, Copy)]
    enum Case {
        Anonymous,
        Exact,
        MissingPin,
        PinWithoutBinding,
        WrongSource,
        WrongUsage,
        HolderNotAllowed,
        VirtualExposure,
        UnsupportedHolder,
        StaleRevision,
        InactiveSource,
        CrossWorkspace,
        WrongMaterial,
    }
    struct Rule {
        id: &'static str,
        case: Case,
        expected_error: Option<&'static str>,
    }
    let rules = [
        Rule {
            id: "H1",
            case: Case::Anonymous,
            expected_error: None,
        },
        Rule {
            id: "H2",
            case: Case::Exact,
            expected_error: None,
        },
        Rule {
            id: "H3",
            case: Case::MissingPin,
            expected_error: Some("has no exact Session pin"),
        },
        Rule {
            id: "H4",
            case: Case::WrongSource,
            expected_error: Some("selects another source"),
        },
        Rule {
            id: "H5",
            case: Case::WrongUsage,
            expected_error: Some("incompatible transport usage"),
        },
        Rule {
            id: "H6",
            case: Case::PinWithoutBinding,
            expected_error: Some("credential pin without a binding"),
        },
        Rule {
            id: "H7",
            case: Case::HolderNotAllowed,
            expected_error: Some("holder is not authorized"),
        },
        Rule {
            id: "H8",
            case: Case::VirtualExposure,
            expected_error: Some("must remain model-invisible"),
        },
        Rule {
            id: "H9",
            case: Case::UnsupportedHolder,
            expected_error: Some("Runtime Host or Worker"),
        },
        Rule {
            id: "H10",
            case: Case::StaleRevision,
            expected_error: Some("credential material revision mismatch"),
        },
        Rule {
            id: "H11",
            case: Case::InactiveSource,
            expected_error: Some("credential material unavailable"),
        },
        Rule {
            id: "H12",
            case: Case::CrossWorkspace,
            expected_error: Some("credential material recipient mismatch"),
        },
        Rule {
            id: "H13",
            case: Case::WrongMaterial,
            expected_error: Some("credential material kind is unsupported"),
        },
    ];

    for rule in rules {
        let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
        let workspace = host.local_workspace().to_owned();
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let mut source = awaken_credential_vault::repo::enter_credential_described(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: if matches!(rule.case, Case::CrossWorkspace) {
                    "another-workspace".into()
                } else {
                    workspace.clone()
                },
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(http_basic_material("git", "repository-decision-secret")),
                oauth_command: None,
            },
            repository_credential_descriptor("https://github.com/awaken/example.git"),
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("author exact Repository credential");
        if matches!(rule.case, Case::WrongMaterial) {
            let material_ref = source
                .material_ref
                .as_ref()
                .expect("described Repository material reference");
            awaken_credential_vault::SecretStore::put(
                secrets.as_ref(),
                material_ref,
                awaken_agent_contract::RedactedString::new("corrupt-scalar-token"),
            )
            .await
            .expect("simulate corrupted Repository material at rest");
        }
        if matches!(rule.case, Case::InactiveSource) {
            source.status = awaken_credential_vault::CredentialStatus::Disabled;
            awaken_credential_vault::repo::CredentialRepo::put(
                credentials.as_ref(),
                source.clone(),
            )
            .await
            .expect("disable exact Repository credential");
        }
        let materializer = pinned_vault(credentials.clone(), secrets, awaken_time::system_clock());
        let host = ((crate::host::with_test_resource_validation(
            host.with_credential_materializer(materializer.clone()),
        ))
        .with_credential_materializer(materializer))
        .build()
        .expect("validated Host dependencies");
        let managed = managed_with_resource_source(host.clone());

        let binding = (!matches!(rule.case, Case::Anonymous)).then(|| source.id.0.clone());
        let resources = effective_repository(
            "repo-1",
            "https://github.com/awaken/example.git",
            "/workspace/repo",
            binding,
        );
        let binding_id = resources.inputs()[0].binding_id.clone();
        let resources = resources
            .update_input(&binding_id, |input| {
                let crate::session_resource::ResolvedInputSource::Repository {
                    config,
                    credential,
                    ..
                } = &mut input.source
                else {
                    unreachable!()
                };
                match rule.case {
                    Case::Anonymous | Case::Exact => {}
                    Case::MissingPin => *credential = None,
                    Case::PinWithoutBinding => config.credential_binding = None,
                    Case::WrongSource => {
                        credential.as_mut().unwrap().access.credential.id = "another-source".into();
                    }
                    Case::WrongUsage => {
                        credential.as_mut().unwrap().access.usage =
                            awaken_agent_contract::credential::CredentialUsage::QueryParameter {
                                name: "token".into(),
                            };
                    }
                    Case::HolderNotAllowed => {
                        let workload = awaken_agent_contract::credential::PlaintextHolder::new(
                            awaken_agent_contract::credential::PlaintextBoundary::Workload,
                            awaken_agent_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                        );
                        credential.as_mut().unwrap().access.policy =
                            awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                                workload,
                                awaken_agent_contract::credential::ModelExposurePolicy::Forbidden,
                            );
                    }
                    Case::VirtualExposure => {
                        let credential = credential.as_mut().unwrap();
                        credential.access.policy =
                            awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                                credential.selected_plaintext_holder.clone(),
                                awaken_agent_contract::credential::ModelExposurePolicy::VirtualOnly,
                            );
                    }
                    Case::UnsupportedHolder => {
                        let worker = awaken_agent_contract::credential::PlaintextHolder::new(
                            awaken_agent_contract::credential::PlaintextBoundary::Worker,
                            awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                        );
                        let credential = credential.as_mut().unwrap();
                        credential.selected_plaintext_holder = worker.clone();
                        credential.access.policy =
                            awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                                worker,
                                awaken_agent_contract::credential::ModelExposurePolicy::Forbidden,
                            );
                    }
                    Case::StaleRevision => {
                        credential.as_mut().unwrap().access.credential.revision = 2;
                    }
                    Case::InactiveSource | Case::CrossWorkspace | Case::WrongMaterial => {}
                }
            })
            .unwrap();
        let thread = format!("repository-decision-{}", rule.id);
        let result = managed
            .run_assembly
            .install_test_session_init(
                &thread,
                SessionInit {
                    workspace_id: host.run_assembly.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    tools: None,
                    resource_revision: 0,
                    resources,
                    model: None,
                    runtime: None,
                    environment: session_environment(
                        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                        serde_json::json!({}),
                    ),
                },
            )
            .await;
        let rejects_at_effect = matches!(
            rule.case,
            Case::StaleRevision | Case::InactiveSource | Case::CrossWorkspace | Case::WrongMaterial
        );
        if rule.expected_error.is_none() || rejects_at_effect {
            result.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
            let staged = host
                .realization_executor
                .staging()
                .thread_repository_activations(&thread);
            assert_eq!(staged.len(), 1, "{}", rule.id);
            assert_eq!(
                staged[0].credential_pin.is_some(),
                !matches!(rule.case, Case::Anonymous),
                "{}",
                rule.id
            );
            let realizer = RecordingRepositoryRealizer::default();
            let effect = host
                .realization_executor
                .staging()
                .realize_thread_repositories(
                    &thread,
                    host.run_assembly.local_workspace(),
                    &realizer,
                )
                .await;
            match rule.expected_error {
                None => {
                    effect.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
                    assert_eq!(realizer.0.lock().unwrap().len(), 1, "{}", rule.id);
                    if matches!(rule.case, Case::Exact) {
                        source.status = awaken_credential_vault::CredentialStatus::Disabled;
                        awaken_credential_vault::repo::CredentialRepo::put(
                            credentials.as_ref(),
                            source.clone(),
                        )
                        .await
                        .expect("H14 disable the exact source between Git effects");
                        let retry = host
                            .realization_executor
                            .staging()
                            .realize_thread_repositories(
                                &thread,
                                host.run_assembly.local_workspace(),
                                &realizer,
                            )
                            .await
                            .expect_err("H14 second Git effect revalidates liveness");
                        assert!(
                            retry
                                .to_string()
                                .contains("credential material unavailable"),
                            "H14"
                        );
                        assert_eq!(realizer.0.lock().unwrap().len(), 1, "H14");
                    }
                }
                Some(fragment) => {
                    let error = effect.expect_err("effect row must reject").to_string();
                    assert!(error.contains(fragment), "{}: {error}", rule.id);
                    assert!(realizer.0.lock().unwrap().is_empty(), "{}", rule.id);
                }
            }
        } else if let Some(fragment) = rule.expected_error {
            let error = result.expect_err("decision row must reject").to_string();
            assert!(error.contains(fragment), "{}: {error}", rule.id);
        }
    }
}

/// Applying a repository manifest with a new credential reference re-keys the
/// Resource realization only; it still creates no MCP projection.
#[tokio::test]
async fn rotating_a_github_repository_credential_re_keys_only_the_clone() {
    use awaken_session_configuration_contract::SessionInit;
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let host = ((crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .with_credentials(credentials.clone(), secrets.clone()))
    .build()
    .expect("validated Host dependencies");

    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.run_assembly.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_old")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let managed = managed_with_resource_source(host.clone());
    let initial = effective_repository(
        "repo-1",
        "https://github.com/awaken/example.git",
        "/workspace/repo",
        Some(credential.id.0.clone()),
    );
    managed
        .install_complete_test_session(
            "t-rot",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: initial.clone(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();
    managed
        .realization_executor
        .apply_session_inputs(
            "t-rot",
            &resource_transition(
                host.run_assembly.local_workspace(),
                0,
                Default::default(),
                0,
                initial.clone(),
            ),
        )
        .await
        .expect("compile the exact initial Repository transition");

    let clone_credential_id = |h: &ExecutorComponents| {
        h.realization_executor
            .staging()
            .thread_repository_activations("t-rot")[0]
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.clone())
    };
    assert_eq!(
        clone_credential_id(&host).as_deref(),
        Some(credential.id.0.as_str())
    );

    let next_credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.run_assembly.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_new")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let next = effective_repository(
        "repo-1",
        "https://github.com/awaken/example.git",
        "/workspace/repo",
        Some(next_credential.id.0.clone()),
    );
    // C1 the initial desired generation is staged but no Environment exists;
    // C2 the aggregate rotates only its credential pin. C1+C2 therefore carries
    // the exact initial->next transition without treating staging as completion.
    let before_manifest = crate::session_resource::SessionResourceManifest::at_revision(
        host.run_assembly.local_workspace(),
        0,
        initial,
    );

    // The Managed adapter stores the supplied credential in the Vault and publishes a
    // new Repository config before invoking this complete-manifest runtime port.
    managed
        .realization_executor
        .apply_session_inputs(
            "t-rot",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                next,
            ),
        )
        .await
        .unwrap();

    assert_eq!(
        clone_credential_id(&host).as_deref(),
        Some(next_credential.id.0.as_str()),
        "clone credential pin rotated without retaining either secret"
    );
    assert!(
        host.run_assembly
            .active_mcp_projections("t-rot")
            .expect("registry and projection are healthy")
            .is_empty()
    );
}
