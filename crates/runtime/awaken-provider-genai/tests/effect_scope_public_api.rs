//! Downstream compatibility of the public effect-scope type after internal moves.
use awaken_provider_genai::{GenaiExecutor, OpenAiResponsesExecutor, provider_model_effect_scope};
use awaken_runtime_contract::llm::{ChatRequest, LlmExecutor};
use awaken_runtime_contract::model_operation::ModelEffectScope;

#[test]
fn provider_effect_scope_keeps_the_public_runtime_contract_type() {
    // These assignments are compile-time contracts for downstream callers. The
    // snapshot may render the defining module, but callers use the public export.
    let _: fn(&GenaiExecutor, &ChatRequest) -> ModelEffectScope =
        <GenaiExecutor as LlmExecutor>::effect_scope;
    let _: fn(&OpenAiResponsesExecutor, &ChatRequest) -> ModelEffectScope =
        <OpenAiResponsesExecutor as LlmExecutor>::effect_scope;
    let _: fn(&str, &str, &str, &ChatRequest) -> ModelEffectScope = provider_model_effect_scope;
}
