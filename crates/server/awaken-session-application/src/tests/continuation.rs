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
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.quiesces.fetch_add(1, Ordering::SeqCst);
        self.source_tuples.lock().unwrap().push((
            "quiesce",
            source_effect_id.into(),
            source_binding.into(),
            generation.id.clone(),
        ));
        if self.fail_quiesce_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected quiescence crash"));
        }
        Ok(awaken_session_contract::QuiescenceReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            activity_epoch: operation.activity_epoch,
            live_environment_effects: 0,
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
        .record_quiescence(&awaken_session_contract::QuiescenceReceipt {
            effect_id: operation.effect_id,
            generation_id: generation.id,
            activity_epoch: 0,
            live_environment_effects: 0,
        })
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
