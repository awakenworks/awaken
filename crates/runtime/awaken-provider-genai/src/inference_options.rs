//! Typed inference-control projection at the final provider adapter boundary.

use awaken_runtime_contract::UnspecifiedReasoning;
use awaken_runtime_contract::agent_bindings::{InferenceSpeed, ReasoningEffort};
use awaken_runtime_contract::llm::{ChatRequest, Error, Result};
use genai::chat::{ChatOptions, ReasoningEffort as GenaiReasoningEffort};

/// Materialize typed snapshot/request controls into one provider call. A control
/// the adapter cannot faithfully express is rejected before network I/O.
pub(crate) fn materialize(
    request: &ChatRequest,
    _streaming: bool,
    unspecified_reasoning: UnspecifiedReasoning,
) -> Result<ChatOptions> {
    if request.inference.speed == Some(InferenceSpeed::Fast) {
        return Err(Error::InvalidRequest(
            "inference speed `fast` is not supported by the configured genai adapter".into(),
        ));
    }
    if let Some(geo) = &request.inference.inference_geo {
        return Err(Error::InvalidRequest(format!(
            "inference_geo `{geo}` is not supported by the configured genai adapter"
        )));
    }
    let mut options = ChatOptions::default();
    if let Some(effort) = request.inference.effort {
        options = options.with_reasoning_effort(match effort {
            ReasoningEffort::Low => GenaiReasoningEffort::Low,
            ReasoningEffort::Medium => GenaiReasoningEffort::Medium,
            ReasoningEffort::High => GenaiReasoningEffort::High,
            ReasoningEffort::Xhigh => GenaiReasoningEffort::XHigh,
            ReasoningEffort::Max => GenaiReasoningEffort::Max,
        });
    } else if unspecified_reasoning == UnspecifiedReasoning::Disabled {
        // JSON is intentionally confined to this SDK transport boundary. The
        // control/runtime boundary carries only the closed policy enum.
        options = options.with_extra_body(serde_json::json!({
            "thinking": { "type": "disabled" }
        }));
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Role;
    use awaken_runtime_contract::UnspecifiedReasoning;
    use awaken_runtime_contract::agent_bindings::{
        InferenceOptions, InferenceSpeed, ReasoningEffort,
    };
    use awaken_runtime_contract::llm::{ChatMessage, ChatRequest};
    use awaken_runtime_contract::resolved::ModelBinding;
    use genai::chat::ReasoningEffort as GenaiReasoningEffort;

    use super::materialize;
    use crate::{AdapterKind, GenaiExecutor};

    fn controlled_request(inference: InferenceOptions) -> ChatRequest {
        ChatRequest {
            model_binding: ModelBinding::new("provider", "claude-opus-4-8", "genai"),
            inference,
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text("hello")],
            }],
            tools: Vec::new(),
        }
    }

    #[test]
    fn typed_inference_controls_are_materialized_or_rejected_before_io() {
        // Causal graph / decision table:
        // C1 high + standard + no geo -> provider reasoning effort High.
        // C2 no controls -> provider defaults.
        // C3 fast -> stable pre-I/O rejection.
        // C4 processing geography -> stable pre-I/O rejection.
        let standard = controlled_request(InferenceOptions {
            effort: Some(ReasoningEffort::High),
            speed: Some(InferenceSpeed::Standard),
            inference_geo: None,
        });
        let options = materialize(&standard, false, UnspecifiedReasoning::ProviderDefault).unwrap();
        assert!(matches!(
            options.reasoning_effort,
            Some(GenaiReasoningEffort::High)
        ));

        let defaults = materialize(
            &controlled_request(Default::default()),
            false,
            UnspecifiedReasoning::ProviderDefault,
        )
        .unwrap();
        assert!(defaults.reasoning_effort.is_none());

        let fast = controlled_request(InferenceOptions {
            effort: Some(ReasoningEffort::Max),
            speed: Some(InferenceSpeed::Fast),
            inference_geo: None,
        });
        let error = materialize(&fast, false, UnspecifiedReasoning::ProviderDefault).unwrap_err();
        assert_eq!(error.code(), "invalid_request");

        let geo = controlled_request(InferenceOptions {
            effort: None,
            speed: None,
            inference_geo: Some(awaken_runtime_contract::agent_bindings::InferenceGeography::Us),
        });
        let error = materialize(&geo, false, UnspecifiedReasoning::ProviderDefault).unwrap_err();
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("inference_geo `us`"));
    }

    #[test]
    fn unspecified_reasoning_is_driven_by_policy_not_provider_hostname() {
        // Cause/effect decision table:
        // D1 explicit Disabled + no authored effort -> adapter-only JSON field.
        // D2 explicit Disabled + authored effort -> authored request wins.
        // D3 DeepSeek-shaped URL + default policy -> no hostname inference.
        let defaults = materialize(
            &controlled_request(Default::default()),
            true,
            UnspecifiedReasoning::Disabled,
        )
        .unwrap();
        assert_eq!(
            defaults.extra_body,
            Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
            "D1",
        );

        let reasoned = materialize(
            &controlled_request(InferenceOptions {
                effort: Some(ReasoningEffort::High),
                ..Default::default()
            }),
            true,
            UnspecifiedReasoning::Disabled,
        )
        .unwrap();
        assert!(reasoned.extra_body.is_none(), "D2");
        assert!(matches!(
            reasoned.reasoning_effort,
            Some(GenaiReasoningEffort::High)
        ));

        let compatible = GenaiExecutor::from_resolved(
            AdapterKind::OpenAI,
            Some("https://api.deepseek.com".into()),
            "fixture-key",
        );
        assert!(
            compatible
                .chat_options(&controlled_request(Default::default()), true)
                .unwrap()
                .extra_body
                .is_none(),
            "D3",
        );
    }
}
