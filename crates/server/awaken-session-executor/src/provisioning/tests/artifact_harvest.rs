//! Artifact harvest: live batch reads, terminal harvest and receipt recovery, and
//! capture failures that must not publish partial sets.

use super::*;
use awaken_session_execution_contract::RealizationEffects as _;

struct BatchArtifactContainerProvider {
    files: Vec<awaken_container_contract::EnvironmentFile>,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}

struct BatchArtifactContainer {
    handle: pc::SandboxHandle,
    files: Vec<awaken_container_contract::EnvironmentFile>,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}

fn batch_artifact_container_handle(spec: &pc::SandboxSpec) -> pc::SandboxHandle {
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    pc::SandboxHandle::container_v2(
        &spec.scope,
        awaken_sandbox_container::ContainerSandboxHandleV2 {
            previous: awaken_sandbox_container::ContainerSandboxHandleV1 {
                container_id: format!("container-{}", spec.scope),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: Vec::new(),
                runtime_handle: None,
                sandbox_control_incarnation: None,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint: fingerprint.clone(),
            realization_fingerprint: fingerprint,
            owned_paths: Vec::new(),
        },
    )
}

#[async_trait::async_trait]
impl awaken_container_contract::ContainerEnvironmentProvider for BatchArtifactContainerProvider {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    async fn create_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        _intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<
        std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment creation",
        ))
    }
    async fn observe_environment(
        &self,
        _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }
    async fn adopt_environment_for_effect(
        &self,
        _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment adoption",
        ))
    }
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            process_tree_supervision: false,
            isolation: pc::IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: true,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: true,
            package_provisioning: true,
            control_services: Default::default(),
        }
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn create_unattached_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(BatchArtifactContainer {
            handle: batch_artifact_container_handle(spec),
            files: self.files.clone(),
            scans: self.scans.clone(),
        }))
    }

    async fn adopt_unattached_environment(
        &self,
        _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "batch Artifact fixture never adopts an Environment",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_container_contract::ContainerEnvironment for BatchArtifactContainer {
    async fn owned_process_binding(
        &self,
    ) -> Result<Option<String>, awaken_provisioning_contract::SandboxError> {
        // This fixture retains no process containment.
        Ok(None)
    }

    async fn checkpoint_upload(
        &self,
        _access: awaken_container_contract::ContainerCheckpointAccess<'_>,
        _mutation: awaken_container_contract::ContainerCheckpointMutation<'_>,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxCheckpointUpload>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not implement a fenced checkpoint participant",
        ))
    }

    async fn remove_live_input_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "late mount removal is unsupported on this test container",
        ))
    }

    fn record_owned_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not support durable owned-path evidence",
        ))
    }

    async fn open_agent_channel(
        &self,
    ) -> Result<
        Box<dyn awaken_agent_channel::AgentChannel>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not expose a resident agent channel",
        ))
    }

    async fn spawn_agent_process(
        &self,
        _command: pc::Command,
    ) -> Result<awaken_container_contract::RuntimeAgentProcess, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "batch Artifact fixture never spawns an Agent",
        ))
    }

    async fn read_files(
        &self,
        root: &str,
    ) -> Result<Vec<awaken_container_contract::EnvironmentFile>, pc::SandboxError> {
        assert_eq!(root, "/outputs", "B1/C1 canonical output root");
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.files.clone())
    }
}

#[async_trait::async_trait]
impl SandboxControlServicePublisher for BatchArtifactContainer {
    async fn publish_sandbox_control_service(
        &self,
        _kind: SandboxControlServiceKind,
        _service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        // Batch Artifact tests advertise no control service; keep the fixture's
        // unavailable path exact and never synthesize a successful lease.
        Err(SandboxControlPublishError)
    }
}

#[async_trait::async_trait]
impl pc::Sandbox for BatchArtifactContainer {
    async fn prepare_disposal_for_effect(
        &self,
        _effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "test double does not prepare disposal",
        ))
    }

    async fn dispose_unattached(&self) -> Result<(), pc::SandboxError> {
        self.dispose().await
    }

    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
    }

    async fn spawn(
        &self,
        _command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "batch Artifact fixture never spawns a process",
        ))
    }

    async fn attach(
        &self,
        _requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "batch Artifact fixture never attaches a mount",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "Host batch capture must not use the Container Sandbox list port",
        ))
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "Host batch capture must not reread one Container Artifact",
        ))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &[]
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "batch Artifact fixture has no process",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(pc::SandboxStatus::Ready)
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))?;
        self.dispose().await
    }
}

