//! Scope qualification of the brokered executor.
use super::*;

#[test]
fn brokered_executor_uses_the_existing_provider_effect_scope_policy() {
    use awaken_runtime_contract::model_operation::ModelEffectScope;
    use awaken_runtime_contract::resolved::{ProviderServerTool, ToolDescriptor};
    // B1 qualified DeepSeek wire without hosted tools => response-only;
    // B2 hosted execution or unqualified provider => opaque; B3 scope
    // observation creates no grant or provider call. This exercises the
    // actual broker executor in addition to its pinned routing wrapper.
    for provider in ["deepseek@1", "openai@1"] {
        let client = Arc::new(RecordingClient::default());
        let executor = BrokeredCandidateExecutor::new(
            client.clone(),
            provider,
            "model",
            &wire_endpoint("open_ai_chat", "openai"),
            UnspecifiedReasoning::ProviderDefault,
            None,
            None,
        )
        .unwrap();
        let mut request = ChatRequest {
            model_binding: awaken_runtime_contract::resolved::ModelBinding::parse(
                "p", "m", "native",
            )
            .unwrap(),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        assert_eq!(
            executor.effect_scope(&request),
            if provider == "deepseek@1" {
                ModelEffectScope::ResponseOnly
            } else {
                ModelEffectScope::Opaque
            }
        );
        let mut hosted = ToolDescriptor::pinned(
            "test",
            "search",
            "search",
            serde_json::json!({"type":"object"}),
        );
        hosted.provider_server_tool = Some(ProviderServerTool::DeepSeekResponsesWebSearch);
        request.tools.push(hosted);
        assert_eq!(executor.effect_scope(&request), ModelEffectScope::Opaque);
        assert!(client.requests.lock().unwrap().is_empty());
    }
}
