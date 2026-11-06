// @formal evidence model_operation.reentry_requires_positive_execution_evidence
//! Returned-response reentry exercises the existing inference and journal owners.
use super::*;

fn returned_eof_provider(
    scope: awaken_runtime_contract::model_operation::ModelEffectScope,
    failures: usize,
) -> Arc<FlakyStreamLlm> {
    Arc::new(FlakyStreamLlm {
        interruption: StreamInterruption::ReturnedEof { scope, failures },
        requests: Default::default(),
        first_partial: "unconfirmed response text",
        first_tool: Some(("unconfirmed-call", "must_not_execute", r#"{"effect":true}"#)),
        continuation: "confirmed retry response",
    })
}

#[tokio::test]
async fn returned_response_only_eof_retries_without_reusing_unconfirmed_output() {
    // Cause/effect rule E1: qualified ResponseOnly + actual returned EOF + one
    // retry left => a second model invocation, exact original request, and only
    // its complete response. E2 unconfirmed text and even parseable tool input
    // from the failed stream must not become a prefix, checkpoint or ToolUse.
    // No terminal provider receipt is fabricated for the failed invocation.
    let provider = returned_eof_provider(
        awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly,
        1,
    );
    let llm: Arc<dyn LlmExecutor> = provider.clone();
    let original = one_step_request();
    let response = infer_with_retry(
        &llm,
        original.clone(),
        &policy(1),
        &crate::circuit_breaker::CircuitBreaker::default(),
        &recording(),
        None,
        None,
        &awaken_runtime_contract::CaptureDecision::default(),
        None,
        &awaken_runtime_contract::metrics::NoopRecorder,
    )
    .await
    .expect("E1 returned response-only EOF retries the model call");
    assert_eq!(
        response.output.text_content(),
        "confirmed retry response",
        "E2"
    );
    assert!(
        response.output.tool_calls().is_empty(),
        "E2 no partial ToolUse"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "E1 exactly one bounded retry");
    assert_eq!(requests[0].messages, original.messages, "E1 original input");
    assert_eq!(
        requests[1].messages, original.messages,
        "E2 no unconfirmed prefix"
    );
}

#[tokio::test]
async fn returned_response_only_retry_retains_scope_and_budget_refusals() {
    use awaken_runtime_contract::model_operation::{ModelEffectScope, ModelExecutionEvidence};
    // Decision table: E3 opaque + returned EOF => one call/Unknown; E4 explicit
    // zero retry budget => one call/Unknown; E5 repeated ResponseOnly EOF with
    // budget two => exactly three calls/Unknown. Every row keeps the provider
    // result unknown and refuses synthetic tool output. The adjacent E1/E2
    // positive witness prevents an all-reject implementation from passing.
    for (rule, scope, retries, calls) in [
        ("E3", ModelEffectScope::Opaque, 2, 1),
        ("E4", ModelEffectScope::ResponseOnly, 0, 1),
        ("E5", ModelEffectScope::ResponseOnly, 2, 3),
    ] {
        let provider = returned_eof_provider(scope, usize::MAX);
        let llm: Arc<dyn LlmExecutor> = provider.clone();
        let error = infer_with_retry(
            &llm,
            one_step_request(),
            &policy(retries),
            &crate::circuit_breaker::CircuitBreaker::default(),
            &recording(),
            None,
            None,
            &awaken_runtime_contract::CaptureDecision::default(),
            None,
            &awaken_runtime_contract::metrics::NoopRecorder,
        )
        .await
        .expect_err("the fixture always returns EOF");
        assert_eq!(
            error.execution_evidence(),
            ModelExecutionEvidence::Unknown,
            "{rule}"
        );
        assert_eq!(provider.requests.lock().unwrap().len(), calls, "{rule}");
    }
}