impl BatchArtifactContainer {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

async fn install_batch_artifact_environment(
    host: &ExecutorComponents,
    thread: &str,
    workspace: &str,
) {
    host.capabilities
        .register_thread_workspace(thread, workspace)
        .expect("installed Sessions");
    let spec = host.realization_executor.sandbox_spec(thread);
    let environment = Arc::new(
        host.realization_executor
            .environments()
            .create_session_environment(
                host.realization_executor.peers(),
                &host
                    .realization_executor
                    .environments()
                    .providers()
                    .session_provider,
                &spec,
            )
            .await
            .expect("create batch Artifact Container Environment"),
    );
    host.run_assembly
        .install_test_resident_session_environment(thread, environment);
}

#[tokio::test]
async fn live_container_artifact_harvest_reads_one_complete_batch() {
    // Cause/effect graph: C1 one live Container owns the canonical `/outputs`
    // root; C2 that root contains three binary-safe files; C3 one ordinary
    // harvest is requested. Effects: E1 the backend captures the complete tree
    // exactly once, independent of file count; E2 the ArtifactHarvester publishes
    // three exact path/content records; E3 no per-Artifact Container read port is
    // consulted. Decision rule B1: C1+C2+C3 => E1+E2+E3. The batch is ephemeral:
    // Files/ArtifactPublisher remains the sole durable output authority.
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let files = vec![
        awaken_container_contract::EnvironmentFile {
            path: "a.txt".into(),
            bytes: b"alpha".to_vec(),
        },
        awaken_container_contract::EnvironmentFile {
            path: "nested/b.bin".into(),
            bytes: vec![0, 0xff],
        },
        awaken_container_contract::EnvironmentFile {
            path: "c.json".into(),
            bytes: br#"{"ok":true}"#.to_vec(),
        },
    ];
    let provider = Arc::new(BatchArtifactContainerProvider {
        files,
        scans: scans.clone(),
    });
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_store_dir(storage.path())
        .await
        .with_session_container_provider(
            provider,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
        ))
    .build()
    .expect("validated Host dependencies");
    let thread = "container-artifact-batch";
    install_batch_artifact_environment(host.as_ref(), thread, "workspace-a").await;

    let harvested = host
        .harvest_thread_artifacts(thread)
        .await
        .expect("B1 harvest one batch");

