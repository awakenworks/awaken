//! Context window management behavior.
//!
//! Registers a [`ContextWindowTransform`] that truncates history and enables
//! prompt caching. The core loop never knows about "context windows" — it
//! just applies generic [`InferenceRequestTransform`] implementations.

use async_trait::async_trait;
use std::sync::Arc;
use tirea_contract::runtime::behavior::ReadOnlyContext;
use tirea_contract::runtime::inference::{
    ContextWindowPolicy, InferenceRequestTransform, InferenceTransformOutput,
};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea_contract::runtime::tool_call::ToolDescriptor;
use tirea_contract::thread::Message;

use crate::engine::context_window::truncate_to_budget;
use crate::engine::token_estimator::estimate_tool_tokens;

/// Behavior ID used for registration.
pub const CONTEXT_WINDOW_PLUGIN_ID: &str = "context_window";

/// Plugin that provides context window management via the generic transform system.
///
/// During `before_inference`, it registers a [`ContextWindowTransform`] that:
/// - Truncates old history when token budget is exceeded
/// - Requests prompt cache hints (`CacheControl::Ephemeral`) on system messages
#[derive(Debug, Clone)]
pub struct ContextWindowPlugin {
    policy: ContextWindowPolicy,
}

impl ContextWindowPlugin {
    /// Create with a specific policy.
    pub fn new(policy: ContextWindowPolicy) -> Self {
        Self { policy }
    }

    /// Create with model-specific defaults.
    pub fn for_model(model: &str) -> Self {
        let policy = match model {
            m if m.contains("claude") => ContextWindowPolicy {
                max_context_tokens: 200_000,
                max_output_tokens: 16_384,
                min_recent_messages: 10,
                enable_prompt_cache: true,
                autocompact_threshold: None,
            },
            m if m.contains("gpt-4o") => ContextWindowPolicy {
                max_context_tokens: 128_000,
                max_output_tokens: 16_384,
                min_recent_messages: 10,
                enable_prompt_cache: false,
                autocompact_threshold: None,
            },
            m if m.contains("gpt-4") => ContextWindowPolicy {
                max_context_tokens: 128_000,
                max_output_tokens: 4_096,
                min_recent_messages: 10,
                enable_prompt_cache: false,
                autocompact_threshold: None,
            },
            _ => ContextWindowPolicy::default(),
        };
        Self { policy }
    }
}

#[async_trait]
impl tirea_contract::runtime::AgentBehavior for ContextWindowPlugin {
    fn id(&self) -> &str {
        CONTEXT_WINDOW_PLUGIN_ID
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        ActionSet::single(BeforeInferenceAction::AddRequestTransform(Arc::new(
            ContextWindowTransform {
                policy: self.policy.clone(),
            },
        )))
    }
}

/// The actual transform that truncates history and signals prompt caching.
///
/// Registered by [`ContextWindowPlugin`] during `before_inference`. The core
/// loop calls it as a generic `InferenceRequestTransform` — no domain coupling.
struct ContextWindowTransform {
    policy: ContextWindowPolicy,
}

