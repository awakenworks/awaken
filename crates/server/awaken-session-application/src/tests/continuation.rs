use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct RestoreSubstrate {
    exact_request: Mutex<Option<awaken_session_contract::SandboxRestoreRequest>>,
    creates: AtomicUsize,
}

#[derive(Default)]
struct ContinuationRuntime {
    fail_quiesce_once: AtomicBool,
    fail_checkpoint_once: AtomicBool,
    fail_dispose_once: AtomicBool,
    fail_restore_once: AtomicBool,
    fail_delete_once: AtomicBool,
    quiesces: AtomicUsize,
    checkpoints: AtomicUsize,
    disposals: AtomicUsize,
    restores: AtomicUsize,
    deletes: AtomicUsize,
    checkpoint_scopes: Mutex<Vec<(String, u64)>>,
    expected_mcp_generations: Mutex<Vec<Vec<awaken_session_contract::McpGenerationRef>>>,
    quiescence_mcp_override: Mutex<Option<Vec<awaken_session_contract::McpGenerationRef>>>,
    restore_requests: Mutex<Vec<awaken_session_contract::SandboxRestoreRequest>>,
    terminal_restore_targets: Mutex<Vec<awaken_session_contract::SandboxRestoreRequest>>,
    terminal_restore_disposals: AtomicUsize,
    terminal_effect_order: Mutex<Vec<&'static str>>,
    restore_substrate: Arc<RestoreSubstrate>,
    source_tuples: Mutex<Vec<(&'static str, String, String, String)>>,
}

impl ContinuationRuntime {
    fn with_restore_substrate(restore_substrate: Arc<RestoreSubstrate>) -> Self {
        Self {
            restore_substrate,
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ContinuationRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        if let Some(request) = command.restore_target.as_ref() {
            let mut exact = self.restore_substrate.exact_request.lock().unwrap();
            match exact.as_ref() {
                Some(observed) if observed != request => {
                    return Err(RunError::unavailable(
                        "terminal cleanup selected another physical restore target",
                    ));
                }
                Some(_) => *exact = None,
                None => {}
            }
            drop(exact);
            self.terminal_restore_targets
                .lock()
                .unwrap()
                .push(request.clone());
            self.terminal_restore_disposals
                .fetch_add(1, Ordering::SeqCst);
            self.terminal_effect_order
                .lock()
                .unwrap()
                .push("dispose-restored-target");
        }
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
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

    async fn dispose_checkpoint_source(
        &self,
        _thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.disposals.fetch_add(1, Ordering::SeqCst);
        self.source_tuples.lock().unwrap().push((
            "dispose",
            source_effect_id.into(),
            source_binding.into(),
            generation.id.clone(),
        ));
        if self.fail_dispose_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected disposal crash"));
        }
        Ok(awaken_session_contract::SourceDisposedReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            source_binding: source_binding.into(),
            terminated: true,
        })
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        self.restore_requests.lock().unwrap().push(request.clone());
        let mut exact = self.restore_substrate.exact_request.lock().unwrap();
        match exact.as_ref() {
            Some(observed) if observed != &request => {
                return Err(RunError::unavailable(
                    "physical restore target belongs to another exact request",
                ));
            }
            Some(_) => {}
            None => {
                self.restore_substrate
                    .creates
                    .fetch_add(1, Ordering::SeqCst);
                *exact = Some(request.clone());
            }
        }
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
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("delete-checkpoint");
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

fn generated_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, "idle");
    let baseline = match &mut session.baseline {
        awaken_session_contract::SessionBaselineState::Frozen(baseline) => baseline,
        _ => unreachable!(),
    };
    baseline.environment.idle_retention = awaken_session_contract::EnvironmentIdleRetentionPolicy {
        mode: awaken_session_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 1,
        retention_secs: 100,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 1024,
        max_checkpoint_duration_secs: 5,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    };
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
    session
}

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
            .unwrap(),
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

pub(super) fn hibernated_session(id: &str, expires_at_unix_ms: u64) -> PersistedSession {
    let mut session = generated_session(id);
    let generation = session.environment.generation().unwrap().clone();
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
// the byte adapter receives exact Workspace + creation time, and replay in
// Hibernated emits no effect.
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
                generation_id.clone(),
            ),
            (
                "dispose",
                "create".into(),
                "source-binding".into(),
                generation_id,
            ),
        ],
        "R19/R20 exact physical source tuple survives every suspend phase"
    );
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
        renew_existing_lease: false,
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
        renew_existing_lease: false,
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