    assert_eq!(
        scans.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "B1/E1 one complete tree capture"
    );
    assert_eq!(harvested.receipts.len(), 3, "B1/E2");
    assert_eq!(
        harvested
            .receipts
            .iter()
            .map(|receipt| receipt.record.logical_path.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["a.txt", "nested/b.bin", "c.json"],
        "B1/E2 exact logical paths"
    );
    let records = host
        .file_application()
        .expect("B1 File application")
        .list("workspace-a", Some(thread))
        .await
        .expect("B1 list durable Files");
    assert_eq!(records.len(), 3, "B1/E2 durable records");
}

#[tokio::test]
async fn terminal_release_harvests_outputs_idempotently_before_sandbox_disposal() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect decision table:
    // R1 output under the canonical layout root + live Sandbox => harvest
    // creates one scoped File with the exact root-relative logical path.
    // R2 identical retry => same File id, no duplicate manifest row/reference.
    // R3 terminal preparation => File is durable and Sandbox remains; R4 the
    // aggregate records that exact preparation plus its Repository participant,
    // then and only then projects physical disposal => Sandbox gone while File
    // metadata/bytes remain.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    host.capabilities
        .register_thread_workspace("session-artifacts", "workspace-a")
        .expect("installed Sessions");
    install_terminal_artifact_projection_before_environment(
        host.as_ref(),
        "session-artifacts",
        "workspace-a",
    )
    .await;
    let lease = terminal_cleanup_test_lease();
    let spec = host.realization_executor.sandbox_spec("session-artifacts");
    let create_fence = lease
        .sandbox_effect_fence("session-artifacts-create")
        .unwrap();
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(
            host.test_local_provider()
                .create_sandbox_for_effect(
                    &spec,
                    &create_fence,
                    None,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &create_fence,
                    ),
                )
                .await
                .unwrap(),
        )
        .into_session_environment(),
    );
    install_terminal_artifact_environment(
        host.as_ref(),
        "session-artifacts",
        "workspace-a",
        environment.clone(),
    );
    let output = storage
        .path()
        .join("sandboxes")
        .join("session-artifacts")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"durable report").unwrap();

    let first = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    let retry = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    assert_eq!(first.receipts.len(), 1);
    assert_eq!(retry.receipts[0].record.id, first.receipts[0].record.id);
    assert!(first.receipts[0].record.id.starts_with("file_"));
    assert!(first.receipts[0].record.downloadable);
    assert_eq!(
        first.receipts[0].record.logical_path.as_deref(),
        Some("report.txt"),
        "R1 canonical output-root projection"
    );

    let (mut session, effect) =
        root_terminal_cleanup_effect(host.as_ref(), "session-artifacts", lease).await;
    let managed = host.clone();
    let workspace_id = host
        .run_assembly
        .installed_sessions
        .realizations()
        .frozen("session-artifacts")
        .unwrap()
        .workspace
        .clone()
        .expect("R4 frozen aggregate Repository inputs");
    let authorization = root_terminal_preparation_authorization(&session, &effect, &workspace_id);
    let preparation = managed
        .run_assembly
        .prepare_terminal_cleanup_for_effect(effect.clone(), authorization)
        .await
        .unwrap();
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("session-artifacts", |r| {
                r.environment().owner.terminal_bound_environment().is_some()
            })
            .unwrap()
            .unwrap_or(false),
        "R3 exact owner remains Retiring until aggregate-authorized Disposal"
    );
    let repository_preparation =
        awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
            &awaken_session_types::SessionId::from("session-artifacts"),
            &workspace_id,
            &session.resources,
        )
        .unwrap();
    session
        .record_terminal_cleanup_preparation(
            &workspace_id,
            &effect.lease,
            preparation,
            Some(repository_preparation),
        )
        .unwrap();
    let disposal_command = match session
        .terminal_cleanup_work_action()
        .unwrap()
        .expect("R4 durable preparation projects disposal")
    {
        awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => command,
        action => panic!("R4 expected aggregate disposal action, got {action:?}"),
    };
    let disposal = awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
        disposal_command,
        effect.lease,
    );
    managed
        .realization_executor
        .dispose_terminal_cleanup_for_effect(disposal)
        .await
        .unwrap();
    assert!(
        host.run_assembly
            .resident_session_environment("session-artifacts")
            .await
            .is_none()
    );
    let records = host
        .file_application()
        .expect("test startup installs File application")
        .list("workspace-a", Some("session-artifacts"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "terminal retry remains idempotent");
    assert_eq!(
        host.file_application()
            .expect("test startup installs File application")
            .bytes("workspace-a", &records[0].id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"durable report"
    );
}

#[tokio::test]
async fn terminal_artifact_response_loss_recovers_the_same_receipt_without_an_environment() {
    // Cause/effect decision table: H1 ordinary harvest then terminal harvest
    // of identical Session/path/content => one File id with a durable terminal
    // association; H2 terminal publication committed but its response/slot
    // Environment is lost => root Workspace + File evidence reconstruct the
    // exact receipt; H3 later logical delete => tombstone readback still
    // reconstructs that same creation receipt; H4 env absent under ordinary
    // Run semantics => empty, never a broad catalog recovery.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_store_dir(storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    host.capabilities
        .register_thread_workspace("artifact-response-loss", "workspace-a")
        .expect("installed Sessions");
    install_terminal_artifact_projection_before_environment(
        host.as_ref(),
        "artifact-response-loss",
        "workspace-a",
    )
    .await;
    let lease = terminal_cleanup_test_lease();
    let spec = host
        .realization_executor
        .sandbox_spec("artifact-response-loss");
    let create_fence = lease
        .sandbox_effect_fence("artifact-response-loss-create")
        .unwrap();
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(
            host.test_local_provider()
                .create_sandbox_for_effect(
                    &spec,
                    &create_fence,
                    None,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &create_fence,
                    ),
                )
                .await
                .unwrap(),
        )
        .into_session_environment(),
    );
    install_terminal_artifact_environment(
        host.as_ref(),
        "artifact-response-loss",
        "workspace-a",
        environment,
    );
    let output = storage
        .path()
        .join("sandboxes")
        .join("artifact-response-loss")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("receipt.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"committed before response loss").unwrap();

    let ordinary = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest("artifact-response-loss")
        .await
        .expect("H1 ordinary");
    let (_cleanup, effect) =
        root_terminal_cleanup_effect(host.as_ref(), "artifact-response-loss", lease).await;
    let terminal = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest_with_fence(
            "artifact-response-loss",
            Some(crate::ResourceOperationFence::Terminal(effect.clone())),
        )
        .await
        .expect("H1 terminal");
    assert_eq!(
        ordinary.receipts[0].record.id, terminal.receipts[0].record.id,
        "H1"
    );

    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization("artifact-response-loss", |r| {
            r.environment_mut().owner = crate::environment_owner::SessionEnvironmentOwner::Vacant;
        })
        .unwrap();
    let recovered = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest_with_fence(
            "artifact-response-loss",
            Some(crate::ResourceOperationFence::Terminal(effect.clone())),
        )
        .await
        .expect("H2 readback");
    assert_eq!(recovered.receipts, terminal.receipts, "H2");

    host.file_application()
        .expect("File application")
        .delete(
            "workspace-a",
            &terminal.receipts[0].record.id,
            awaken_session_realization::test_support::wall_clock_now_ms(),
        )
        .await
        .expect("H3 logical delete");
    let after_delete = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest_with_fence(
            "artifact-response-loss",
            Some(crate::ResourceOperationFence::Terminal(effect)),
        )
        .await
        .expect("H3 tombstone readback");
    assert_eq!(after_delete.receipts, terminal.receipts, "H3");
    assert!(
        host.realization_executor
            .effects()
            .artifacts()
            .clone()
            .harvest("artifact-response-loss")
            .await
            .expect("H4 ordinary absent")
            .receipts
            .is_empty(),
        "H4"
    );
}

