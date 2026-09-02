use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct ContinuationRuntime {
    fail_quiesce_once: AtomicBool,
    fail_checkpoint_once: AtomicBool,
    fail_prepare_once: AtomicBool,
    fail_dispose_once: AtomicBool,
    fail_restore_once: AtomicBool,
    fail_adopt_once: AtomicBool,
    fail_delete_once: AtomicBool,
    quiesces: AtomicUsize,
    checkpoints: AtomicUsize,
    preparations: AtomicUsize,
    disposals: AtomicUsize,
    restores: AtomicUsize,
    projection_attempts: AtomicUsize,
    adoption_attempts: AtomicUsize,
    deletes: AtomicUsize,
    terminal_cleanups: AtomicUsize,
    terminal_checkpoint_projections: AtomicUsize,
    checkpoint_scopes: Mutex<Vec<(String, u64)>>,
    expected_mcp_generations: Mutex<Vec<Vec<awaken_session_contract::McpGenerationRef>>>,
    quiescence_mcp_override: Mutex<Option<Vec<awaken_session_contract::McpGenerationRef>>>,
    source_tuples: Mutex<Vec<(&'static str, String, String, String)>>,
    preparation_effects: Mutex<Vec<awaken_session_contract::SourceReleasePreparationEffect>>,
    disposal_authorizations: Mutex<Vec<awaken_provisioning_contract::SandboxDisposalAuthorization>>,
    projection_installs: Mutex<
        Vec<(
            String,
            awaken_session_contract::FrozenSessionProjection,
            awaken_session_contract::SessionProjectionInstallMode,
        )>,
    >,
    adoptions: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
impl SessionRuntime for ContinuationRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        self.projection_attempts.fetch_add(1, Ordering::SeqCst);
        let successful_install = (thread.to_string(), projection.clone(), mode.clone());
        install_complete_test_projection(self, thread, projection, mode).await?;
        self.projection_installs
            .lock()
            .unwrap()
            .push(successful_install);
        Ok(())
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        self.adoption_attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_adopt_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected adoption failure"));
        }
        self.adoptions.lock().unwrap().push((
            agent.to_string(),
            thread.to_string(),
            binding.to_string(),
        ));
        Ok(())
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupEffect,
        _authorization: awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_contract::SessionCleanupPreparation, RunError> {
        let provider_prepared_effect_fence = effect
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        awaken_session_contract::SessionCleanupPreparation::try_new(
            &effect,
            provider_prepared_effect_fence,
            Vec::new(),
        )
        .map_err(|error| RunError::internal(error.to_string()))
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_contract::SessionCleanupDisposalReceipt, RunError> {
        self.terminal_cleanups.fetch_add(1, Ordering::SeqCst);
        Ok(awaken_session_contract::SessionCleanupDisposalReceipt::new(
            &effect.command,
        ))
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
    ) -> Result<(), RunError> {
        if assignment.projection.environment.checkpoint().is_some() {
            self.terminal_checkpoint_projections
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(None)
    }

    async fn quiesce_session_environment(
        &self,
        _thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_contract::McpGenerationRef],
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.quiesces.fetch_add(1, Ordering::SeqCst);
        self.source_tuples.lock().unwrap().push((
            "quiesce",
            source_effect_id.into(),
            source_binding.into(),
            generation.id.clone(),
        ));
        self.expected_mcp_generations
            .lock()
            .unwrap()
            .push(expected_mcp_generations.to_vec());
        if self.fail_quiesce_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected quiescence crash"));
        }
        let mcp_generations = self
            .quiescence_mcp_override
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| expected_mcp_generations.to_vec());
        Ok(awaken_session_contract::QuiescenceReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            activity_epoch: operation.activity_epoch,
            live_environment_effects: 0,
            mcp_generations,
        })
    }

    async fn checkpoint_session_environment(
        &self,
        _thread: &str,
        request: awaken_session_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_contract::CheckpointReceipt, RunError> {
        self.checkpoints.fetch_add(1, Ordering::SeqCst);
        self.source_tuples.lock().unwrap().push((
            "checkpoint",
            request.source_effect_id.clone(),
            request.source_binding.clone(),
            request.generation.id.clone(),
        ));
        self.checkpoint_scopes
            .lock()
            .unwrap()
            .push((request.workspace_id.clone(), request.created_at_unix_ms));
        if self.fail_checkpoint_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected upload crash"));
        }
        Ok(awaken_session_contract::CheckpointReceipt {
            effect_id: request.operation.effect_id.clone(),
            generation_id: request.generation.id.clone(),
            checkpoint: awaken_session_contract::SandboxCheckpointRef {
                id: "object".into(),
                format: request.format,
                digest: "digest".into(),
                size_bytes: 10,
                created_at_unix_ms: request.created_at_unix_ms,
                expires_at_unix_ms: request.expires_at_unix_ms,
                environment_fingerprint: request.generation.environment_fingerprint,
                base_image_fingerprint: request.generation.base_image_fingerprint,
                excluded_mounts: Vec::new(),
                suspend_effect_id: request.operation.effect_id,
            },
        })
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        _thread: &str,
        preparation: &awaken_session_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceReleasePreparedReceipt, RunError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        self.preparation_effects
            .lock()
            .unwrap()
            .push(preparation.clone());
        if self.fail_prepare_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected source preparation crash"));
        }
        let provider_prepared_effect_fence = preparation
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        awaken_session_contract::SourceReleasePreparedReceipt::try_new(
            preparation.clone(),
            provider_prepared_effect_fence,
            generation,
            source_binding,
        )
        .map_err(|error| RunError::internal(error.to_string()))
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        _thread: &str,
        disposal: &awaken_session_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.disposals.fetch_add(1, Ordering::SeqCst);
        self.disposal_authorizations.lock().unwrap().push(
            disposal.sandbox_disposal_authorization().map_err(|error| {
                RunError::unavailable_classified("test_source_disposal_invalid", error.to_string())
            })?,
        );
        if self.fail_dispose_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected disposal crash"));
        }
        Ok(awaken_session_contract::SourceDisposedReceipt {
            effect_id: disposal.operation().effect_id.clone(),
            generation_id: disposal.generation().id.clone(),
            source_binding: disposal.source_binding().into(),
            terminated: true,
        })
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.fail_restore_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected restore crash"));
        }
        Ok(awaken_session_contract::RestoreReceipt {
            effect_id: request.effect_id,
            generation_id: request.generation_id,
            checkpoint_id: request.checkpoint.id,
            binding: "restored-binding".into(),
        })
    }

    async fn delete_session_checkpoint(
        &self,
        _thread: &str,
        _checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        if self.fail_delete_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected delete crash"));
        }
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("continuation tests do not execute a model")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("continuation tests do not resume")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("continuation tests do not resume")
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        unreachable!("continuation tests do not define outcomes")
    }

    fn model(&self) -> String {
        "continuation-test".into()
    }
}

