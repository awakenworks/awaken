//! Serializable agent definition — pure data, no trait objects.
//!
//! `AgentSpec` is the unified agent configuration: it describes both the
//! declarative registry references (model, plugins, tools) and the runtime
//! behavior (active_hook_filter filtering, typed plugin sections, context policy).
//!
//! Supersedes the former `AgentProfile` — see ADR-0009.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::contract::inference::{ContextWindowPolicy, ReasoningEffort};
use crate::error::StateError;

// ---------------------------------------------------------------------------
// PluginConfigKey — compile-time binding between key string and config type
// ---------------------------------------------------------------------------

/// Typed plugin configuration key.
///
/// Binds a string key to a concrete config type at compile time.
///
/// ```ignore
/// struct PermissionConfigKey;
/// impl PluginConfigKey for PermissionConfigKey {
///     const KEY: &'static str = "permission";
///     type Config = PermissionConfig;
/// }
/// ```
pub trait PluginConfigKey: 'static + Send + Sync {
    /// Section key in the `sections` map.
    const KEY: &'static str;

    /// Typed configuration value.
    type Config: Default
        + Clone
        + Serialize
        + DeserializeOwned
        + schemars::JsonSchema
        + Send
        + Sync
        + 'static;
}

// ---------------------------------------------------------------------------
// AgentSpec
// ---------------------------------------------------------------------------

/// Serializable agent definition referencing registries by ID.
///
/// Can be saved to JSON, loaded from config files, or transmitted over the network.
/// Resolved at runtime via the resolve pipeline into a `ResolvedAgent`.
///
/// Also serves as the runtime behavior configuration passed to hooks via
/// `PhaseContext.agent_spec`. Plugins read their typed config via `spec.config::<K>()`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSpec {
    /// Unique agent identifier.
    pub id: String,
    /// ModelRegistry ID — resolved to a [`super::traits::ModelEntry`].
    pub model: String,
    /// System prompt sent to the LLM.
    pub system_prompt: String,
    /// Maximum inference rounds before the agent stops.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    /// Maximum continuation retries for truncated LLM responses.
    #[serde(default = "default_max_continuation_retries")]
    pub max_continuation_retries: usize,
    /// Context window management policy. `None` disables compaction and truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<ContextWindowPolicy>,
    /// Default reasoning effort for this agent. `None` means no thinking/reasoning.
    /// Can be overridden per-run via `InferenceOverride` or per-step via plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// PluginRegistry IDs — resolved at build time.
    #[serde(default)]
    pub plugin_ids: Vec<String>,
    /// Runtime hook filter: only hooks from plugins in this set will run.
    /// Empty = no filtering (all loaded plugins' hooks run).
    /// Distinct from `plugin_ids` which controls which plugins are loaded.
    #[serde(
        default,
        skip_serializing_if = "HashSet::is_empty",
        alias = "active_plugins"
    )]
    pub active_hook_filter: HashSet<String>,
    /// Allowed tool IDs (whitelist). `None` = all tools.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Excluded tool IDs (blacklist). Applied after `allowed_tools`.
    #[serde(default)]
    pub excluded_tools: Option<Vec<String>>,
    /// Optional remote endpoint. If set, this agent runs on a remote backend.
    /// If None, this agent runs locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<RemoteEndpoint>,
    /// IDs of sub-agents this agent can delegate to.
    /// Each ID must be a registered agent in the AgentSpecRegistry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delegates: Vec<String>,
    /// Plugin-specific configuration sections (keyed by PluginConfigKey::KEY).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sections: HashMap<String, Value>,
    /// Registry source this agent was loaded from.
    /// `None` for locally defined agents; `Some("cloud")` for agents from the "cloud" registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
}

/// Remote backend authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct RemoteAuth {
    #[serde(rename = "type")]
    pub auth_type: String,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
}

impl RemoteAuth {
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        let mut params = BTreeMap::new();
        params.insert("token".into(), Value::String(token.into()));
        Self {
            auth_type: "bearer".into(),
            params,
        }
    }

    #[must_use]
    pub fn param_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(Value::as_str)
    }
}

