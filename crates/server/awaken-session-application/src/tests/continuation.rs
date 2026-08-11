use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
}

#[async_trait::async_trait]
impl SessionRuntime for ContinuationRuntime {
    async fn quiesce_session_environment(
        &self,
        _thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
    ) -> Result<awaken_session_contract::QuiescenceReceipt, RunError> {
        self.quiesces.fetch_add(1, Ordering::SeqCst);
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
        generation: &awaken_session_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_contract::SourceDisposedReceipt, RunError> {
        self.disposals.fetch_add(1, Ordering::SeqCst);
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
        _agent: &str,
        _thread: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        generation: &awaken_session_contract::SandboxGeneration,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
    ) -> Result<awaken_session_contract::RestoreReceipt, RunError> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.fail_restore_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable("injected restore crash"));
        }
        Ok(awaken_session_contract::RestoreReceipt {
            effect_id: operation.effect_id.clone(),
            generation_id: generation.id.clone(),
            checkpoint_id: checkpoint.id.clone(),
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

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        unreachable!("continuation tests do not add system messages")
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
        unreachable!("continuation tests do not define outcomes")
    }

    fn model(&self) -> String {
        "continuation-test".into()
    }
}

fn generated_session(id: &str) -> PersistedSession {
    let mut session = persisted(id, false, false, "idle");
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

fn hibernated_session(id: &str, expires_at_unix_ms: u64) -> PersistedSession {
    let mut session = generated_session(id);
    let generation = session.environment.generation().unwrap().clone();
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        id,
        "suspend",
        &generation.id,
        0,
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
// C5=checkpoint success, C6=termination success, C8=no terminal race. Decision
// rule R3 => E2 through each phase then E4; replay in Hibernated emits no effect.
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
    assert!(matches!(
        repo.get("suspend").await.unwrap().environment,
        awaken_session_contract::SessionEnvironmentState::Hibernated { .. }
    ));
    app.reconcile_environment_continuation("suspend", 2_000)
        .await
        .unwrap();
    assert_eq!(runtime.quiesces.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.disposals.load(Ordering::SeqCst), 1);
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
        .begin_suspend("during-upload", 0, None)
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