pub(super) fn checkpoint_release_retention()
-> awaken_session_contract::EnvironmentIdleRetentionPolicy {
    awaken_session_contract::EnvironmentIdleRetentionPolicy {
        mode: awaken_session_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 1,
        retention_secs: 100,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 1024,
        max_checkpoint_duration_secs: 5,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    }
}

fn generated_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let baseline = match &mut session.baseline {
        awaken_session_contract::SessionBaselineState::Frozen(baseline) => baseline,
        _ => unreachable!(),
    };
    baseline.environment.idle_retention = checkpoint_release_retention();
    let generation = awaken_session_contract::SandboxGeneration::new(
        id,
        0,
        100_000,
        baseline.environment.config_fingerprint.0.clone(),
        "base",
    );
    session.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: "source-binding".into(),
        effect_id: Some("create".into()),
        generation: Some(generation),
        idle_since_unix_ms: Some(1),
    };
    session.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: "continuation-worker-a".into(),
        runtime_incarnation: "continuation-runtime-a".into(),
        epoch: 4,
        expires_at_unix_ms: 100_000,
    });
    session
}

/// Add the one durable Active MCP generation used by continuation tests. This
/// fixture writes the same Session aggregate fields that the production MCP
/// admission owns; callers vary only the generation set and receipt returned by
/// the existing Runtime fake.
fn add_acknowledged_active_mcp(
    session: &mut PersistedSession,
    name: &str,
    generation: u64,
) -> awaken_session_contract::McpGenerationRef {
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX,
    };
    session.realization = Some(lease.clone());
    let attachment_id = awaken_session_contract::McpAttachmentId(name.into());
    session
        .mcp
        .desired_names
        .get_or_insert_with(Default::default)
        .insert(name.into());
    session
        .mcp
        .attachments
        .push(awaken_session_contract::SessionMcpAttachment {
            attachment_id: attachment_id.clone(),
            name: name.into(),
            generation: awaken_session_contract::McpGeneration(generation),
            target: awaken_session_contract::McpTarget::parse_http(format!(
                "https://{name}.example.test/mcp"
            ))
            .expect("continuation MCP fixture endpoint is valid"),
            prompts_as_skills: false,
            origin: awaken_session_contract::McpAttachmentOrigin::Agent,
            credential: None,
            selected_plaintext_holder: None,
            state: awaken_session_contract::McpAttachmentState::Active,
            publication_acknowledged: true,
            realization: Some(awaken_session_contract::McpRealizationClaim {
                realization_id: format!("realize-{name}-{generation}"),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                stage_idempotency_key: format!("stage-{name}-{generation}"),
            }),
            attempts: 1,
            last_error: None,
        });
    awaken_session_contract::McpGenerationRef {
        session_id: session.session_id.clone(),
        attachment_id,
        generation: awaken_session_contract::McpGeneration(generation),
        runtime_incarnation: lease.runtime_incarnation,
        lease_epoch: lease.epoch,
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
    }
}

fn externally_placed(mut session: PersistedSession) -> PersistedSession {
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
    else {
        unreachable!("continuation fixture baseline is frozen")
    };
    baseline.runtime_placement = awaken_session_contract::SessionRuntimePlacement::Worker;
    session
}

fn memory_input(
    binding_id: &str,
    access: awaken_resource_contract::ResourceAccess,
) -> awaken_session_contract::ResolvedInput {
    awaken_session_contract::ResolvedInput {
        binding_id: awaken_resource_contract::BindingId::from(binding_id),
        source: awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("continuation-memory"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                    "continuation-memory",
                ),
                version: awaken_resource_contract::ConfigVersion(1),
                retention_policy: Default::default(),
            },
        },
        mount_path: format!("/memory/{binding_id}"),
        access,
        instructions: None,
    }
}

fn legacy_memory_binding(id: &str) -> String {
    serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::local(
        id,
        awaken_provisioning_contract::LocalSandboxHandleV1 {
            outputs_path: "/mnt/session/outputs".into(),
            base_env: Vec::new(),
            continuation_excluded_paths: Vec::new(),
            deny_tool_egress: false,
        },
    ))
    .unwrap()
}

pub(super) fn hibernated_session(id: &str, expires_at_unix_ms: u64) -> PersistedSession {
    let mut session = generated_session(id);
    let source_generation = session.environment.generation().unwrap();
    let generation = awaken_session_contract::SandboxGeneration::new(
        id,
        source_generation.created_at_unix_ms,
        expires_at_unix_ms,
        source_generation.environment_fingerprint.clone(),
        source_generation.base_image_fingerprint.clone(),
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        id,
        "suspend",
        &generation,
        0,
        None,
        None,
    );
    session.environment = awaken_session_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_session_contract::SandboxCheckpointRef {
            id: "object".into(),
            format: "awaken-fs-tar-v1".into(),
            digest: "digest".into(),
            size_bytes: 10,
            created_at_unix_ms: 10,
            expires_at_unix_ms,
            environment_fingerprint: generation.environment_fingerprint.clone(),
            base_image_fingerprint: generation.base_image_fingerprint.clone(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: operation.effect_id,
        },
        generation,
    };
    session
}

