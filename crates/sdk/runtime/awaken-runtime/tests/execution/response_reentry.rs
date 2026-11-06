// @formal evidence model_operation.reentry_requires_positive_execution_evidence
//! Durable reentry must use the original scope, phase, commit ACK and checkpoint.
use super::*;
use awaken_runtime_contract::model_operation::{
    ModelEffectScope, ModelExecutionEvidence, ModelOperation, ModelOperationPhase,
};

#[tokio::test]
async fn response_only_returned_eof_does_not_override_failed_journal_acknowledgement() {
    // Causes: C1 qualified Returned/Unknown; C2 return commit rejected before
    // writing / written with lost ACK; C3 one retry allowed. J1/J2 => one model
    // invocation, commit error, no next invocation or fabricated terminal Run.
    // The original durable prefix distinguishes Entered from Returned; neither
    // case may use an unacknowledged local draft as permission to retry.
    for lose_ack in [false, true] {
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let model = Arc::new(ReturnedUnknownModel {
            scope: ModelEffectScope::ResponseOnly,
            calls: AtomicUsize::new(0),
            timeout: false,
        });
        let fault = Arc::new(ModelEvidenceFault {
            durable: durable.clone(),
            phase: ModelOperationPhase::Returned,
            lose_ack,
            pause: None,
        });
        let result = Runtime::new()
            .with_llm(model.clone())
            .with_retry_policy(awaken_runtime::RetryPolicy {
                max_retries: 1,
                backoff_base_ms: 0,
                ..Default::default()
            })
            .execute(
                activation("catalog-a"),
                RuntimeRunContext::new()
                    .with_commit(fault)
                    .with_reader(durable.clone()),
            )
            .await;
        assert!(result.is_err(), "J1/J2 commit failure is not model success");
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let operations = model_operations(&durable);
        assert_eq!(operations.len(), if lose_ack { 2 } else { 1 });
        let last = operations.last().unwrap();
        assert_eq!(
            last.phase,
            if lose_ack {
                ModelOperationPhase::Returned
            } else {
                ModelOperationPhase::Entered
            }
        );
        assert_eq!(last.execution, ModelExecutionEvidence::Unknown);
    }
}