/// Remote endpoint configuration for agents running on external backends.
#[derive(Debug, Clone, Serialize, PartialEq, schemars::JsonSchema)]
pub struct RemoteEndpoint {
    #[serde(default = "default_remote_backend")]
    pub backend: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RemoteAuth>,
    /// Target resource on the remote backend. Backend-specific semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, Value>,
}

impl Default for RemoteEndpoint {
    fn default() -> Self {
        Self {
            backend: default_remote_backend(),
            base_url: String::new(),
            auth: None,
            target: None,
            timeout_ms: default_timeout(),
            options: BTreeMap::new(),
        }
    }
}

fn default_remote_backend() -> String {
    "a2a".to_string()
}

fn default_timeout() -> u64 {
    300_000
}

#[derive(Debug, Deserialize)]
struct RawRemoteEndpoint {
    #[serde(default)]
    backend: Option<String>,
    base_url: String,
    #[serde(default)]
    auth: Option<RemoteAuth>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    options: BTreeMap<String, Value>,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    poll_interval_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for RemoteEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawRemoteEndpoint::deserialize(deserializer)?;
        let has_legacy_fields =
            raw.bearer_token.is_some() || raw.agent_id.is_some() || raw.poll_interval_ms.is_some();
        let has_canonical_fields = raw.backend.is_some()
            || raw.auth.is_some()
            || raw.target.is_some()
            || !raw.options.is_empty();

        if has_legacy_fields && has_canonical_fields {
            return Err(serde::de::Error::custom(
                "cannot mix legacy A2A endpoint fields with canonical remote endpoint fields",
            ));
        }

        if has_legacy_fields {
            let mut options = BTreeMap::new();
            if let Some(poll_interval_ms) = raw.poll_interval_ms {
                options.insert("poll_interval_ms".into(), Value::from(poll_interval_ms));
            }
            return Ok(Self {
                backend: default_remote_backend(),
                base_url: raw.base_url,
                auth: raw.bearer_token.map(RemoteAuth::bearer),
                target: raw.agent_id,
                timeout_ms: raw.timeout_ms.unwrap_or_else(default_timeout),
                options,
            });
        }

        let backend = raw.backend.unwrap_or_else(default_remote_backend);
        if backend.trim().is_empty() {
            return Err(serde::de::Error::custom(
                "remote endpoint backend must not be empty",
            ));
        }

        Ok(Self {
            backend,
            base_url: raw.base_url,
            auth: raw.auth,
            target: raw.target,
            timeout_ms: raw.timeout_ms.unwrap_or_else(default_timeout),
            options: raw.options,
        })
    }
}

// ---------------------------------------------------------------------------
// ModelSpec
// ---------------------------------------------------------------------------

/// Serializable model definition mapping a stable ID to a provider and model name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSpec {
    /// Unique identifier (for example `"gpt-4o-mini"` or `"research-default"`).
    pub id: String,
    /// Provider spec ID referenced by this model.
    pub provider: String,
    /// Actual model name sent to the upstream provider.
    pub model: String,
}

// ---------------------------------------------------------------------------
// ProviderSpec
// ---------------------------------------------------------------------------

/// Serializable provider configuration used to construct an LLM executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderSpec {
    /// Unique identifier (for example `"openai"` or `"anthropic-prod"`).
    pub id: String,
    /// GenAI adapter kind (for example `"openai"`, `"anthropic"`, `"ollama"`).
    pub adapter: String,
    /// Explicit API key. If absent, the adapter's environment variable is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Base URL override for proxy or self-hosted deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    #[serde(default = "default_provider_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_provider_timeout_secs() -> u64 {
    300
}

// ---------------------------------------------------------------------------
// McpServerSpec
// ---------------------------------------------------------------------------

/// Transport type for an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    /// Launch an MCP server as a child process over stdio.
    Stdio,
    /// Connect to an MCP server over HTTP.
    Http,
}