#[derive(Default)]
struct RecordingArtifactRecovery {
    publish_calls: std::sync::atomic::AtomicUsize,
    recover_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>
    for RecordingArtifactRecovery
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.publish_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "unexpected publication",
        ))
    }

    async fn recover(
        &self,
        _recovery: awaken_resource_contract::ArtifactRecovery<crate::ResourceOperationFence>,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.recover_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn capture(
        &self,
        _workspace_id: &str,
        _session_id: &str,
        _fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not capture Artifacts",
        ))
    }

    async fn replace_capture(
        &self,
        _request: awaken_resource_contract::ReplaceArtifactCapture,
        _fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not replace Artifact captures",
        ))
    }
}

#[tokio::test]
async fn receipt_only_terminal_harvest_skips_a_present_container() {
    // Cause/effect graph: C1 an exact terminal fence is present; C2 cleanup
    // evidence selects ReceiptOnly; C3 a cleanup-only Container Environment is
    // still installed; C4 durable publication receipts may already exist.
    // Effects: E1 no live output batch or per-file port is called; E2 no new
    // publication is attempted; E3 the one ArtifactHarvester owner invokes
    // publisher recovery exactly once. Rule B2: C1+C2+C3+C4 => E1+E2+E3.
    // Complement B3: ordinary Run + absent Environment => empty and zero
    // additional recovery. B4: ReceiptOnly without a terminal fence is rejected
    // before output or catalog I/O; it never broadens non-terminal authority.
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = Arc::new(BatchArtifactContainerProvider {
        files: vec![awaken_container_contract::EnvironmentFile {
            path: "must-not-be-read.txt".into(),
            bytes: b"already durable".to_vec(),
        }],
        scans: scans.clone(),
    });
    let publisher = Arc::new(RecordingArtifactRecovery::default());
    let mut raw_host = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_session_container_provider(
            provider,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
        );
    raw_host.resources.artifact_publisher = publisher.clone();
    let host = (raw_host).build().expect("validated Host dependencies");
    let thread = "container-artifact-receipt-only";
    install_batch_artifact_environment(host.as_ref(), thread, "workspace-a").await;

    let mut cleanup = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(
        cleanup.request(&awaken_session_types::SessionId::from(thread)),
        "B2 terminal request"
    );
    cleanup
        .freeze_targets(&awaken_session_types::SessionId::from(thread), [], 0, 0)
        .expect("B2 freeze root target");
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        cleanup
            .command_for(&awaken_session_types::SessionId::from(thread), thread)
            .expect("B2 root command"),
        terminal_cleanup_test_lease(),
    );
    let recovered = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest_with_fence_mode(
            thread,
            Some(crate::ResourceOperationFence::Terminal(effect)),
            ArtifactCaptureMode::ReceiptOnly,
        )
        .await
        .expect("B2 receipt-only recovery");

    assert!(recovered.receipts.is_empty(), "B2 fixture receipt set");
    assert_eq!(
        scans.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "B2/E1 zero live capture"
    );
    assert_eq!(
        publisher
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "B2/E2"
    );
    assert_eq!(
        publisher
            .recover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "B2/E3"
    );

    assert!(
        host.realization_executor
            .effects()
            .artifacts()
            .clone()
            .harvest("ordinary-absent")
            .await
            .expect("B3 ordinary absence")
            .receipts
            .is_empty(),
        "B3 empty result"
    );
    assert_eq!(
        publisher
            .recover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "B3 zero additional recovery"
    );
    let error = host
        .realization_executor
        .effects()
        .artifacts()
        .clone()
        .harvest_with_fence_mode("ordinary-absent", None, ArtifactCaptureMode::ReceiptOnly)
        .await
        .expect_err("B4 non-terminal receipt-only must fail closed");
    assert!(
        matches!(error, ArtifactPublicationError::InvalidRequest { .. }),
        "B4"
    );
    assert_eq!(
        publisher
            .recover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "B4 zero catalog I/O"
    );
}