// Cause/effect design: C1=due Resident, C2/C3=quiescent, C4=current epoch,
// C5=checkpoint success, C6=termination success, C8=no terminal race,
// C9=repository owner scope. Decision rule R3 => E2 through each phase then E4;
// the byte adapter receives exact Workspace + creation time, both
// source-dependent calls receive the same physical source tuple, and replay in
// Hibernated emits no effect. The prepared disposal fence is covered by the
// adjacent two-stage artifact tests; this fake does not reconstruct source
// facts that the disposal API no longer accepts.
#[tokio::test]
async fn due_idle_environment_suspends_once_in_order() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), generated_session("suspend")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.reconcile_environment_continuation("suspend", 2_000)
        .await
        .unwrap();
    let settled = repo.get("suspend").await.unwrap();
    let generation_id = settled
        .environment
        .generation()
        .expect("hibernated generation")
        .id
        .clone();
    assert!(matches!(
        settled.environment,
        awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ));
    app.reconcile_environment_continuation("suspend", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(
        *runtime.checkpoint_scopes.lock().unwrap(),
        [("workspace".into(), 2_000)]
    );
    assert_eq!(
        *runtime.source_tuples.lock().unwrap(),
        [
            (
                "quiesce",
                "create".into(),
                "source-binding".into(),
                generation_id.clone(),
            ),
            (
                "checkpoint",
                "create".into(),
                "source-binding".into(),
                generation_id,
            ),
        ],
        "R19/R20 exact physical source tuple reaches every source-dependent boundary"
    );
}

#[tokio::test]
async fn suspend_admission_reuses_the_closed_memory_evidence_decision() {
    // Admission cause/effect table (the detailed None/Some matrix is owned by
    // the neutral contract test): A1 active RW + legacy None => remain Resident
    // and zero Runtime effect; A2 pending Memory generation => remain Resident
    // because no single active input/evidence join exists; A3 active RO +
    // legacy None => no write-back obligation, so the ordinary suspend saga may
    // proceed. Explicit Some([]), exact RO Copy, RW Copy, and foreign evidence
    // are delegated unchanged to the same contract wrapper (rules C5-C8).
    for (id, mut session, should_suspend) in {
        let mut legacy_rw = generated_session("memory-legacy-rw");
        legacy_rw.resources = awaken_session_contract::SessionResourceState::from_active(
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![memory_input(
                    "rw",
                    awaken_resource_contract::ResourceAccess::ReadWrite,
                )],
                Vec::new(),
            )
            .unwrap(),
        );
        if let awaken_session_contract::SessionEnvironmentState::Resident { binding, .. } =
            &mut legacy_rw.environment
        {
            *binding = legacy_memory_binding("memory-legacy-rw");
        }

        let mut pending = generated_session("memory-pending");
        pending.resources.pending = Some(
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![memory_input(
                    "pending",
                    awaken_resource_contract::ResourceAccess::ReadOnly,
                )],
                Vec::new(),
            )
            .unwrap(),
        );

        let mut legacy_ro = generated_session("memory-legacy-ro");
        legacy_ro.resources = awaken_session_contract::SessionResourceState::from_active(
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![memory_input(
                    "ro",
                    awaken_resource_contract::ResourceAccess::ReadOnly,
                )],
                Vec::new(),
            )
            .unwrap(),
        );
        if let awaken_session_contract::SessionEnvironmentState::Resident { binding, .. } =
            &mut legacy_ro.environment
        {
            *binding = legacy_memory_binding("memory-legacy-ro");
        }

        [
            ("memory-legacy-rw", legacy_rw, false),
            ("memory-pending", pending, false),
            ("memory-legacy-ro", legacy_ro, true),
        ]
    } {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        session.session_id = id.into();
        create(repo.as_ref(), session).await;
        let runtime = Arc::new(ContinuationRuntime::default());
        let app = application_with_runtime(
            runtime.clone(),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let result = app.reconcile_environment_continuation(id, 2_000).await;
        if should_suspend {
            result.expect("A3 RO suspension");
            assert!(matches!(
                repo.get(id).await.unwrap().environment,
                awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
            ));
            assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1, "A3");
        } else {
            assert!(result.is_err(), "A1/A2");
            assert!(matches!(
                repo.get(id).await.unwrap().environment,
                awaken_session_contract::SessionEnvironmentState::Resident { .. }
            ));
            assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 0, "A1/A2");
            assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 0, "A1/A2");
            assert_eq!(runtime.disposals.load(Ordering::SeqCst), 0, "A1/A2");
        }
    }
}