// Cause/effect design: C5=upload failure after Quiescing committed. FMECA crash
// window U1 => E3 source retained in Uploading; retry reuses the stable operation
// and reaches E4 without repeating quiescence or disposing early.
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
    app.reconcile_environment_continuation("retry", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
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

// Cause/effect design: C6=dispose fails after checkpoint receipt committed.
// Crash window D1 => E3 keep ReadyToDispose plus the exact checkpoint; retry
// repeats only idempotent disposal and cannot upload a parallel object.
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
            suspend_phase: awaken_session_contract::SuspendPhase::ReadyToDispose,
            checkpoint: Some(_),
            ..
        }
    ));
    app.reconcile_environment_continuation("dispose-retry", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 2);
}

// Cause/effect design: C1=Hibernated, C7=valid, C8=two driving joins. Restore
// rule => E5 one stable restore effect and one Resident binding; subsequent
// admission observes Resident and cannot create a parallel environment.
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
    app.ensure_environment_resident("restore", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1);
    assert_eq!(
        repo.get("restore").await.unwrap().environment.binding(),
        Some("restored-binding")
    );
    app.begin_activity("restore").await.unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1);
}

// Cause/effect design: C7=valid checkpoint and restore effect fails after the
// Restoring intent commits. Restore crash rule => E3 retain Restoring; retry
// joins the stable operation and publishes one Resident binding.
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
    app.ensure_environment_resident("restore-retry", 1_000)
        .await
        .unwrap();
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 2);
    assert_eq!(
        repo.get("restore-retry")
            .await
            .unwrap()
            .environment
            .binding(),
        Some("restored-binding")
    );
}

/*
 * Restore publication cause/effect decision table (R5, R8).
 * Causes: C1 provider completes the exact physical target; C2 root CAS fails;
 * C3 the first Host/runtime object is lost; C4 a fresh Host retries from the
 * durable Restoring aggregate. Effects: E1 retain Restoring and the physical
 * target; E2 publish no Runtime-local adoption before CAS; E3 pass the identical
 * canonical request to the fresh runtime; E4 leave post-CAS process projection
 * to the existing realization owner; E5 create one physical target and commit
 * its same binding.
 * Rules: R5=C1+C2=>E1+E2; R8=C3+C4=>E3+E4+E5.
 */
#[tokio::test]
async fn restore_receipt_cas_gap_is_recovered_by_a_fresh_runtime_without_target_two() {
    let durable =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        durable.as_ref(),
        hibernated_session("restore-cas-gap", 100_000),
    )
    .await;
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    faulting.fail_once("environment-restored");
    let substrate = Arc::new(RestoreSubstrate::default());
    let first_runtime = Arc::new(ContinuationRuntime::with_restore_substrate(
        substrate.clone(),
    ));
    let first = application_with_runtime(
        first_runtime.clone(),
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        first
            .ensure_environment_resident("restore-cas-gap", 1_000)
            .await
            .is_err(),
        "R5 injected root-CAS gap"
    );
    assert!(matches!(
        durable.get("restore-cas-gap").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Restoring { .. }
    ));
    assert_eq!(substrate.creates.load(Ordering::SeqCst), 1, "R5/E1");
    let first_request = first_runtime.restore_requests.lock().unwrap()[0].clone();
    drop(first);
    drop(first_runtime);

    let fresh_runtime = Arc::new(ContinuationRuntime::with_restore_substrate(
        substrate.clone(),
    ));
    let fresh = application_with_runtime(
        fresh_runtime.clone(),
        durable.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    fresh
        .ensure_environment_resident("restore-cas-gap", 1_000)
        .await
        .unwrap();

    assert_eq!(
        fresh_runtime.restore_requests.lock().unwrap().as_slice(),
        [first_request],
        "R8/E3"
    );
    assert_eq!(substrate.creates.load(Ordering::SeqCst), 1, "R8/E5");
    assert_eq!(
        durable
            .get("restore-cas-gap")
            .await
            .unwrap()
            .environment
            .binding(),
        Some("restored-binding"),
        "R8/E5"
    );
}

/*
 * Concurrent owner rule (R9): two driving callers can race the Hibernated ->
 * Restoring and Restoring -> Resident CAS edges, but both carry one aggregate
 * tuple. The effects are one physical target, one Resident binding, and no
 * alternate restore request.
 */