#[tokio::test]
async fn terminal_artifact_recovery_requires_the_root_workspace_before_catalog_io() {
    // Cause/effect decision table: H5 a child terminal command is exact but
    // its root Session projection is missing. E5 fail before any recovery
    // read and never substitute the process-local Workspace. This is the
    // fail-closed complement to H2/H3 above; the root assignment is the
    // only tenant authority for both live publication and readback.
    let mut operation = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(operation.request(&awaken_session_types::SessionId::from("missing-root")));
    operation
        .freeze_targets(
            &awaken_session_types::SessionId::from("missing-root"),
            ["missing-child".to_string()],
            0,
            0,
        )
        .unwrap();
    let command = operation
        .command_for(
            &awaken_session_types::SessionId::from("missing-root"),
            "missing-child",
        )
        .unwrap();
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        command,
        terminal_cleanup_test_lease(),
    );
    let publisher = Arc::new(RecordingArtifactRecovery::default());
    let harvester = ArtifactHarvester::new(
        Arc::new(crate::installed_session::InstalledSessions::default()).realizations(),
        "must-not-be-used".into(),
        publisher.clone(),
    );

    let error = harvester
        .harvest_with_fence(
            "missing-child",
            Some(crate::ResourceOperationFence::Terminal(effect)),
        )
        .await
        .expect_err("H5/E5");
    assert!(
        matches!(error, ArtifactPublicationError::AuthorityUnavailable { .. }),
        "H5/E5"
    );
    assert_eq!(
        publisher
            .recover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "H5/E5 zero catalog reads"
    );
}

#[tokio::test]
async fn retained_artifact_harvester_observes_late_realization_without_retaining_host() {
    // Ownership decision table: R1 a harvester is captured before Session
    // installation => no outputs; R2 the same Session slot later receives an
    // Environment => the captured component sees that exact Environment;
    // R3 the Host is dropped => publication still uses the selected authority,
    // without a strong back-reference; R4 replay => one durable File identity.
    let storage = tempfile::tempdir().unwrap();
    let host = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .build()
        .expect("Host");
    let harvester = host.realization_executor.effects().artifacts().clone();
    let application = host.file_application().expect("File application");
    assert!(
        harvester
            .harvest("late-output")
            .await
            .unwrap()
            .receipts
            .is_empty(),
        "R1"
    );

    host.capabilities
        .register_thread_workspace("late-output", "workspace-late")
        .expect("installed Sessions");
    let spec = agent_run_sandbox_spec("late-output");
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        )
        .into_session_environment(),
    );
    host.run_assembly
        .install_test_resident_session_environment("late-output", environment);
    let output = storage
        .path()
        .join("late-output")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(output, b"installed after the harvester").unwrap();
    let weak_host = Arc::downgrade(&host);
    drop(host);
    assert!(weak_host.upgrade().is_none(), "R3 no Host ownership cycle");

    let first = harvester.harvest("late-output").await.expect("R2/R3");
    assert_eq!(first.receipts.len(), 1, "R2");
    assert_eq!(
        application
            .bytes("workspace-late", &first.receipts[0].record.id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"installed after the harvester",
        "R3 exact publisher"
    );
    let replay = harvester.harvest("late-output").await.expect("R4");
    assert_eq!(
        replay.receipts[0].record.id, first.receipts[0].record.id,
        "R4"
    );
}