#[tokio::test]
async fn quiescence_requests_the_exact_durable_mcp_generation_set() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut session = generated_session("suspend-with-mcp");
    let generation = add_acknowledged_active_mcp(&mut session, "browser", 3);
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    // Cause/effect decision table:
    // | Rule | Durable Active set | Phase | Effect |
    // | D1 | exact browser generation | Quiescing | pass exact set to Runtime; CAS clears only publication ack |
    // | D2 | same durable generation | Hibernated | direct/scan realization is NotReady and performs no effect |
    // | D3 | same durable generation | restored Resident | existing Stage -> Publish -> ack protocol reprojects it |
    // This prevents an empty/partial process-local projection from becoming the
    // desired ownership authority, while H22 in the Runtime-host suite proves
    // the same exact Stage rebuilds a quiescence-created Removed tombstone.
    app.reconcile_environment_continuation("suspend-with-mcp", 2_000)
        .await
        .unwrap();
    assert_eq!(
        runtime.expected_mcp_generations.lock().unwrap().as_slice(),
        [vec![generation.clone()]],
        "D1 exact active set"
    );
    let hibernated = repo.get("suspend-with-mcp").await.unwrap();
    assert!(
        matches!(
            hibernated.environment,
            awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
        ),
        "D1"
    );
    assert!(
        !hibernated.mcp.attachments[0].publication_acknowledged,
        "D1 quiescence CAS requires reprojection"
    );

    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    assert!(
        matches!(
            awaken_session_contract::SessionRealizationControl::begin_session_realization(
                &app,
                awaken_session_contract::BeginSessionRealization {
                    session_id: "suspend-with-mcp".into(),
                    target: target.clone(),
                },
            )
            .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        ),
        "D2 direct realization gate"
    );
    let scan = app.reconcile_session_realizations().await;
    assert!(scan.settled.is_empty(), "D2 scan gate");
    assert!(scan.failures.is_empty(), "D2 scan gate");
    assert!(
        !repo.get("suspend-with-mcp").await.unwrap().mcp.attachments[0].publication_acknowledged,
        "D2 no early publication"
    );

    app.ensure_environment_resident("suspend-with-mcp", 2_000)
        .await
        .unwrap();
    let staged = awaken_session_contract::SessionRealizationControl::begin_session_realization(
        &app,
        awaken_session_contract::BeginSessionRealization {
            session_id: "suspend-with-mcp".into(),
            target,
        },
    )
    .await
    .expect("D3 restored Resident may reproject");
    let awaken_session_contract::SessionRealizationAction::Stage {
        prepare_session,
        mcp_stages,
    } = staged.action
    else {
        panic!("D3 expected Stage")
    };
    assert!(!prepare_session, "D3 reuses the restored Environment");
    assert_eq!(mcp_stages.len(), 1, "D3 exact Stage");
    let request = &mcp_stages[0];
    assert_eq!(request.generation, generation, "D3 exact Stage");
    let receipt = awaken_session_contract::McpRealizationReceipt {
        generation: request.generation.clone(),
        realization_id: request.realization_id.clone(),
        selected_plaintext_holder: request.selected_plaintext_holder.clone(),
        actual_realization_kind: None,
        receipt_fingerprint: request.fingerprint(),
    };
    let published =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &app,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "suspend-with-mcp".into(),
                lease: staged.lease.clone(),
                prepared_resource_revision: None,
                mcp_receipts: vec![receipt],
            },
        )
        .await
        .expect("D3 staged generation is publishable");
    let awaken_session_contract::SessionRealizationAction::Publish { publish, drain } =
        published.action
    else {
        panic!("D3 expected Publish")
    };
    assert_eq!(
        publish.as_slice(),
        std::slice::from_ref(&generation),
        "D3 exact Publish"
    );
    assert!(drain.is_empty(), "D3");
    let completed =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &app,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "suspend-with-mcp".into(),
                lease: staged.lease,
                published: vec![generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect("D3 publication acknowledgement");
    assert!(
        matches!(
            completed.action,
            awaken_session_contract::SessionRealizationAction::Complete
        ),
        "D3"
    );
    assert!(
        repo.get("suspend-with-mcp").await.unwrap().mcp.attachments[0].publication_acknowledged,
        "D3 reacknowledged"
    );
}

#[tokio::test]
async fn continuation_phases_gate_direct_and_recovery_realization_effects() {
    // Cause graph: an Active generation needs reprojection (C1), while the
    // Environment is Suspending, Hibernated, or Restoring (C2). Both a direct
    // phase command and the recovery scan (C3) must share one gate. Effect E1:
    // NotReady/no settled scan/no aggregate mutation and therefore no early
    // Stage or Publish. Resident-after-restore -> Stage is covered by D3 above.
    //
    // | Rule | Environment | Direct begin | Recovery scan | Effect |
    // |---|---|---|---|---|
    // | G1 | Suspending | NotReady | skip | E1 |
    // | G2 | Hibernated | NotReady | skip | E1 |
    // | G3 | Restoring | NotReady | skip | E1 |
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    for (rule, mut session) in [
        ("G1", generated_session("gate-suspending")),
        ("G2", hibernated_session("gate-hibernated", 100_000)),
        ("G3", hibernated_session("gate-restoring", 100_000)),
    ] {
        add_acknowledged_active_mcp(&mut session, "browser", 3);
        session.mcp.attachments[0].publication_acknowledged = false;
        let session_id = session.session_id.clone();
        let activity_epoch = session.activity_epoch;
        let realization = session.realization.clone();
        match rule {
            "G1" => {
                session
                    .environment
                    .begin_suspend("workspace", &session_id, activity_epoch, realization)
                    .unwrap();
            }
            "G3" => {
                session
                    .environment
                    .begin_restore(
                        "workspace",
                        &session_id,
                        activity_epoch + 1,
                        realization,
                        1_000,
                    )
                    .unwrap();
            }
            _ => {}
        }
        create(repo.as_ref(), session).await;
    }
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime,
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    for (rule, session_id) in [
        ("G1", "gate-suspending"),
        ("G2", "gate-hibernated"),
        ("G3", "gate-restoring"),
    ] {
        let before = repo.get(session_id).await.unwrap();
        assert!(
            matches!(
                awaken_session_contract::SessionRealizationControl::begin_session_realization(
                    &app,
                    awaken_session_contract::BeginSessionRealization {
                        session_id: session_id.into(),
                        target: target.clone(),
                    },
                )
                .await,
                Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
            ),
            "{rule}/E1 direct"
        );
        assert_eq!(repo.get(session_id).await.unwrap(), before, "{rule}/E1");
    }
    let scan = app.reconcile_session_realizations().await;
    assert!(scan.settled.is_empty(), "G1-G3/E1 scan");
    assert!(scan.failures.is_empty(), "G1-G3/E1 scan");
    for (rule, session_id) in [
        ("G1", "gate-suspending"),
        ("G2", "gate-hibernated"),
        ("G3", "gate-restoring"),
    ] {
        assert!(
            !repo.get(session_id).await.unwrap().mcp.attachments[0].publication_acknowledged,
            "{rule}/E1 no publication"
        );
    }
}

