//! Capability of the actual native wire mapping, reused by pinned proxies.
use awaken_runtime_contract::llm::ChatRequest;
use awaken_runtime_contract::model_operation::ModelEffectScope;

/// The qualified DeepSeek mappings send data and ordinary function schemas;
/// they execute no provider-hosted tools and retain no independent output task.
/// This describes the adapter contract, not proof that the provider stopped.
/// Other/custom providers and server-tool requests remain opaque.
pub fn provider_model_effect_scope(
    provider: &str,
    dialect: &str,
    adapter: &str,
    request: &ChatRequest,
) -> ModelEffectScope {
    // Published provider references may carry a pinned instance suffix. Its
    // native kind still owns the wire contract; model names grant no capability.
    let provider = provider
        .split_once('@')
        .map_or(provider, |(kind, _)| kind)
        .trim();
    if provider == "deepseek"
        && adapter == "openai"
        && matches!(dialect, "open_ai_chat" | "open_ai_responses")
        && request
            .tools
            .iter()
            .all(|tool| tool.provider_server_tool.is_none())
    {
        ModelEffectScope::ResponseOnly
    } else {
        ModelEffectScope::Opaque
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::{ModelBinding, ProviderServerTool, ToolDescriptor};

    #[test]
    fn qualified_wire_refuses_hosted_execution_and_compatible_provider_aliases() {
        // C1 exact native provider; C2 supported dialect+adapter; C3 hosted
        // execution absent. R1=C1+C2+C3 => response-only. Every missing conjunct
        // => opaque. Arbitrary model names never grant implementation capability.
        let mut request = ChatRequest {
            model_binding: ModelBinding::parse("p", "arbitrary", "genai").unwrap(),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        for provider in [
            "deepseek",
            "deepseek@instance-1",
            "deepseek-compatible",
            "compatible-gateway",
        ] {
            for (dialect, adapter) in [
                ("open_ai_chat", "openai"),
                ("open_ai_responses", "openai"),
                ("anthropic_messages", "anthropic"),
                ("open_ai_chat", "anthropic"),
            ] {
                assert_eq!(
                    provider_model_effect_scope(provider, dialect, adapter, &request),
                    if matches!(provider, "deepseek" | "deepseek@instance-1") && adapter == "openai"
                    {
                        ModelEffectScope::ResponseOnly
                    } else {
                        ModelEffectScope::Opaque
                    }
                );
            }
        }
        let mut tool = ToolDescriptor::pinned(
            "test",
            "search",
            "search",
            serde_json::json!({"type":"object"}),
        );
        request.tools.push(tool.clone());
        assert_eq!(
            provider_model_effect_scope("deepseek", "open_ai_chat", "openai", &request),
            ModelEffectScope::ResponseOnly
        );
        tool.provider_server_tool = Some(ProviderServerTool::DeepSeekResponsesWebSearch);
        request.tools = vec![tool];
        assert_eq!(
            provider_model_effect_scope("deepseek", "open_ai_responses", "openai", &request),
            ModelEffectScope::Opaque
        );
    }
}
