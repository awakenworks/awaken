//! ContextTransformPlugin — registers the context truncation request transform.

use serde::{Deserialize, Serialize};

use crate::context::ArtifactCompactionConfig;
use crate::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};

/// Plugin ID for context truncation transform.
pub const CONTEXT_TRANSFORM_PLUGIN_ID: &str = "context_transform";

/// Configuration for the built-in context request transform.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct ContextTransformConfig {
    pub artifact_compaction: ArtifactCompactionConfig,
}

/// Plugin config key for [`ContextTransformConfig`].
pub struct ContextTransformConfigKey;

impl awaken_runtime_contract::registry_spec::PluginConfigKey for ContextTransformConfigKey {
    const KEY: &'static str = "context_transform";
    type Config = ContextTransformConfig;
}

/// Plugin that registers the built-in context truncation request transform.
/// Wraps a `ContextWindowPolicy` and registers a `ContextTransform` via
/// `register_request_transform()` during plugin registration (ADR-0001).
pub struct ContextTransformPlugin {
    policy: awaken_runtime_contract::contract::inference::ContextWindowPolicy,
    config: ContextTransformConfig,
}

impl ContextTransformPlugin {
    pub fn new(policy: awaken_runtime_contract::contract::inference::ContextWindowPolicy) -> Self {
        Self::with_config(policy, ContextTransformConfig::default())
    }

    pub fn with_config(
        policy: awaken_runtime_contract::contract::inference::ContextWindowPolicy,
        config: ContextTransformConfig,
    ) -> Self {
        Self { policy, config }
    }

    /// Effective policy this plugin will install as a request transform.
    ///
    /// Only used by resolve-pipeline tests; gated to avoid `dead_code` in
    /// release builds where no consumer reaches in to read the policy.
    #[cfg(test)]
    pub(crate) fn policy(
        &self,
    ) -> &awaken_runtime_contract::contract::inference::ContextWindowPolicy {
        &self.policy
    }

    #[cfg(test)]
    pub(crate) fn config(&self) -> &ContextTransformConfig {
        &self.config
    }
}