#[tokio::test]
async fn inexact_quiescence_receipts_leave_the_complete_root_unchanged() {
    // Cause graph: a committed Quiescing root owns two exact Active MCP
    // generations (C1); Runtime returns a partial, foreign, or duplicate set
    // (C2). Effect E1: receipt rejection leaves both Environment phase and MCP
    // publication truth byte-for-byte unchanged; no Uploading CAS is emitted.
    //
    // | Rule | Runtime generation set | Effect |
    // |---|---|---|
    // | X1 | partial | E1 |
    // | X2 | foreign | E1 |
    // | X3 | duplicate | E1 |
    for rule in ["X1", "X2", "X3"] {
        let session_id = format!("quiescence-{rule}");
        let mut session = generated_session(&session_id);
        add_acknowledged_active_mcp(&mut session, "alpha", 1);
        add_acknowledged_active_mcp(&mut session, "beta", 2);
        session
            .environment
            .begin_suspend(
                "workspace",
                &session_id,
                session.activity_epoch,
                session.realization.clone(),
            )
            .unwrap();
        let expected = session
            .mcp
            .active_generation_refs(&session_id)
            .expect("valid fixture Active set");
        let asserted = match rule {
            "X1" => expected[..1].to_vec(),
            "X2" => {
                let mut foreign = expected.clone();
                foreign[0].session_id = "foreign-session".into();
                foreign
            }
            "X3" => vec![expected[0].clone(), expected[0].clone()],
            _ => unreachable!(),
        };
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        create(repo.as_ref(), session).await;
        let before = repo.get(&session_id).await.unwrap();
        let runtime = Arc::new(ContinuationRuntime::default());
        *runtime.quiescence_mcp_override.lock().unwrap() = Some(asserted);
        let app = application_with_runtime(
            runtime,
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        assert!(
            app.reconcile_environment_continuation(&session_id, 2_000)
                .await
                .is_err(),
            "{rule}/E1"
        );
        assert_eq!(repo.get(&session_id).await.unwrap(), before, "{rule}/E1");
    }
}

#[tokio::test]
async fn checkpoint_release_artifact_authorization_is_exact_root_truth() {
    // Root authorization table: C1 nonterminal ReadyToDispose + exact operation
    // + current realization => canonical Workspace; C2 altered operation/lease
    // => StaleOwnership; C3 Resident/Uploading/Hibernated or terminal takeover
    // => NotReady/Terminal. The read grants no receipt or state transition.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut session = generated_session("checkpoint-artifact-auth");
    session.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker:boot".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX,
    });
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_prepare_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime,
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.reconcile_environment_continuation("checkpoint-artifact-auth", 2_000)
            .await
            .is_err(),
        "C1 fixture stops at ReadyToDispose"
    );
    let ready = repo.get("checkpoint-artifact-auth").await.unwrap();
    let awaken_session_contract::SessionEnvironmentState::Suspending { operation, .. } =
        &ready.environment
    else {
        panic!("C1 fixture is not suspending")
    };
    assert_eq!(
        awaken_session_contract::SessionRealizationControl::authorize_checkpoint_release_artifact_effect(
            &app,
            "checkpoint-artifact-auth",
            operation,
        )
        .await,
        Ok("workspace".into()),
        "C1"
    );
    let mut stale = operation.clone();
    stale.effect_id.push_str("-stale");
    assert_eq!(
        awaken_session_contract::SessionRealizationControl::authorize_checkpoint_release_artifact_effect(
            &app,
            "checkpoint-artifact-auth",
            &stale,
        )
        .await,
        Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership),
        "C2"
    );

    let resident_repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut resident = generated_session("checkpoint-artifact-not-ready");
    resident.realization = ready.realization.clone();
    create(resident_repo.as_ref(), resident).await;
    let resident_app = application_with_runtime(
        Arc::new(ContinuationRuntime::default()),
        resident_repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert_eq!(
        awaken_session_contract::SessionRealizationControl::authorize_checkpoint_release_artifact_effect(
            &resident_app,
            "checkpoint-artifact-not-ready",
            operation,
        )
        .await,
        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady),
        "C3"
    );
}

// Cause/effect design: C1=upload failure after Quiescing committed at T1;
// C2=retry supervisor observes T2>T1. FMECA crash window U1 => E3 source
// retained in Uploading; retry reuses the stable operation and its exact T1
// object metadata, then reaches E4 without repeating quiescence, creating a
// second logical checkpoint, or disposing early.
#[tokio::test]
async fn checkpoint_failure_retains_source_and_retries_from_committed_phase() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), generated_session("retry")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_checkpoint_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.reconcile_environment_continuation("retry", 2_000)
            .await
            .is_err()
    );
    let pending = repo.get("retry").await.unwrap();
    assert!(matches!(
        pending.environment,
        awaken_session_contract::SessionEnvironmentState::Suspending {
            suspend_phase: awaken_session_contract::SuspendPhase::Uploading,
            ..
        }
    ));
    assert_eq!(pending.environment.binding(), Some("source-binding"));
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 0);
    app.reconcile_environment_continuation("retry", 3_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(
        *runtime.checkpoint_scopes.lock().unwrap(),
        [("workspace".into(), 2_000), ("workspace".into(), 2_000)]
    );
}

// Cause/effect design: C3=quiescence effect fails before receipt; rule R6 =>
// E3 keep Quiescing and the live source. Retry repeats only quiescence, then
// proceeds through checkpoint/dispose once.
#[tokio::test]
async fn quiescence_failure_retains_resident_source_for_retry() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), generated_session("quiesce-retry")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.reconcile_environment_continuation("quiesce-retry", 2_000)
            .await
            .is_err()
    );
    assert!(matches!(
        repo.get("quiesce-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Suspending {
            suspend_phase: awaken_session_contract::SuspendPhase::Quiescing,
            ..
        }
    ));
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 0);
    app.reconcile_environment_continuation("quiesce-retry", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
}