#[tokio::test]
async fn response_only_history_reentry_requires_return_and_never_salvages_unknown_checkpoint() {
    // Causes: C1 original scope Opaque/ResponseOnly; C2 original local phase
    // Entered/Returned/TimedOut; C3 checkpoint absent/present with complete tool
    // JSON. H1 Unknown+ResponseOnly+Returned+absent => one NEW model request;
    // H2 every other combination => Indeterminate, no model/tool effect.
    // All 12 rows preserve original Unknown and use the same ThreadCommit log.
    // In particular, a new deployment cannot upgrade old Opaque evidence, and
    // a parseable tool fragment never grants checkpoint reuse.
    for scope in [ModelEffectScope::Opaque, ModelEffectScope::ResponseOnly] {
        for phase in [
            ModelOperationPhase::Entered,
            ModelOperationPhase::Returned,
            ModelOperationPhase::TimedOut,
        ] {
            for checkpoint_present in [false, true] {
                let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
                let mut original =
                    ModelOperation::next(None, ModelBinding::parse("p", "m", "b").unwrap())
                        .unwrap();
                original.effect_scope = scope;
                original.phase = phase;
                commit_model_operations(&durable, [original]).await;
                let (checkpoints, mut checkpoint) = interrupted_model_checkpoint().await;
                if checkpoint_present {
                    checkpoint.partial_tools.push(
                        awaken_agent_contract::stream::checkpoint::PartialToolCall {
                            call_id: "unconfirmed".into(),
                            tool_id: "must-not-execute".into(),
                            raw_arguments: "{}".into(),
                        },
                    );
                    checkpoints.put(checkpoint.clone()).await.unwrap();
                } else {
                    checkpoints.delete(&checkpoint.run_id).await.unwrap();
                }
                let allowed = scope == ModelEffectScope::ResponseOnly
                    && phase == ModelOperationPhase::Returned
                    && !checkpoint_present;
                let result = Runtime::new()
                    .with_llm(Arc::new(TextLlm("fresh response")))
                    .execute(
                        activation("catalog-a"),
                        RuntimeRunContext::new()
                            .with_commit(durable.clone())
                            .with_reader(durable.clone())
                            .with_stream_checkpoint(checkpoints.clone()),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    result,
                    RunState::Ended(if allowed {
                        EndCause::NaturalEnd
                    } else {
                        EndCause::Indeterminate
                    }),
                    "{scope:?}/{phase:?}/checkpoint={checkpoint_present}"
                );
                let operations = model_operations(&durable);
                assert_eq!(
                    operations.len(),
                    if allowed { 3 } else { 1 },
                    "only H1 enters a new model"
                );
                assert_eq!(operations[0].execution, ModelExecutionEvidence::Unknown);
                if checkpoint_present {
                    assert_eq!(
                        checkpoints.get(&checkpoint.run_id).await,
                        Observed::Present(checkpoint)
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn response_only_returned_eof_cancelled_during_return_commit_never_retries() {
    // C1 qualified response-only EOF has returned; C2 its original Returned
    // commit is held; C3 cancellation arrives before that ACK. C1+C2+C3 => drain
    // the exact owned commit, retain Returned/Unknown, logical Cancelled, one
    // model call, no retry or uncertain partial-tool publication. The existing
    // journal pause controls a precise commit boundary, not elapsed-time success.
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let model = Arc::new(ReturnedUnknownModel {
        scope: ModelEffectScope::ResponseOnly,
        calls: AtomicUsize::new(0),
        timeout: false,
    });
    let pause = Arc::new(ModelEvidencePause {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        entries: AtomicUsize::new(0),
    });
    let fault = Arc::new(ModelEvidenceFault {
        durable: durable.clone(),
        phase: ModelOperationPhase::Returned,
        lose_ack: false,
        pause: Some(pause.clone()),
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let context = RuntimeRunContext::new()
        .with_commit(fault)
        .with_reader(durable.clone())
        .with_cancellation(cancellation.clone());
    let runtime =
        Runtime::new()
            .with_llm(model.clone())
            .with_retry_policy(awaken_runtime::RetryPolicy {
                max_retries: 3,
                backoff_base_ms: 0,
                ..Default::default()
            });
    let driving =
        tokio::spawn(async move { runtime.execute(activation("catalog-a"), context).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), pause.entered.notified())
        .await
        .unwrap();
    cancellation.cancel();
    pause.release.add_permits(1);
    let state = tokio::time::timeout(std::time::Duration::from_secs(2), driving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let operations = model_operations(&durable);
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[1].phase, ModelOperationPhase::Returned);
    assert_eq!(operations[1].execution, ModelExecutionEvidence::Unknown);
    assert_eq!(operations[0].ordinal, operations[1].ordinal);
}

#[tokio::test]
async fn response_only_eof_exhaustion_keeps_a_specific_inference_failure_and_original_evidence() {
    // Cause/effect E6: ResponseOnly, actual returned EOF, two retries, no fallback,
    // one tolerated failure => three calls and ordinary Inference/provider_error
    // with the EOF cause. Original provider outcome stays Unknown for all calls;
    // it must not replace the known model-request failure with Indeterminate.
    // The canonical audit owns is_error+retry_count; no additional receipt store.
    let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let model = Arc::new(ReturnedUnknownModel {
        scope: ModelEffectScope::ResponseOnly,
        calls: AtomicUsize::new(0),
        timeout: false,
    });
    let state = Runtime::new()
        .with_llm(model.clone())
        .with_retry_policy(awaken_runtime::RetryPolicy {
            max_retries: 2,
            backoff_base_ms: 0,
            ..Default::default()
        })
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new()
                .with_commit(durable.clone())
                .with_reader(durable.clone()),
        )
        .await
        .unwrap();
    let RunState::Ended(EndCause::Error(awaken_agent_contract::agent::run::Failure::Inference {
        code,
        message,
    })) = state
    else {
        panic!("E6 a known exhausted request is an inference failure: {state:?}")
    };
    assert_eq!(code, "provider_error");
    assert!(
        message.contains("EOF before protocol end"),
        "E6 concrete cause survives"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    let operations = model_operations(&durable);
    assert_eq!(operations.len(), 6);
    assert!(
        operations
            .iter()
            .all(|o| o.execution == ModelExecutionEvidence::Unknown)
    );
    let requests = committed_model_requests(&durable);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].is_error);
    assert_eq!(requests[0].retry_count, 2);
}
