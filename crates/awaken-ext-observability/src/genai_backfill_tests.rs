//! Tests for GenAI semantic-convention field backfill in AfterInferenceHook.
//!
//! Verifies that `finish_reasons` is populated from the upstream
//! `stop_reason` field. `response_model` and `response_id` are not available
//! on `StreamResult` and remain `None`; this is asserted explicitly.
//!
//! Note: `response_model` and `response_id` are absent because the
//! `StreamResult` contract type does not carry the provider-returned model
//! name or response ID. Those fields would require changes to the upstream
//! contract type to propagate from the genai crate's `StreamEnd`.

use std::sync::Arc;

use awaken_runtime::{PhaseContext, PhaseHook};
use awaken_runtime_contract::contract::content::ContentBlock;
use awaken_runtime_contract::contract::inference::{
    InferenceError, LLMResponse, StopReason, StreamResult, TokenUsage,
};
use awaken_runtime_contract::model::Phase;
use awaken_runtime_contract::state::{Snapshot, StateMap};

use crate::InMemorySink;
use crate::plugin::{AfterInferenceHook, BeforeInferenceHook, ObservabilityPlugin};

fn empty_snapshot() -> Snapshot {
    Snapshot::new(0, Arc::new(StateMap::default()))
}

fn make_response(stop_reason: StopReason) -> LLMResponse {
    LLMResponse::success(StreamResult {
        content: vec![ContentBlock::text("hello")],
        tool_calls: vec![],
        usage: Some(TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        }),
        stop_reason: Some(stop_reason),
        has_incomplete_tool_calls: false,
    })
}

async fn run_inference_cycle(plugin: &ObservabilityPlugin, response: LLMResponse) {
    let inner = Arc::clone(&plugin.inner);

    BeforeInferenceHook(Arc::clone(&inner))
        .run(&PhaseContext::new(Phase::BeforeInference, empty_snapshot()))
        .await
        .unwrap();

    AfterInferenceHook(inner)
        .run(
            &PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(response),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn finish_reasons_backfilled_from_end_turn_stop_reason() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    run_inference_cycle(&plugin, make_response(StopReason::EndTurn)).await;

    let metrics = sink.metrics();
    assert_eq!(metrics.inferences.len(), 1);
    assert_eq!(
        metrics.inferences[0].finish_reasons,
        vec!["end_turn".to_string()],
        "finish_reasons must be backfilled from stop_reason EndTurn"
    );
}

#[tokio::test]
async fn finish_reasons_backfilled_from_max_tokens_stop_reason() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    run_inference_cycle(&plugin, make_response(StopReason::MaxTokens)).await;

    let metrics = sink.metrics();
    assert_eq!(
        metrics.inferences[0].finish_reasons,
        vec!["max_tokens".to_string()]
    );
}

#[tokio::test]
async fn finish_reasons_backfilled_from_tool_use_stop_reason() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    run_inference_cycle(&plugin, make_response(StopReason::ToolUse)).await;

    let metrics = sink.metrics();
    assert_eq!(
        metrics.inferences[0].finish_reasons,
        vec!["tool_use".to_string()]
    );
}

#[tokio::test]
async fn finish_reasons_backfilled_from_stop_sequence_stop_reason() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    run_inference_cycle(&plugin, make_response(StopReason::StopSequence)).await;

    let metrics = sink.metrics();
    assert_eq!(
        metrics.inferences[0].finish_reasons,
        vec!["stop_sequence".to_string()]
    );
}

#[tokio::test]
async fn finish_reasons_empty_when_no_stop_reason() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    let response = LLMResponse::success(StreamResult {
        content: vec![ContentBlock::text("hello")],
        tool_calls: vec![],
        usage: None,
        stop_reason: None,
        has_incomplete_tool_calls: false,
    });

    run_inference_cycle(&plugin, response).await;

    let metrics = sink.metrics();
    assert!(
        metrics.inferences[0].finish_reasons.is_empty(),
        "finish_reasons must be empty when stop_reason is None"
    );
}

#[tokio::test]
async fn finish_reasons_empty_on_inference_error() {
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    let response = LLMResponse::error(InferenceError {
        error_type: "timeout".into(),
        message: "request timed out".into(),
        error_class: Some("connection".into()),
    });

    run_inference_cycle(&plugin, response).await;

    let metrics = sink.metrics();
    assert!(
        metrics.inferences[0].finish_reasons.is_empty(),
        "finish_reasons must be empty on error outcome"
    );
}

#[tokio::test]
async fn response_model_and_response_id_not_available_on_stream_result() {
    // `response_model` and `response_id` cannot be populated because the
    // upstream `StreamResult` contract type does not carry the provider-returned
    // model name or response ID. Both remain None until the contract is extended.
    let sink = InMemorySink::new();
    let plugin = ObservabilityPlugin::new(sink.clone())
        .with_model("claude-opus-4-7")
        .with_provider("anthropic");

    run_inference_cycle(&plugin, make_response(StopReason::EndTurn)).await;

    let metrics = sink.metrics();
    assert!(
        metrics.inferences[0].response_model.is_none(),
        "response_model not available on StreamResult"
    );
    assert!(
        metrics.inferences[0].response_id.is_none(),
        "response_id not available on StreamResult"
    );
}