// Source-release decision table: C1 prep fails after checkpoint => retain
// ReadyToDispose, disposal zero; C2 prep succeeds and its root CAS is durable
// but physical disposal fails => retain Disposing; C3 retry from Disposing =>
// never repeat checkpoint or live preparation, replay only exact disposal.
// These rules make total source absence meaningful only after the durable prep
// phase and close the response-loss window between provider delete and root CAS.
// C4 a higher-epoch realization takes over durable Disposing => retain A and
// its preparation fingerprint, use current B, and keep one physical operation.
#[tokio::test]
async fn disposal_failure_retries_without_recheckpointing() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), generated_session("dispose-retry")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_dispose_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.reconcile_environment_continuation("dispose-retry", 2_000)
            .await
            .is_err()
    );
    assert!(matches!(
        repo.get("dispose-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Suspending {
            suspend_phase: awaken_session_contract::SuspendPhase::Disposing,
            checkpoint: Some(_),
            ..
        }
    ));
    let mut failed_over = repo.get("dispose-retry").await.unwrap();
    failed_over.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: "continuation-worker-b".into(),
        runtime_incarnation: "continuation-runtime-b".into(),
        epoch: 5,
        expires_at_unix_ms: 100_000,
    });
    app.commit_session_snapshot(
        "workspace",
        failed_over,
        "test-continuation-disposal-failover",
        Vec::new(),
    )
    .await
    .unwrap();
    app.reconcile_environment_continuation("dispose-retry", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 2);
    let authorizations = runtime.disposal_authorizations.lock().unwrap();
    assert_eq!(authorizations.len(), 2, "C4 two delivery attempts");
    assert_eq!(
        authorizations[0].prepared_effect_fence(),
        authorizations[1].prepared_effect_fence(),
        "C4 immutable A",
    );
    assert_eq!(
        authorizations[0].preparation_fingerprint(),
        authorizations[1].preparation_fingerprint(),
        "C4 immutable preparation",
    );
    assert_eq!(
        authorizations[0].effect_fence().operation_id,
        authorizations[1].effect_fence().operation_id,
        "C4 one physical operation",
    );
    assert_eq!(authorizations[0].effect_fence().epoch, 4, "C4 A delivery");
    assert_eq!(authorizations[1].effect_fence().epoch, 5, "C4 B takeover");
}

#[tokio::test]
async fn preparation_response_loss_persists_the_exact_renewed_fence() {
    // Exact-preparation cause/effect table:
    // | Rule | suspend A0 | aggregate lease at prep | prep CAS/result | successor | Effect |
    // | P1 | epoch4 expiry100k | same generation expiry200k | commit then reported conflict | none | durable Disposing stores A=200k, not guessed A0 |
    // | P2 | P1 durable | unchanged | response-loss retry | epoch5 B | zero live re-prep; one physical id uses persisted A + current B |
    // Constraints: root admits the receipt only when its current generation
    // authorizes the embedded preparation lease; the Runtime never rebuilds A
    // from the mutable slot or the older suspend operation.
    let durable =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    create(repo.as_ref(), generated_session("prep-response-loss")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_checkpoint_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        app.reconcile_environment_continuation("prep-response-loss", 2_000)
            .await
            .is_err(),
        "P1 stop after the suspend operation freezes A0",
    );
    let mut renewed = repo.get("prep-response-loss").await.unwrap();
    renewed.realization.as_mut().unwrap().expires_at_unix_ms = 200_000;
    app.commit_session_snapshot(
        "workspace",
        renewed,
        "test-renew-before-source-preparation",
        Vec::new(),
    )
    .await
    .unwrap();
    repo.commit_then_conflict_once("environment-source-release-prepared");
    assert!(
        app.reconcile_environment_continuation("prep-response-loss", 2_000)
            .await
            .is_err(),
        "P1 aggregate commit succeeded but its response was lost",
    );

    let mut disposing = repo.get("prep-response-loss").await.unwrap();
    let awaken_session_contract::SessionEnvironmentState::Suspending {
        operation,
        suspend_phase: awaken_session_contract::SuspendPhase::Disposing,
        source_release_preparation: Some(prepared),
        ..
    } = &disposing.environment
    else {
        panic!("P1 durable preparation survives response loss")
    };
    assert_eq!(
        operation.realization.as_ref().unwrap().expires_at_unix_ms,
        100_000,
        "P1 immutable suspend A0",
    );
    assert_eq!(
        prepared.preparation().lease().expires_at_unix_ms,
        200_000,
        "P1 exact renewed preparation A",
    );
    disposing.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: "continuation-worker-b".into(),
        runtime_incarnation: "continuation-runtime-b".into(),
        epoch: 5,
        expires_at_unix_ms: 300_000,
    });
    app.commit_session_snapshot(
        "workspace",
        disposing,
        "test-prepared-source-disposal-failover",
        Vec::new(),
    )
    .await
    .unwrap();
    app.reconcile_environment_continuation("prep-response-loss", 2_000)
        .await
        .unwrap();

    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "P2");
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1, "P2");
    let authorizations = runtime.disposal_authorizations.lock().unwrap();
    assert_eq!(authorizations.len(), 1, "P2");
    assert_eq!(
        authorizations[0].prepared_effect_fence().expires_at_unix_ms,
        200_000,
        "P2 persisted A",
    );
    assert_eq!(authorizations[0].effect_fence().epoch, 5, "P2 current B");
}