/// Restart policy for MCP server connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpRestartPolicy {
    /// Whether to automatically restart on failure.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of restart attempts. `None` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Delay between restart attempts in milliseconds.
    #[serde(default = "default_mcp_restart_delay_ms")]
    pub delay_ms: u64,
    /// Exponential backoff multiplier.
    #[serde(default = "default_mcp_restart_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Maximum delay between restarts in milliseconds.
    #[serde(default = "default_mcp_restart_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for McpRestartPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: None,
            delay_ms: default_mcp_restart_delay_ms(),
            backoff_multiplier: default_mcp_restart_backoff_multiplier(),
            max_delay_ms: default_mcp_restart_max_delay_ms(),
        }
    }
}

const fn default_mcp_restart_delay_ms() -> u64 {
    1000
}

const fn default_mcp_restart_backoff_multiplier() -> f64 {
    2.0
}

const fn default_mcp_restart_max_delay_ms() -> u64 {
    30_000
}

/// Serializable MCP server configuration used to construct a live MCP tool registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpServerSpec {
    /// Unique identifier and MCP server name.
    pub id: String,
    /// Connection transport kind.
    pub transport: McpTransportKind,
    /// Command to execute when using stdio transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Command arguments for stdio transport.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// URL for HTTP transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Server-specific configuration payload forwarded during initialization.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, Value>,
    /// Connection timeout in seconds.
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
    /// Environment variables for stdio transport.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Restart policy for reconnecting failed servers.
    #[serde(default)]
    pub restart_policy: McpRestartPolicy,
}

fn default_mcp_timeout_secs() -> u64 {
    30
}

impl Default for McpServerSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            transport: McpTransportKind::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            config: serde_json::Map::new(),
            timeout_secs: default_mcp_timeout_secs(),
            env: BTreeMap::new(),
            restart_policy: McpRestartPolicy::default(),
        }
    }
}

impl Default for ProviderSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            adapter: String::new(),
            api_key: None,
            base_url: None,
            timeout_secs: default_provider_timeout_secs(),
        }
    }
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            system_prompt: String::new(),
            max_rounds: default_max_rounds(),
            max_continuation_retries: default_max_continuation_retries(),
            context_policy: None,
            reasoning_effort: None,
            plugin_ids: Vec::new(),
            active_hook_filter: HashSet::new(),
            allowed_tools: None,
            excluded_tools: None,
            endpoint: None,
            delegates: Vec::new(),
            sections: HashMap::new(),
            registry: None,
        }
    }
}

fn default_max_rounds() -> usize {
    16
}

fn default_max_continuation_retries() -> usize {
    2
}