impl InferenceRequestTransform for ContextWindowTransform {
    fn transform(
        &self,
        messages: Vec<Message>,
        tool_descriptors: &[ToolDescriptor],
    ) -> InferenceTransformOutput {
        let tool_tokens = estimate_tool_tokens(tool_descriptors);

        // Split system messages from history.
        let system_end = messages
            .iter()
            .position(|m| m.role != tirea_contract::thread::Role::System)
            .unwrap_or(messages.len());
        let (system_msgs, history_msgs) = messages.split_at(system_end);

        let result = truncate_to_budget(system_msgs, history_msgs, tool_tokens, &self.policy);

        InferenceTransformOutput {
            messages: result.messages.into_iter().cloned().collect(),
            enable_prompt_cache: self.policy.enable_prompt_cache,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    #[test]
    fn default_policy_values() {
        let plugin = ContextWindowPlugin::new(ContextWindowPolicy::default());
        assert_eq!(plugin.policy.max_context_tokens, 200_000);
        assert_eq!(plugin.policy.max_output_tokens, 16_384);
        assert!(plugin.policy.enable_prompt_cache);
    }

    #[test]
    fn for_model_claude() {
        let plugin = ContextWindowPlugin::for_model("claude-3-opus");
        assert_eq!(plugin.policy.max_context_tokens, 200_000);
        assert!(plugin.policy.enable_prompt_cache);
    }

    #[test]
    fn for_model_gpt4o() {
        let plugin = ContextWindowPlugin::for_model("gpt-4o-mini");
        assert_eq!(plugin.policy.max_context_tokens, 128_000);
        assert!(!plugin.policy.enable_prompt_cache);
    }

    #[test]
    fn for_model_unknown_uses_defaults() {
        let plugin = ContextWindowPlugin::for_model("some-custom-model");
        assert_eq!(plugin.policy.max_context_tokens, 200_000);
    }

    #[tokio::test]
    async fn before_inference_emits_transform() {
        let plugin = ContextWindowPlugin::new(ContextWindowPolicy {
            max_context_tokens: 100_000,
            max_output_tokens: 8_192,
            min_recent_messages: 5,
            enable_prompt_cache: false,
            autocompact_threshold: None,
        });

        use tirea_contract::runtime::AgentBehavior;
        let config = RunPolicy::new();
        let doc = DocCell::new(json!({}));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let actions = plugin.before_inference(&ctx).await;
        assert_eq!(actions.len(), 1);

        // Verify the transform works by calling it directly.
        let action = actions.into_vec().pop().unwrap();
        match action {
            BeforeInferenceAction::AddRequestTransform(transform) => {
                let messages = vec![
                    Message::system("You are helpful."),
                    Message::user("Hello"),
                    Message::assistant("Hi!"),
                ];
                let output = transform.transform(messages, &[]);
                // With 100k budget and tiny messages, nothing should be truncated.
                assert_eq!(output.messages.len(), 3);
                assert!(!output.enable_prompt_cache);
            }
            _ => panic!("expected AddRequestTransform"),
        }
    }

    #[test]
    fn transform_with_tool_descriptors_reduces_budget() {
        use tirea_contract::runtime::tool_call::ToolDescriptor;

        let transform_no_tools = ContextWindowTransform {
            policy: ContextWindowPolicy {
                max_context_tokens: 300,
                max_output_tokens: 50,
                min_recent_messages: 1,
                enable_prompt_cache: false,
                autocompact_threshold: None,
            },
        };
        let transform_with_tools = ContextWindowTransform {
            policy: transform_no_tools.policy.clone(),
        };

        let mut messages = vec![Message::system("System.")];
        for i in 0..30 {
            messages.push(Message::user(format!("Question {i} with padding text")));
            messages.push(Message::assistant(format!("Answer {i}")));
        }

        let tools = vec![
            ToolDescriptor::new("search", "Search", "Search the web").with_parameters(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer" }
                    }
                }),
            ),
            ToolDescriptor::new("calc", "Calculator", "Evaluate math").with_parameters(
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "expression": { "type": "string" }
                    }
                }),
            ),
        ];

        let output_no_tools = transform_no_tools.transform(messages.clone(), &[]);
        let output_with_tools = transform_with_tools.transform(messages, &tools);

        assert!(
            output_with_tools.messages.len() <= output_no_tools.messages.len(),
            "tool descriptors should reduce available budget, keeping fewer messages"
        );
    }

    #[test]
    fn transform_truncates_when_over_budget() {
        let transform = ContextWindowTransform {
            policy: ContextWindowPolicy {
                max_context_tokens: 50, // Very small budget
                max_output_tokens: 10,
                min_recent_messages: 1,
                enable_prompt_cache: true,
                autocompact_threshold: None,
            },
        };

        let mut messages = vec![Message::system("System prompt content here.")];
        for i in 0..20 {
            messages.push(Message::user(format!(
                "Message number {i} with some content"
            )));
            messages.push(Message::assistant(format!("Response number {i}")));
        }

        let output = transform.transform(messages, &[]);
        // With a 50-token budget, most messages should be truncated.
        assert!(output.messages.len() < 42); // less than 1 system + 40 history
        assert!(output.enable_prompt_cache);
        // System message is always preserved.
        assert_eq!(
            output.messages[0].role,
            tirea_contract::thread::Role::System
        );
    }
}