#[tokio::test]
async fn artifact_harvest_has_one_file_authority_and_no_magic_bundle_gate() {
    // Cause/effect decision table: C1 an output path happens to be named
    // `skill-export/change.patch`; C2 no manifest or checksum sibling exists;
    // C3 the same immutable bytes are harvested again. Effects: E1 the File
    // aggregate publishes the exact bytes without interpreting the directory;
    // E2 terminal harvest is not blocked by a second bundle-completion rule;
    // E3 replay returns the same File identity. R1 C1+C2=>E1+E2;
    // R2 C1+C2+C3=>E1+E2+E3.
    let storage = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await)
        .build()
        .expect("validated Host dependencies");
    host.capabilities
        .register_thread_workspace("plain-files", "workspace-a")
        .expect("installed Sessions");
    let spec = agent_run_sandbox_spec("plain-files");
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(
            LocalProvider::new(storage.path())
                .create_sandbox(&spec)
                .await
                .unwrap(),
        )
        .into_session_environment(),
    );
    host.run_assembly
        .install_test_resident_session_environment("plain-files", environment);
    let output = storage
        .path()
        .join("plain-files")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("skill-export")
        .join("change.patch");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"diff --git a/a b/a\n").unwrap();

    let first = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R1/E1+E2");
    let replay = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R2/E1+E2+E3");
    assert_eq!(first.receipts.len(), 1, "R1/E1");
    assert_eq!(replay.receipts.len(), 1, "R2/E3");
    assert_eq!(
        replay.receipts[0].record.id, first.receipts[0].record.id,
        "R2/E3"
    );
    // R3 real output bytes change A->B->A -> capture tracks the latest scan
    // using the original A File id, while historical B remains addressable.
    // R4 deleting the physical output -> explicit empty capture, no deletion
    // of prior Files. A missing Environment is different and preserves it.
    let application = host.file_application().unwrap();
    let mut versions = Vec::new();
    for bytes in [
        b"second revision".as_slice(),
        b"diff --git a/a b/a\n".as_slice(),
    ] {
        std::fs::write(&output, bytes).unwrap();
        let harvested = host.harvest_thread_artifacts("plain-files").await.unwrap();
        let capture = application
            .artifact_capture("workspace-a", "plain-files")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.file_ids,
            [harvested.receipts[0].record.id.clone()],
            "R3 exact selection"
        );
        assert_eq!(
            application
                .bytes("workspace-a", &capture.file_ids[0])
                .await
                .unwrap()
                .unwrap()
                .1,
            bytes,
            "R3 bytes"
        );
        versions.push(capture.file_ids[0].clone());
    }
    assert_ne!(versions[0], first.receipts[0].record.id);
    assert_eq!(versions[1], first.receipts[0].record.id, "R3 reused A");
    assert_eq!(
        application
            .list("workspace-a", Some("plain-files"))
            .await
            .unwrap()
            .len(),
        2,
        "R3 history"
    );
    std::fs::remove_file(&output).unwrap();
    assert!(
        host.harvest_thread_artifacts("plain-files")
            .await
            .unwrap()
            .receipts
            .is_empty()
    );
    let empty = application
        .artifact_capture("workspace-a", "plain-files")
        .await
        .unwrap()
        .unwrap();
    assert!(empty.file_ids.is_empty(), "R4 observed empty");
    assert_eq!(empty.revision, 5, "R4 one revision per full scan");
    assert_eq!(
        application
            .list("workspace-a", Some("plain-files"))
            .await
            .unwrap()
            .len(),
        2,
        "R4 no history deletion"
    );
}

struct FailingFileCatalog;

use awaken_resource_contract::CreateFileRecordOutcome;

#[async_trait::async_trait]
impl FileCatalog for FailingFileCatalog {
    async fn artifact_capture(
        &self,
        _workspace: &str,
        _session: &str,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, FileCatalogError> {
        // This fixture fails the later File write, not the pre-scan read.
        Ok(None)
    }

    async fn replace_artifact_capture(
        &self,
        _request: awaken_resource_contract::ReplaceArtifactCapture,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, FileCatalogError> {
        Err(FileCatalogError::Indeterminate {
            detail: "injected catalog replacement failure".into(),
        })
    }

    async fn create_file(
        &self,
        _record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        Err(FileCatalogError::Indeterminate {
            detail: "injected catalog failure".into(),
        })
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Ok(Vec::new())
    }

    async fn list_files_including_deleted(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Ok(Vec::new())
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
        Ok(0)
    }
}

#[tokio::test]
async fn terminal_harvest_failure_preserves_the_sandbox_for_retry() {
    // Test design. Causes: R4 has terminal output present while its durable
    // catalog write fails. Effects: terminal preparation fails and preserves
    // both the exact hidden Retiring Environment and output for retry.
    // Constraint/Invariant: sandbox disposal follows successful durable
    // harvest, never precedes it. Decision rule: execute R4 and require failure
    // with zero disposal.
    let storage = tempfile::tempdir().unwrap();
    let mut raw_host = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_store_dir(storage.path())
        .await;
    let catalog = Arc::new(FailingFileCatalog);
    raw_host.resources.file_catalog = catalog.clone();
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        raw_host.resources.file_store.clone(),
        catalog,
        raw_host
            .resources
            .resource_reclamation
            .clone()
            .expect("test lifecycle repository"),
        awaken_time::system_clock(),
    ));
    raw_host = raw_host.with_file_application(
        application.clone(),
        Arc::new(
            awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
        ),
        Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
    );
    let host = (raw_host).build().expect("validated Host dependencies");
    host.capabilities
        .register_thread_workspace("session-harvest-failure", "test")
        .expect("installed Sessions");
    install_terminal_artifact_projection_before_environment(
        host.as_ref(),
        "session-harvest-failure",
        "test",
    )
    .await;
    let lease = terminal_cleanup_test_lease();
    let spec = host
        .realization_executor
        .sandbox_spec("session-harvest-failure");
    let create_fence = lease
        .sandbox_effect_fence("session-harvest-failure-create")
        .unwrap();
    let environment = Arc::new(
        awaken_session_environment::SandboxEnvironment::workdir(
            host.test_local_provider()
                .create_sandbox_for_effect(
                    &spec,
                    &create_fence,
                    None,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &create_fence,
                    ),
                )
                .await
                .unwrap(),
        )
        .into_session_environment(),
    );
    install_terminal_artifact_environment(
        host.as_ref(),
        "session-harvest-failure",
        "test",
        environment,
    );
    let output = storage
        .path()
        .join("sandboxes")
        .join("session-harvest-failure")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"retry me").unwrap();

    let (cleanup, effect) =
        root_terminal_cleanup_effect(host.as_ref(), "session-harvest-failure", lease).await;
    let workspace_id = host
        .capabilities
        .registered_thread_workspace("session-harvest-failure")
        .expect("healthy registry")
        .expect("harvest failure retains the frozen Workspace");
    let authorization = root_terminal_preparation_authorization(&cleanup, &effect, &workspace_id);
    let error = (host.clone())
        .run_assembly
        .prepare_terminal_cleanup_for_effect(effect, authorization)
        .await
        .unwrap_err();
    assert!(error.message.contains("injected catalog failure"));
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("session-harvest-failure", |r| {
                r.environment().owner.terminal_bound_environment().is_some()
            })
            .unwrap()
            .unwrap_or(false),
        "R4 exact terminal owner remains hidden and retryable"
    );
    assert_eq!(std::fs::read(output).unwrap(), b"retry me");
}