impl AgentSpec {
    /// Create a new agent spec with default settings.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::registry_spec::AgentSpec;
    ///
    /// let spec = AgentSpec::new("assistant")
    ///     .with_model("gpt-4o-mini")
    ///     .with_system_prompt("You are helpful.")
    ///     .with_max_rounds(10);
    /// assert_eq!(spec.id, "assistant");
    /// assert_eq!(spec.model, "gpt-4o-mini");
    /// assert_eq!(spec.system_prompt, "You are helpful.");
    /// assert_eq!(spec.max_rounds, 10);
    /// ```
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }

    // -- Typed config access --

    /// Read a typed plugin config section.
    /// Returns `Config::default()` if the section is missing.
    /// Returns error if the section exists but fails to deserialize.
    pub fn config<K: PluginConfigKey>(&self) -> Result<K::Config, StateError> {
        match self.sections.get(K::KEY) {
            Some(value) => {
                serde_json::from_value(value.clone()).map_err(|e| StateError::KeyDecode {
                    key: K::KEY.into(),
                    message: e.to_string(),
                })
            }
            None => Ok(K::Config::default()),
        }
    }

    /// Set a typed plugin config section.
    pub fn set_config<K: PluginConfigKey>(&mut self, config: K::Config) -> Result<(), StateError> {
        let value = serde_json::to_value(config).map_err(|e| StateError::KeyEncode {
            key: K::KEY.into(),
            message: e.to_string(),
        })?;
        self.sections.insert(K::KEY.to_string(), value);
        Ok(())
    }

    // -- Builder methods --

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    #[must_use]
    pub fn with_max_rounds(mut self, n: usize) -> Self {
        self.max_rounds = n;
        self
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    #[must_use]
    pub fn with_hook_filter(mut self, plugin_id: impl Into<String>) -> Self {
        self.active_hook_filter.insert(plugin_id.into());
        self
    }

    /// Set a typed plugin config section (builder variant).
    pub fn with_config<K: PluginConfigKey>(
        mut self,
        config: K::Config,
    ) -> Result<Self, StateError> {
        self.set_config::<K>(config)?;
        Ok(self)
    }

    #[must_use]
    pub fn with_delegate(mut self, agent_id: impl Into<String>) -> Self {
        self.delegates.push(agent_id.into());
        self
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: RemoteEndpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// Set a raw JSON section (for tests or untyped usage).
    #[must_use]
    pub fn with_section(mut self, key: impl Into<String>, value: Value) -> Self {
        self.sections.insert(key.into(), value);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_spec_serde_roundtrip() {
        let spec = AgentSpec {
            id: "coder".into(),
            model: "claude-opus".into(),
            system_prompt: "You are a coding assistant.".into(),
            max_rounds: 8,
            plugin_ids: vec!["permission".into(), "logging".into()],
            allowed_tools: Some(vec!["read_file".into(), "write_file".into()]),
            excluded_tools: Some(vec!["delete_file".into()]),
            sections: {
                let mut m = HashMap::new();
                m.insert("permission".into(), json!({"mode": "strict"}));
                m
            },
            ..Default::default()
        };

        let json_str = serde_json::to_string(&spec).unwrap();
        let parsed: AgentSpec = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.id, "coder");
        assert_eq!(parsed.model, "claude-opus");
        assert_eq!(parsed.system_prompt, "You are a coding assistant.");
        assert_eq!(parsed.max_rounds, 8);
        assert_eq!(parsed.plugin_ids, vec!["permission", "logging"]);
        assert_eq!(
            parsed.allowed_tools,
            Some(vec!["read_file".into(), "write_file".into()])
        );
        assert_eq!(parsed.excluded_tools, Some(vec!["delete_file".into()]));
        assert_eq!(parsed.sections["permission"]["mode"], "strict");
    }

    #[test]
    fn agent_spec_defaults() {
        let json_str = r#"{"id":"min","model":"m","system_prompt":"sp"}"#;
        let spec: AgentSpec = serde_json::from_str(json_str).unwrap();

        assert_eq!(spec.max_rounds, 16);
        assert_eq!(spec.max_continuation_retries, 2);
        assert!(spec.context_policy.is_none());
        assert!(spec.plugin_ids.is_empty());
        assert!(spec.active_hook_filter.is_empty());
        assert!(spec.allowed_tools.is_none());
        assert!(spec.excluded_tools.is_none());
        assert!(spec.sections.is_empty());
    }

    // -- Typed config tests (merged from AgentProfile) --

    struct ModelNameKey;
    impl PluginConfigKey for ModelNameKey {
        const KEY: &'static str = "model_name";
        type Config = ModelNameConfig;
    }

    #[derive(
        Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
    )]
    struct ModelNameConfig {
        pub name: String,
    }

    struct PermKey;
    impl PluginConfigKey for PermKey {
        const KEY: &'static str = "permission";
        type Config = PermConfig;
    }

    #[derive(
        Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
    )]
    struct PermConfig {
        pub mode: String,
    }

    #[test]
    fn typed_config_roundtrip() {
        let spec = AgentSpec::new("test")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "opus".into(),
            })
            .unwrap()
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "opus");

        let perm: PermConfig = spec.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "strict");
    }

    #[test]
    fn missing_config_returns_default() {
        let spec = AgentSpec::new("test");
        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model, ModelNameConfig::default());
    }

    #[test]
    fn config_serializes_to_json() {
        let spec = AgentSpec::new("coder")
            .with_model("sonnet")
            .with_config::<ModelNameKey>(ModelNameConfig {
                name: "custom".into(),
            })
            .unwrap();

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: AgentSpec = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "coder");
        assert_eq!(parsed.model, "sonnet");

        let model: ModelNameConfig = parsed.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "custom");
    }

    #[test]
    fn multiple_configs_independent() {
        let mut spec = AgentSpec::new("test");
        spec.set_config::<ModelNameKey>(ModelNameConfig { name: "a".into() })
            .unwrap();
        spec.set_config::<PermKey>(PermConfig { mode: "b".into() })
            .unwrap();

        // Update one doesn't affect the other
        spec.set_config::<ModelNameKey>(ModelNameConfig {
            name: "updated".into(),
        })
        .unwrap();

        let model: ModelNameConfig = spec.config::<ModelNameKey>().unwrap();
        assert_eq!(model.name, "updated");

        let perm: PermConfig = spec.config::<PermKey>().unwrap();
        assert_eq!(perm.mode, "b");
    }

    #[test]
    fn with_section_raw_json_still_works() {
        let spec =
            AgentSpec::new("test").with_section("custom", serde_json::json!({"key": "value"}));
        assert_eq!(spec.sections["custom"]["key"], "value");
    }

    #[test]
    fn remote_endpoint_canonical_roundtrip_uses_single_shape() {
        let mut options = BTreeMap::new();
        options.insert("poll_interval_ms".into(), json!(1000));
        let endpoint = RemoteEndpoint {
            backend: "a2a".into(),
            base_url: "https://remote.example.com/v1/a2a".into(),
            auth: Some(RemoteAuth::bearer("tok_123")),
            target: Some("worker".into()),
            timeout_ms: 60_000,
            options,
        };

        let encoded = serde_json::to_value(&endpoint).unwrap();
        assert_eq!(encoded["backend"], "a2a");
        assert_eq!(encoded["auth"]["type"], "bearer");
        assert_eq!(encoded["auth"]["token"], "tok_123");
        assert_eq!(encoded["target"], "worker");
        assert_eq!(encoded["options"]["poll_interval_ms"], 1000);
        assert!(encoded.get("bearer_token").is_none());
        assert!(encoded.get("agent_id").is_none());
        assert!(encoded.get("poll_interval_ms").is_none());

        let parsed: RemoteEndpoint = serde_json::from_value(encoded).unwrap();
        assert_eq!(parsed, endpoint);
    }

    #[test]
    fn remote_endpoint_legacy_a2a_input_normalizes_to_canonical_shape() {
        let endpoint: RemoteEndpoint = serde_json::from_value(json!({
            "base_url": "https://remote.example.com/v1/a2a",
            "bearer_token": "tok_legacy",
            "agent_id": "worker",
            "poll_interval_ms": 750,
            "timeout_ms": 60_000
        }))
        .unwrap();

        assert_eq!(endpoint.backend, "a2a");
        assert_eq!(
            endpoint
                .auth
                .as_ref()
                .and_then(|auth| auth.param_str("token")),
            Some("tok_legacy")
        );
        assert_eq!(endpoint.target.as_deref(), Some("worker"));
        assert_eq!(endpoint.options.get("poll_interval_ms"), Some(&json!(750)));
        assert_eq!(endpoint.timeout_ms, 60_000);
    }

    #[test]
    fn remote_endpoint_rejects_mixed_legacy_and_canonical_fields() {
        let err = serde_json::from_value::<RemoteEndpoint>(json!({
            "backend": "a2a",
            "base_url": "https://remote.example.com/v1/a2a",
            "auth": { "type": "bearer", "token": "tok_new" },
            "bearer_token": "tok_old"
        }))
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot mix legacy A2A endpoint fields")
        );
    }

    #[test]
    fn builder() {
        let spec = AgentSpec::new("reviewer")
            .with_model("claude-opus")
            .with_hook_filter("permission")
            .with_config::<PermKey>(PermConfig {
                mode: "strict".into(),
            })
            .unwrap();

        assert_eq!(spec.id, "reviewer");
        assert_eq!(spec.model, "claude-opus");
        assert!(spec.active_hook_filter.contains("permission"));
    }
}