impl Plugin for ContextTransformPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: CONTEXT_TRANSFORM_PLUGIN_ID,
        }
    }

    fn register(
        &self,
        registrar: &mut PluginRegistrar,
    ) -> Result<(), awaken_runtime_contract::StateError> {
        self.config
            .artifact_compaction
            .validate()
            .map_err(|message| awaken_runtime_contract::StateError::KeyDecode {
                key: <ContextTransformConfigKey as awaken_runtime_contract::registry_spec::PluginConfigKey>::KEY
                    .into(),
                message,
            })?;
        registrar.register_request_transform(
            CONTEXT_TRANSFORM_PLUGIN_ID,
            crate::context::ContextTransform::with_artifact_compaction(
                self.policy.clone(),
                self.config.artifact_compaction.clone(),
            ),
        );
        Ok(())
    }

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![
            ConfigSchema::for_key::<ContextTransformConfigKey>()
                .with_display_name("Context Transform")
                .with_description("Artifact compaction and request context transformation.")
                .with_category("context"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::plugin::CompactionPlugin;

    #[test]
    fn context_transform_plugin_descriptor_name() {
        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy::default();
        let plugin = ContextTransformPlugin::new(policy);
        assert_eq!(plugin.descriptor().name, CONTEXT_TRANSFORM_PLUGIN_ID);
    }

    #[test]
    fn context_transform_plugin_registers_transform() {
        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy::default();
        let plugin = ContextTransformPlugin::new(policy);
        let mut registrar = PluginRegistrar::new();
        plugin.register(&mut registrar).unwrap();
        assert_eq!(
            registrar.request_transforms.len(),
            1,
            "should have registered one transform"
        );
        assert_eq!(
            registrar.request_transforms[0].plugin_id,
            CONTEXT_TRANSFORM_PLUGIN_ID
        );
    }

    #[test]
    fn context_transform_plugin_uses_artifact_compaction_config() {
        use awaken_runtime_contract::contract::message::{Message, ToolCall};
        use serde_json::json;

        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy {
            max_context_tokens: 100_000,
            max_output_tokens: 0,
            ..Default::default()
        };
        let plugin = ContextTransformPlugin::with_config(
            policy,
            ContextTransformConfig {
                artifact_compaction: ArtifactCompactionConfig {
                    threshold_tokens: 1,
                    preview_max_chars: 6,
                    preview_max_lines: 1,
                },
            },
        );
        assert_eq!(
            plugin.config().artifact_compaction.preview_max_chars,
            6,
            "plugin keeps the typed artifact compaction config"
        );
        let mut registrar = PluginRegistrar::new();
        plugin.register(&mut registrar).unwrap();

        let output = registrar.request_transforms[0].transform.transform(
            vec![
                Message::assistant_with_tool_calls(
                    "",
                    vec![ToolCall::new("c1", "large", json!({}))],
                ),
                Message::tool("c1", "abcdefghijklmnop"),
            ],
            &[],
        );
        assert!(output.messages[1].text().starts_with("abcdef"));
        assert!(output.messages[1].text().contains("[Content compacted:"));
    }

    #[test]
    fn context_transform_plugin_rejects_invalid_artifact_compaction_config() {
        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy::default();
        let plugin = ContextTransformPlugin::with_config(
            policy,
            ContextTransformConfig {
                artifact_compaction: ArtifactCompactionConfig {
                    threshold_tokens: 0,
                    ..Default::default()
                },
            },
        );
        let mut registrar = PluginRegistrar::new();

        let err = plugin
            .register(&mut registrar)
            .expect_err("plugin registration should reject invalid runtime config");

        assert!(err.to_string().contains("threshold_tokens"));
        assert!(registrar.request_transforms.is_empty());
    }

    #[test]
    fn context_transform_plugin_declares_config_schema() {
        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy::default();
        let plugin = ContextTransformPlugin::new(policy);
        let schemas = plugin.config_schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0].key,
            <ContextTransformConfigKey as awaken_runtime_contract::registry_spec::PluginConfigKey>::KEY
        );
    }

    #[test]
    fn transform_ordering_compaction_then_context() {
        // Compaction plugin should register no transforms
        let mut reg_compaction = PluginRegistrar::new();
        CompactionPlugin::default()
            .register(&mut reg_compaction)
            .unwrap();
        assert!(
            reg_compaction.request_transforms.is_empty(),
            "CompactionPlugin should not register request transforms"
        );
        // ContextTransformPlugin should register exactly one transform
        let policy = awaken_runtime_contract::contract::inference::ContextWindowPolicy::default();
        let mut reg_transform = PluginRegistrar::new();
        ContextTransformPlugin::new(policy)
            .register(&mut reg_transform)
            .unwrap();
        assert_eq!(reg_transform.request_transforms.len(), 1);
    }

    #[test]
    fn token_count_estimation_for_various_content_types() {
        use awaken_runtime_contract::contract::message::Message;
        use awaken_runtime_contract::contract::transform::estimate_message_tokens;

        // Text message
        let text_msg = Message::user("Hello, this is a test message with some content.");
        let text_tokens = estimate_message_tokens(&text_msg);
        assert!(
            text_tokens > 4,
            "text message should have tokens beyond overhead"
        );

        // Empty content message
        let empty_msg = Message::user("");
        let empty_tokens = estimate_message_tokens(&empty_msg);
        assert_eq!(
            empty_tokens, 4,
            "empty message should have only overhead tokens"
        );

        // Very long message
        let long_msg = Message::user("x".repeat(4000));
        let long_tokens = estimate_message_tokens(&long_msg);
        assert!(
            long_tokens >= 1000,
            "4000-char message should estimate >= 1000 tokens, got {long_tokens}"
        );
    }

    #[test]
    fn enable_prompt_cache_flag_in_policy() {
        let policy_cached = awaken_runtime_contract::contract::inference::ContextWindowPolicy {
            enable_prompt_cache: true,
            ..Default::default()
        };
        assert!(policy_cached.enable_prompt_cache);

        let policy_uncached = awaken_runtime_contract::contract::inference::ContextWindowPolicy {
            enable_prompt_cache: false,
            ..Default::default()
        };
        assert!(!policy_uncached.enable_prompt_cache);

        // Both should create valid transform plugins
        let _ = ContextTransformPlugin::new(policy_cached);
        let _ = ContextTransformPlugin::new(policy_uncached);
    }

    #[test]
    fn autocompact_threshold_check() {
        use awaken_runtime_contract::contract::message::Message;
        use awaken_runtime_contract::contract::transform::estimate_tokens;

        let policy_with_threshold =
            awaken_runtime_contract::contract::inference::ContextWindowPolicy {
                autocompact_threshold: Some(500),
                ..Default::default()
            };

        // Simulate checking if messages exceed autocompact threshold
        let messages = vec![Message::user("short"), Message::assistant("reply")];
        let total = estimate_tokens(&messages);
        assert!(
            total < policy_with_threshold.autocompact_threshold.unwrap(),
            "short conversation should be under threshold"
        );

        // Longer conversation should exceed threshold
        let long_messages: Vec<Message> = (0..100)
            .map(|i| Message::user(format!("message {i} with some filler text to add tokens")))
            .collect();
        let long_total = estimate_tokens(&long_messages);
        assert!(
            long_total > policy_with_threshold.autocompact_threshold.unwrap(),
            "100-message conversation should exceed threshold of 500, got {long_total}"
        );
    }
}