#[tokio::test]
async fn preparation_failure_has_zero_physical_disposal_and_retries_before_disposing() {
    // C1 exact ReadyToDispose + preparation failure => E1 retain source and
    // checkpoint, E2 zero physical disposal; C2 retry => E3 one more prep,
    // durable Disposing, then one disposal and Hibernated. Checkpoint remains
    // single and no caller can collapse prep+delete into one authority edge.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), generated_session("prepare-retry")).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_prepare_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.reconcile_environment_continuation("prepare-retry", 2_000)
            .await
            .is_err(),
        "C1",
    );
    assert!(matches!(
        repo.get("prepare-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Suspending {
            suspend_phase: awaken_session_contract::SuspendPhase::ReadyToDispose,
            checkpoint: Some(_),
            ..
        }
    ));
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "E1");
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 0, "E2");

    app.reconcile_environment_continuation("prepare-retry", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1, "E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 2, "E3");
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1, "E3");
    assert!(matches!(
        repo.get("prepare-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ));
}

// Restore-to-projection cause/effect graph: C1 the durable Environment is
// Hibernated with a live exact realization lease; C2 restore returns the exact
// receipt; C3 the root CAS commits Resident; C4 the same driving boundary is
// replayed after Resident. Effects: E1 one stable physical restore; E2 only
// after C3, install the complete committed projection through Realization with
// prepare_session=true and the exact current lease; E3 C4 idempotently repeats
// that one projection/adoption path without another restore or Environment.
//
// | Rule | durable phase | restore | Resident CAS | driving replay | Effect |
// | R1 | Hibernated | succeeds | succeeds | no | E1 + E2 |
// | R2 | Resident | n/a | already committed | yes | E3 |
#[tokio::test]
async fn driving_ingress_restores_once_before_activity() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), hibernated_session("restore", 100_000)).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.ensure_environment_resident("restore", 1_000)
        .await
        .unwrap();
    let restored = repo.get("restore").await.unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(
        restored.environment.binding(),
        Some("restored-binding"),
        "R1/E2 durable Resident precedes projection success",
    );
    {
        let installs = runtime.projection_installs.lock().unwrap();
        assert_eq!(installs.len(), 1, "R1/E2");
        assert_eq!(installs[0].0, "restore", "R1/E2 exact Session");
        assert_eq!(installs[0].1.environment, restored.environment, "R1/E2");
        assert_eq!(installs[0].1.revision, restored.revision, "R1/E2");
        assert!(
            matches!(
                &installs[0].2,
                awaken_session_contract::SessionProjectionInstallMode::Realization {
                    lease,
                    prepare_session: true,
                } if Some(lease) == restored.realization.as_ref()
            ),
            "R1/E2 exact current lease and adoption mode",
        );
    }
    assert_eq!(
        runtime.adoptions.lock().unwrap().as_slice(),
        [("agent".into(), "restore".into(), "restored-binding".into())],
        "R1/E2 exact Resident binding is adopted before handoff succeeds",
    );
    app.ensure_environment_resident("restore", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        2,
        "R2/E3"
    );
    app.begin_activity("restore").await.unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        3,
        "R2/E3"
    );
}

// Failure decision table: C1 restore fails while the root is Restoring; C2
// restore succeeds and Resident commits; C3 the post-CAS adoption fails.
// Effects: E1 C1 retains Restoring and performs zero projection; E2 C2+C3
// retains durable Resident and performs no successful projection; E3 a later
// Resident ensure retries the same complete projection without restoring again.
//
// | Rule | restore | durable state after call | projection | Effect |
// | F1 | fails | Restoring | zero attempts | E1 |
// | F2 | succeeds | Resident | exact adoption fails once | E2 |
// | F3 | not repeated | Resident | succeeds on retry | E3 |
#[tokio::test]
async fn restore_failure_retries_the_committed_restore_operation() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), hibernated_session("restore-retry", 100_000)).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_restore_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.ensure_environment_resident("restore-retry", 1_000)
            .await
            .is_err()
    );
    assert!(matches!(
        repo.get("restore-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Restoring { .. }
    ));
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        0,
        "F1/E1 no projection before a Resident CAS",
    );
    app.ensure_environment_resident("restore-retry", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 2, "F1 retry");
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        1,
        "F1 retry"
    );
    assert_eq!(
        repo.get("restore-retry")
            .await
            .unwrap()
            .environment
            .binding(),
        Some("restored-binding")
    );
}

#[tokio::test]
async fn resident_projection_failure_retries_without_restoring_again() {
    // This is F2/F3 from the adjacent decision table. The failed Runtime call
    // occurs after the root is already Resident, making that durable phase the
    // sole retry fact; no projection receipt, slot flag, or resume queue exists.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        hibernated_session("projection-retry", 100_000),
    )
    .await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_adopt_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        app.ensure_environment_resident("projection-retry", 1_000)
            .await
            .is_err(),
        "F2/E2 projection failure is returned",
    );
    let resident = repo.get("projection-retry").await.unwrap();
    assert!(
        matches!(
            resident.environment,
            awaken_session_contract::SessionEnvironmentState::Resident { .. }
        ),
        "F2/E2 durable Resident is the retry fact",
    );
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "F2/E2");
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        1,
        "F2/E2"
    );
    assert_eq!(runtime.adoption_attempts.load(Ordering::SeqCst), 1, "F2/E2");
    assert!(
        runtime.projection_installs.lock().unwrap().is_empty(),
        "F2/E2"
    );

    app.ensure_environment_resident("projection-retry", 1_000)
        .await
        .expect("F3/E3 Resident projection retry");
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "F3/E3");
    assert_eq!(
        runtime.projection_attempts.load(Ordering::SeqCst),
        2,
        "F3/E3"
    );
    assert_eq!(runtime.adoption_attempts.load(Ordering::SeqCst), 2, "F3/E3");
    let installs = runtime.projection_installs.lock().unwrap();
    assert_eq!(installs.len(), 1, "F3/E3 one successful handoff");
    assert_eq!(installs[0].1.environment, resident.environment, "F3/E3");
    assert_eq!(
        runtime.adoptions.lock().unwrap().as_slice(),
        [(
            "agent".into(),
            "projection-retry".into(),
            "restored-binding".into(),
        )],
        "F3/E3 exact adoption succeeds once",
    );
}

