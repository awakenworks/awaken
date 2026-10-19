// @formal evidence model_operation.reentry_requires_positive_execution_evidence
//! Whole-Run retry preserves the earlier committed tool and original uncertainty.
use super::*;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::model_operation::{
    ActiveModelOperation, ModelEffectScope, ModelExecutionEvidence, ModelOperationPhase,
};

#[tokio::test]
async fn response_only_eof_retries_the_current_model_without_replaying_committed_tools() {
    // Cause/effect table: C1 one ordinary tool effect and result already committed;
    // C2 next model request returns EOF, with parseable but unconfirmed tool JSON;
    // C3 original scope ResponseOnly/Opaque. R1 C1+C2+ResponseOnly => exactly three
    // model calls, one tool effect/result, NaturalEnd, and the middle journal
    // observation stays Returned/Unknown. R2 C1+C2+Opaque => two model calls,
    // one tool effect/result, Indeterminate. Both rows => no speculative tool,
    // no failed text in committed output or checkpoint. Real Runtime, ToolBatch,
    // ThreadCommit and Run identities are reused; no fake terminal provider fact.
    for (scope, calls, end) in [
        (ModelEffectScope::ResponseOnly, 3, EndCause::NaturalEnd),
        (ModelEffectScope::Opaque, 2, EndCause::Indeterminate),
    ] {
        let ran = Arc::new(AtomicUsize::new(0));
        let model = Arc::new(ToolThenText {
            calls: AtomicUsize::new(0),
            interrupted_response: Some(scope),
        });
        let durable = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let checkpoints = Arc::new(awaken_store_inmem::MemoryStreamCheckpointStore::new());
        let context = RuntimeRunContext::new()
            .with_commit(durable.clone())
            .with_reader(durable.clone())
            .with_stream_checkpoint(checkpoints.clone());
        let state = Runtime::new()
            .with_llm(model.clone())
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(ConstGate(GateOutcome::Allow)))
            .with_retry_policy(awaken_runtime::RetryPolicy {
                max_retries: 1,
                backoff_base_ms: 0,
                ..Default::default()
            })
            .execute(activation(), context)
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(end), "{scope:?}");
        assert_eq!(model.calls.load(Ordering::SeqCst), calls, "{scope:?}");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "no committed or speculative tool replay"
        );
        let committed = durable.committed();
        assert_eq!(
            committed
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count(),
            1
        );
        assert!(
            committed
                .messages
                .iter()
                .all(|m| !m.text_content().contains("unconfirmed EOF"))
        );
        let recorded = ActiveModelOperation::recorded(
            &durable.committed_state(&ThreadId("thread-1".into())),
            &RunId("run-1".into()),
        )
        .unwrap();
        assert_eq!(recorded.len(), calls);
        assert_eq!(recorded[1].ordinal.get(), 2);
        assert_eq!(recorded[1].phase, ModelOperationPhase::Returned);
        assert_eq!(recorded[1].effect_scope, scope);
        assert_eq!(
            recorded[1].execution,
            ModelExecutionEvidence::Unknown,
            "no invented provider termination"
        );
        use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
        assert_eq!(
            checkpoints.get("run-1").await,
            awaken_agent_contract::observation::Observed::Absent
        );
    }
}