struct CaptureFaultPublisher {
    inner: Arc<dyn awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>>,
    mode: &'static str,
    scans: Arc<std::sync::atomic::AtomicUsize>,
    calls: std::sync::Mutex<[usize; 3]>,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>
    for CaptureFaultPublisher
{
    async fn capture(
        &self,
        workspace: &str,
        session: &str,
        fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.calls.lock().unwrap()[0] += 1;
        assert_eq!(
            self.scans.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "F1 revision must precede scan"
        );
        if self.mode == "read_failure" {
            return Err(awaken_resource_contract::ArtifactPublicationError::new(
                "injected capture read failure",
            ));
        }
        let mut current = self.inner.capture(workspace, session, fence).await?;
        if self.mode == "corrupt_read" {
            current.as_mut().unwrap().workspace_id = "foreign".into();
        }
        if self.mode == "conflict" {
            self.inner
                .replace_capture(
                    awaken_resource_contract::ReplaceArtifactCapture {
                        origin: None,
                        workspace_id: workspace.into(),
                        session_id: session.into(),
                        expected_revision: current.as_ref().unwrap().revision,
                        file_ids: Vec::new(),
                    },
                    fence,
                )
                .await?;
        }
        Ok(current)
    }

    async fn publish(
        &self,
        request: awaken_resource_contract::ArtifactPublication<crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls[1] += 1;
            calls[1]
        };
        assert_eq!(
            self.scans.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "F1 one completed scan before publication"
        );
        if self.mode == "publish_first" && count == 1 || self.mode == "publish_second" && count == 2
        {
            return Err(awaken_resource_contract::ArtifactPublicationError::new(
                "injected File publication failure",
            ));
        }
        self.inner.publish(request).await
    }

    async fn replace_capture(
        &self,
        request: awaken_resource_contract::ReplaceArtifactCapture,
        fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.calls.lock().unwrap()[2] += 1;
        assert_eq!(
            self.calls.lock().unwrap()[1],
            2,
            "F1 all File publications precede replacement"
        );
        if self.mode == "replace_failure" {
            return Err(awaken_resource_contract::ArtifactPublicationError::new(
                "injected capture replacement failure",
            ));
        }
        if self.mode == "corrupt_reply" {
            return Ok(
                awaken_resource_contract::ReplaceArtifactCaptureOutcome::Applied(
                    awaken_resource_contract::ArtifactCapture {
                        origin: None,
                        workspace_id: request.workspace_id,
                        session_id: request.session_id,
                        revision: request.expected_revision + 1,
                        file_ids: vec!["foreign-file".into()],
                    },
                ),
            );
        }
        if self.mode == "replay" {
            self.inner.replace_capture(request.clone(), fence).await?;
        }
        self.inner.replace_capture(request, fence).await
    }