#[tokio::test]
async fn external_checkpoint_retention_never_invokes_the_coordinator_runtime() {
    // Frozen-topology cause/effect graph: C1 placement is external Worker; C2
    // retention is CheckpointAndRelease; C3 a historical root is Resident or
    // Hibernated. C1+C2 means Coordinator has no claim-fenced continuation
    // effect transport. Effects: E1 Resident remains unchanged with zero local
    // quiesce/checkpoint; E2 Hibernated remains unchanged with zero local
    // restore/projection. Local placement is the only supported positive rule
    // and is covered by the adjacent restore/suspend tests.
    //
    // | Rule | placement | phase | Coordinator effect | durable phase |
    // | X1 | Worker | Resident | none | Resident |
    // | X2 | Worker | Hibernated | none | Hibernated |
    // | X3 | Local | either | canonical continuation | adjacent tests |
    let resident_repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        resident_repo.as_ref(),
        externally_placed(generated_session("external-resident")),
    )
    .await;
    let resident_runtime = Arc::new(ContinuationRuntime::default());
    let resident_app = application_with_runtime(
        resident_runtime.clone(),
        resident_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        resident_app
            .reconcile_environment_continuation("external-resident", 2_000)
            .await
            .is_err(),
        "X1 unsupported remote continuation fails closed",
    );
    assert!(matches!(
        resident_repo
            .get("external-resident")
            .await
            .unwrap()
            .environment,
        awaken_session_contract::SessionEnvironmentState::Resident { .. }
    ));
    assert_eq!(resident_runtime.quiesces.load(Ordering::SeqCst), 0, "X1/E1");
    assert_eq!(
        resident_runtime.checkpoints.load(Ordering::SeqCst),
        0,
        "X1/E1"
    );

    let hibernated_repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        hibernated_repo.as_ref(),
        externally_placed(hibernated_session("external-hibernated", 100_000)),
    )
    .await;
    let hibernated_runtime = Arc::new(ContinuationRuntime::default());
    let hibernated_app = application_with_runtime(
        hibernated_runtime.clone(),
        hibernated_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        hibernated_app
            .ensure_environment_resident("external-hibernated", 1_000)
            .await
            .is_err(),
        "X2 unsupported remote restore fails closed",
    );
    assert!(matches!(
        hibernated_repo
            .get("external-hibernated")
            .await
            .unwrap()
            .environment,
        awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ));
    assert_eq!(
        hibernated_runtime.restores.load(Ordering::SeqCst),
        0,
        "X2/E2"
    );
    assert_eq!(
        hibernated_runtime
            .projection_attempts
            .load(Ordering::SeqCst),
        0,
        "X2/E2",
    );
}

// Cause/effect design: C1=Suspending/Uploading with the exact current
// realization frozen into the operation; C8=driving message arrives. The
// message joins the committed operation: complete checkpoint+prepare+dispose,
// then E5 restore exactly once. A legacy/foreign preparation fence remains
// rejected by the adjacent continuation authority table; this fixture must not
// weaken that invariant merely because it starts directly in Uploading. No
// cancellation branch or second resume queue exists.
#[tokio::test]
async fn driving_ingress_during_upload_completes_suspend_then_restores() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut session = generated_session("during-upload");
    let realization = session.realization.clone();
    let operation = session
        .environment
        .begin_suspend_at("workspace", "during-upload", 0, realization, 2_000)
        .unwrap()
        .clone();
    let generation = session.environment.generation().unwrap().clone();
    session
        .environment
        .record_quiescence(
            &awaken_session_contract::QuiescenceReceipt {
                effect_id: operation.effect_id,
                generation_id: generation.id,
                activity_epoch: 0,
                live_environment_effects: 0,
                mcp_generations: Vec::new(),
            },
            &[],
        )
        .unwrap();
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.ensure_environment_resident("during-upload", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1);
}

// Cause/effect design: C7=expired Hibernated checkpoint and C8=driving event.
// Expiry rule => delete idempotently before dropping the reference, E7 fresh
// Unmaterialized generation, and never call restore.
#[tokio::test]
async fn expired_checkpoint_is_deleted_then_rebuilt_from_frozen_environment() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), hibernated_session("expired", 100)).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.ensure_environment_resident("expired", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    assert!(matches!(
        repo.get("expired").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
}

// Cause/effect design: C7=expired and checkpoint deletion fails. Expiry cleanup
// rule => E3 retain Hibernated reference (no fresh generation yet); retry
// replays delete, then and only then transitions to Unmaterialized.
#[tokio::test]
async fn expired_checkpoint_delete_failure_retains_reference_for_retry() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), hibernated_session("delete-retry", 100)).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_delete_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    assert!(
        app.ensure_environment_resident("delete-retry", 1_000)
            .await
            .is_err()
    );
    assert!(matches!(
        repo.get("delete-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ));
    app.ensure_environment_resident("delete-retry", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 2);
    assert!(matches!(
        repo.get("delete-retry").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
}

// Cause/effect design: C1=terminal command races a Hibernated Session whose
// frozen Environment projection contains a checkpoint; C2=the Runtime terminal
// preparation and disposal effects succeed. Terminal rule R7 => E8 install the
// frozen checkpoint projection for each closed action, execute one physical
// disposal, never call ordinary restore or the application expiry-delete side
// channel, and commit terminal cleanup through the two-stage protocol.
#[tokio::test]
async fn terminal_cleanup_delegates_checkpoint_to_the_one_runtime_effect() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(repo.as_ref(), hibernated_session("terminal", 100_000)).await;
    let runtime = Arc::new(ContinuationRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.terminate_session(
        "terminal",
        "2026-08-11T00:00:00Z",
        awaken_session_contract::ManagedLifecycleFact {
            id: "terminal-archive".into(),
            object_id: "terminal".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.archived".into(),
            timestamp: 1,
            runtime_interval: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        runtime
            .terminal_checkpoint_projections
            .load(Ordering::SeqCst),
        2,
        "Prepare and Dispose each install their exact aggregate snapshot"
    );
    assert_eq!(runtime.terminal_cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    assert!(
        repo.get("terminal")
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed()
    );
}
