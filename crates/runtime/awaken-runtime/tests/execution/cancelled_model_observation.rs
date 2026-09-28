use super::*;
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, ModelOperationPhase};
use awaken_runtime_contract::runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, ModelCancellationObservation,
};

struct CurrentClaim;

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for CurrentClaim {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        Ok(())
    }
}

struct HeldReceipt {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    calls: AtomicUsize,
    confirmed: bool,
}

#[async_trait::async_trait]
impl LlmExecutor for HeldReceipt {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        if self.confirmed {
            TextLlm("must not publish after cancellation")
                .infer(request)
                .await
        } else {
            Err(awaken_runtime_contract::llm::Error::Provider(
                "no terminal envelope".into(),
            ))
        }
    }
}

#[tokio::test]
async fn cancelled_native_call_retains_its_exact_future_and_uses_only_returned_evidence() {
    // Cause/effect graph: C0 a physical Dispatch slot and committed journal
    // select retained observation; C1 the actual model entered and is blocked;
    // C2 cancel arrives; C3 the SAME call returns a terminal / Unknown error.
    // R1 C0+C1+C2 => no early terminal, one retained call; R2 R1+terminal => one
    // Returned(Terminated), logical Cancelled, physical ACK permitted; R3 R1+
    // Unknown => one Returned(Unknown), logical Cancelled, physical ACK vetoed.
    // EVERY rule => no retry, no assistant message/tool execution, same ordinal.
    // Notify plus semaphore establish the exact entry/return boundaries; no
    // sleeps, fabricated journal, replacement operation, or expiry assumption.
    for confirmed in [false, true] {
        let durable = Arc::new(MemoryCommitCoordinator::new(
            awaken_lease_contract::system_clock(),
        ));
        let model = Arc::new(HeldReceipt {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            calls: AtomicUsize::new(0),
            confirmed,
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone())
            .with_ownership(Arc::new(CurrentClaim))
            .with_model_cancellation_observation(
                ModelCancellationObservation::RetainClaimedUntilReceipt,
            )
            .with_cancellation(cancellation.clone());
        let physical = context.physical_quiescence.clone();
        let runtime = Runtime::new().with_llm(model.clone());
        let driving =
            tokio::spawn(async move { runtime.execute(activation("catalog-a"), context).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), model.entered.notified())
            .await
            .unwrap();
        cancellation.cancel();
        tokio::task::yield_now().await;
        assert!(
            !driving.is_finished(),
            "R1 the owned call remains observable"
        );
        model.release.add_permits(1);
        let state = tokio::time::timeout(std::time::Duration::from_secs(2), driving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1, "no replacement call");
        let operations = model_operations(&durable);
        assert_eq!(operations.len(), 2, "one entry and one receipt");
        assert_eq!(operations[0].ordinal, operations[1].ordinal);
        assert_eq!(operations[1].phase, ModelOperationPhase::Returned);
        assert_eq!(
            matches!(
                operations[1].execution,
                ModelExecutionEvidence::Terminated(_)
            ),
            confirmed
        );
        assert_eq!(
            physical.can_acknowledge_return(),
            confirmed,
            "only exact returned evidence permits ACK"
        );
        assert!(
            durable
                .committed_messages(&ThreadId("thread-1".into()))
                .iter()
                .all(|message| message.role != Role::Assistant),
            "canceled response cannot publish output"
        );
    }
}

#[tokio::test]
async fn attempt_without_a_claim_bound_journal_aborts_and_retains_unknown() {
    // C0 absent (direct/local attempt), C1 entered, C2 cancel, C3 no returned
    // envelope. L1 => prompt logical Cancelled, one Entered/Unknown, physical
    // ACK vetoed. Even a commit port AND local ownership verifier cannot stand
    // in for an admitted Dispatch slot. L2 the retained policy without a commit
    // port also aborts: policy alone cannot produce a claim-bound receipt.
    // A later release is never invented in either row.
    for (committed, observation) in [
        (true, ModelCancellationObservation::AbortAndFence),
        (
            false,
            ModelCancellationObservation::RetainClaimedUntilReceipt,
        ),
    ] {
        let durable = Arc::new(MemoryCommitCoordinator::new(
            awaken_lease_contract::system_clock(),
        ));
        let model = Arc::new(HeldReceipt {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            calls: AtomicUsize::new(0),
            confirmed: true,
        });
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut context = RuntimeRunContext::new()
            .with_reader(durable.clone())
            .with_ownership(Arc::new(CurrentClaim))
            .with_model_cancellation_observation(observation)
            .with_cancellation(cancellation.clone());
        if committed {
            context = context.with_commit(durable.clone());
        }
        let physical = context.physical_quiescence.clone();
        let runtime = Runtime::new().with_llm(model.clone());
        let driving =
            tokio::spawn(async move { runtime.execute(activation("catalog-a"), context).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), model.entered.notified())
            .await
            .unwrap();
        cancellation.cancel();
        let state = tokio::time::timeout(std::time::Duration::from_secs(2), driving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let operations = model_operations(&durable);
        assert_eq!(
            operations.len(),
            usize::from(committed),
            "L1/L2 no fabricated return"
        );
        if committed {
            assert_eq!(operations[0].phase, ModelOperationPhase::Entered);
            assert_eq!(operations[0].execution, ModelExecutionEvidence::Unknown);
        }
        assert!(
            !physical.can_acknowledge_return(),
            "L1/L2 no physical release"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn retained_cancelled_call_expires_at_its_original_deadline_without_reentry() {
    // R4 C0+C1+C2 (the admitted call is retained after cancellation) + C4 no
    // response before the ORIGINAL 1s deadline => [Entered,TimedOut(Unknown)],
    // logical Cancelled, physical ACK vetoed, zero retry/failover/output.
    // Cancel at 500ms; the call must still be pending at 999ms and end at 1s.
    // This distinguishes bounded retention from a fresh post-cancel timeout.
    // The existing HeldReceipt and Tokio clock own both exact boundaries.
    let durable = Arc::new(MemoryCommitCoordinator::new(
        awaken_lease_contract::system_clock(),
    ));
    let model = Arc::new(HeldReceipt {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        confirmed: true,
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = RuntimeRunContext::new()
        .with_commit(durable.clone())
        .with_reader(durable.clone())
        .with_ownership(Arc::new(CurrentClaim))
        .with_model_cancellation_observation(
            ModelCancellationObservation::RetainClaimedUntilReceipt,
        )
        .with_cancellation(cancellation.clone());
    let physical = context.physical_quiescence.clone();
    let runtime = Runtime::new()
        .with_llm(model.clone())
        .with_retry_policy(awaken_runtime::RetryPolicy {
            max_retries: 3,
            // Watchdog = 900ms response window + 100ms return allowance = 1s.
            budget: awaken_runtime_contract::llm::ModelInferenceBudget::new(
                std::time::Duration::from_millis(3000),
                std::time::Duration::from_millis(900),
                std::time::Duration::from_millis(900),
                std::time::Duration::from_millis(100),
            )
            .unwrap(),
            ..Default::default()
        })
        .with_max_consecutive_inference_failures(3);
    let mut request = activation("catalog-a");
    request
        .snapshot
        .resolved_spec
        .model_candidates
        .push(ResolvedModelCandidate::host(
            ModelBinding::parse("fallback", "fallback", "fallback").unwrap(),
        ));
    let driving = tokio::spawn(async move { runtime.execute(request, context).await });
    model.entered.notified().await;
    tokio::time::advance(std::time::Duration::from_millis(500)).await;
    cancellation.cancel();
    tokio::time::advance(std::time::Duration::from_millis(499)).await;
    assert!(
        !driving.is_finished(),
        "R4 retained until the original horizon"
    );
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(
        driving.is_finished(),
        "R4 cancellation must not reset the horizon"
    );
    assert_eq!(
        driving.await.unwrap().unwrap(),
        RunState::Ended(EndCause::Cancelled)
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "R4 no replacement call"
    );
    let operations = model_operations(&durable);
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0].phase, ModelOperationPhase::Entered);
    assert_eq!(operations[1].phase, ModelOperationPhase::TimedOut);
    assert_eq!(operations[0].ordinal, operations[1].ordinal);
    assert_eq!(operations[1].execution, ModelExecutionEvidence::Unknown);
    assert!(
        !physical.can_acknowledge_return(),
        "R4 no invented terminal proof"
    );
    assert!(
        durable
            .committed_messages(&ThreadId("thread-1".into()))
            .iter()
            .all(|message| message.role != Role::Assistant),
        "R4 no output from an abandoned call"
    );
}