#[tokio::test]
async fn concurrent_restore_drivers_converge_on_one_exact_target_and_binding() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        hibernated_session("restore-concurrent", 100_000),
    )
    .await;
    let substrate = Arc::new(RestoreSubstrate::default());
    let left_runtime = Arc::new(ContinuationRuntime::with_restore_substrate(
        substrate.clone(),
    ));
    let right_runtime = Arc::new(ContinuationRuntime::with_restore_substrate(
        substrate.clone(),
    ));
    let left = application_with_runtime(
        left_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let right = application_with_runtime(
        right_runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let (left_result, right_result) = tokio::join!(
        left.ensure_environment_resident("restore-concurrent", 1_000),
        right.ensure_environment_resident("restore-concurrent", 1_000),
    );
    left_result.expect("R9 left converges");
    right_result.expect("R9 right converges");
    assert_eq!(substrate.creates.load(Ordering::SeqCst), 1, "R9");
    let mut requests = left_runtime.restore_requests.lock().unwrap().clone();
    requests.extend(right_runtime.restore_requests.lock().unwrap().clone());
    assert!(!requests.is_empty(), "R9 exercised restore");
    assert!(requests.iter().all(|request| request == &requests[0]), "R9");
    assert_eq!(
        repo.get("restore-concurrent")
            .await
            .unwrap()
            .environment
            .binding(),
        Some("restored-binding"),
        "R9"
    );
}

// Cause/effect design: C1=Suspending/Uploading and C8=driving message arrives.
// The message joins the committed operation: complete checkpoint+dispose, then
// E5 restore exactly once. No cancellation branch or second resume queue exists.
#[tokio::test]
async fn driving_ingress_during_upload_completes_suspend_then_restores() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut session = generated_session("during-upload");
    let operation = session
        .environment
        .begin_suspend("workspace", "during-upload", 0, None)
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

// Cause/effect design: C8=terminal command races a Hibernated Session. Terminal
// rule R7 => E8 delete checkpoint through the existing cleanup saga, never call
// restore, and commit terminal cleanup only after deletion succeeds.
#[tokio::test]
async fn terminal_cleanup_deletes_checkpoint_without_restore() {
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
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    assert!(
        repo.get("terminal")
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed()
    );
}

/*
 * Terminal restore-orphan cause/effect table (R10).
 * Causes: C1 restore acquired the exact physical target; C2 its completion
 * failed before the root Restoring -> Resident CAS; C3 terminal cleanup wins.
 * Effects: E1 project the unchanged Workspace/Session/effect/generation/
 * checkpoint tuple only on the root cleanup command; E2 dispose that exact
 * target before deleting checkpoint bytes; E3 never retry restore; E4 commit
 * terminal cleanup only after both effects succeed.
 * Rule R10=C1+C2+C3=>E1+E2+E3+E4.
 */
#[tokio::test]
async fn terminal_restoring_cleanup_disposes_exact_target_before_checkpoint() {
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        hibernated_session("terminal-restoring", 100_000),
    )
    .await;
    let runtime = Arc::new(ContinuationRuntime::default());
    runtime.fail_restore_once.store(true, Ordering::SeqCst);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    assert!(
        app.ensure_environment_resident("terminal-restoring", 1_000)
            .await
            .is_err(),
        "R10/C1+C2"
    );
    let pending = repo.get("terminal-restoring").await.unwrap();
    let exact = pending
        .environment
        .restoring_request("workspace", "terminal-restoring")
        .expect("R10/E1 durable Restoring tuple");
    assert_eq!(
        runtime
            .restore_substrate
            .exact_request
            .lock()
            .unwrap()
            .as_ref(),
        Some(&exact),
        "R10/C1"
    );

    app.terminate_session(
        "terminal-restoring",
        "2026-08-11T00:00:00Z",
        awaken_session_contract::ManagedLifecycleFact {
            id: "terminal-restoring-archive".into(),
            object_id: "terminal-restoring".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.archived".into(),
            timestamp: 1,
            runtime_interval: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.terminal_restore_targets.lock().unwrap().as_slice(),
        [exact],
        "R10/E1"
    );
    assert_eq!(
        runtime.terminal_restore_disposals.load(Ordering::SeqCst),
        1,
        "R10/E2"
    );
    assert_eq!(
        runtime.terminal_effect_order.lock().unwrap().as_slice(),
        ["dispose-restored-target", "delete-checkpoint"],
        "R10/E2"
    );
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "R10/E3");
    assert!(
        runtime
            .restore_substrate
            .exact_request
            .lock()
            .unwrap()
            .is_none(),
        "R10/E2"
    );
    assert!(
        repo.get("terminal-restoring")
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed(),
        "R10/E4"
    );
}