    async fn recover(
        &self,
        _recovery: awaken_resource_contract::ArtifactRecovery<crate::ResourceOperationFence>,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not recover Artifacts",
        ))
    }
}

#[tokio::test]
async fn artifact_capture_failures_do_not_publish_partial_or_rebased_sets() {
    // Causes: F1 canonical ordering; F2 read/first File/second File/CAS outage;
    // F3 another complete observation wins after revision read; F4 response
    // loss replay; F5 malformed read or replacement receipt. Effects: error
    // leaves the complete head unchanged (including a concurrently newer head),
    // successful/replayed CAS selects both outputs, and each File retains its
    // independent publication lifecycle. No conflict-triggered reread/rebase.
    // Table columns: mode, success, scans, File calls, CAS calls, head revision,
    // durable File count. This tests the real Harvester and resource application;
    // only the named boundary fault is injected into the existing publisher.
    for (mode, success, scans_expected, publishes, replaces, revision, files) in [
        ("success", true, 1, 2, 1, 2, 2),
        ("replay", true, 1, 2, 1, 2, 2),
        ("read_failure", false, 0, 0, 0, 1, 0),
        ("corrupt_read", false, 0, 0, 0, 1, 0),
        ("publish_first", false, 1, 1, 0, 1, 0),
        ("publish_second", false, 1, 2, 0, 1, 1),
        ("replace_failure", false, 1, 2, 1, 1, 2),
        ("conflict", false, 1, 2, 1, 2, 2),
        ("corrupt_reply", false, 1, 2, 1, 1, 2),
    ] {
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(BatchArtifactContainerProvider {
            files: vec![
                awaken_container_contract::EnvironmentFile {
                    path: "a.txt".into(),
                    bytes: b"a".to_vec(),
                },
                awaken_container_contract::EnvironmentFile {
                    path: "b.txt".into(),
                    bytes: b"b".to_vec(),
                },
            ],
            scans: scans.clone(),
        });
        let mut dependencies = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
            .await
            .with_session_container_provider(
                provider,
                Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            );
        let application = dependencies.resources.file_application.clone().unwrap();
        let request = awaken_resource_contract::ReplaceArtifactCapture {
            origin: None,
            workspace_id: "workspace-a".into(),
            session_id: "capture-fault".into(),
            expected_revision: 0,
            file_ids: Vec::new(),
        };
        application.replace_artifact_capture(request).await.unwrap();
        let publisher = Arc::new(CaptureFaultPublisher {
            inner: dependencies.resources.artifact_publisher.clone(),
            mode,
            scans: scans.clone(),
            calls: std::sync::Mutex::new([0; 3]),
        });
        dependencies.resources.artifact_publisher = publisher.clone();
        let host = (dependencies).build().expect("validated Host dependencies");
        install_batch_artifact_environment(&host, "capture-fault", "workspace-a").await;
        let result = host.harvest_thread_artifacts("capture-fault").await;
        assert_eq!(result.is_ok(), success, "{mode}: {result:?}");
        // F6 classification survives the real Host boundary: IO/response loss
        // stays indeterminate; a bad observation stays corrupt; a known CAS
        // conflict stays conflict. No Storage(String) wrapper may erase it.
        if let Err(error) = &result {
            use awaken_resource_contract::{ArtifactPublicationError as Failure, FileCatalogError};
            match mode {
                "corrupt_read" | "corrupt_reply" => assert!(
                    matches!(
                        error,
                        Failure::Catalog(FileCatalogError::CorruptData { .. })
                    ),
                    "{mode}: {error:?}"
                ),
                "conflict" => assert!(
                    matches!(error, Failure::Conflict { .. }),
                    "{mode}: {error:?}"
                ),
                _ => assert!(
                    matches!(error, Failure::Indeterminate { .. }),
                    "{mode}: {error:?}"
                ),
            }
        }
        assert_eq!(
            scans.load(std::sync::atomic::Ordering::SeqCst),
            scans_expected,
            "{mode}"
        );
        assert_eq!(
            *publisher.calls.lock().unwrap(),
            [1, publishes, replaces],
            "{mode}: no hidden retry"
        );
        let capture = application
            .artifact_capture("workspace-a", "capture-fault")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(capture.revision, revision, "{mode}");
        assert_eq!(
            capture.file_ids.len(),
            if success { 2 } else { 0 },
            "{mode}: no partial or forged head"
        );
        assert_eq!(
            application
                .list("workspace-a", Some("capture-fault"))
                .await
                .unwrap()
                .len(),
            files,
            "{mode}: independent File lifecycle"
        );
    }
}
