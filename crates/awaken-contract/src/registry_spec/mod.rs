//! Serializable agent definition — pure data, no trait objects.
//!
//! `AgentSpec` is the unified agent configuration: it describes both the
//! declarative registry references (model id, plugins, tools) and the runtime
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

/// Internal helper used by `AgentSpec`'s `Deserialize` impl to apply the
/// legacy "absent catalog = allow all" default. Not part of the public API.
///
/// Mirrors `AgentSpec`'s fields exactly (names, types, `#[serde(...)]`
/// attributes). The `From` impl only modifies catalog fields; everything
/// else passes through. Adding or renaming any `AgentSpec` field requires
/// the corresponding update here AND in the `From` impl below — the
/// compiler enforces the latter via the struct literal.
///
/// The catalog fields use the "double-Option" pattern via `double_option`
/// so the `From` impl can tell apart three input states:
///   * field absent → `None` (legacy YAML compat — shim may fire)
///   * field present as JSON `null` → `Some(None)` (explicit user intent)
///   * field present as JSON array → `Some(Some(vec))` (explicit value)
///
/// Without this, `null` and absent both collapse to `None` and the
/// "absent catalog = allow all" shim silently flips a user-declared
/// deny-all (`{"allowed_tools": null, "allowed_tool_patterns": null}`)
/// into allow-all on the direct-PUT path.
// schemars::JsonSchema kept because AgentSpec's derive transitively
// requires it under #[serde(from = "AgentSpecRaw")].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentSpecRaw {
    id: String,
    model_id: String,
    system_prompt: String,
    #[serde(default = "default_max_rounds")]
    max_rounds: usize,
    #[serde(default = "default_max_continuation_retries")]
    max_continuation_retries: usize,
    #[serde(default)]
    context_policy: Option<ContextWindowPolicy>,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    plugin_ids: Vec<String>,
    #[serde(default, alias = "active_plugins")]
    active_hook_filter: HashSet<String>,
    // --- catalog fields (subject to migration; double-Option so the
    // `From` impl can distinguish absent from explicit-null) ---
    #[serde(default, deserialize_with = "double_option")]
    allowed_tools: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    allowed_tool_patterns: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    excluded_tools: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    excluded_tool_patterns: Option<Option<Vec<String>>>,
    // --- pass-through tail ---
    #[serde(default)]
    endpoint: Option<RemoteEndpoint>,
    #[serde(default)]
    delegates: Vec<String>,
    #[serde(default)]
    sections: HashMap<String, Value>,
    #[serde(default)]
    registry: Option<String>,
}

impl From<AgentSpecRaw> for AgentSpec {
    fn from(raw: AgentSpecRaw) -> Self {
        let (allowed_tools, allowed_tool_patterns) =
            inject_legacy_allow_default(raw.allowed_tools, raw.allowed_tool_patterns);
        // Excluded fields have no legacy shim — just collapse the
        // double-Option down to a single Option<Vec<_>>.
        let excluded_tools = raw.excluded_tools.flatten();
        let excluded_tool_patterns = raw.excluded_tool_patterns.flatten();
        AgentSpec {
            id: raw.id,
            model_id: raw.model_id,
            system_prompt: raw.system_prompt,
            max_rounds: raw.max_rounds,
            max_continuation_retries: raw.max_continuation_retries,
            context_policy: raw.context_policy,
            reasoning_effort: raw.reasoning_effort,
            plugin_ids: raw.plugin_ids,
            active_hook_filter: raw.active_hook_filter,
            allowed_tools,
            allowed_tool_patterns,
            excluded_tools,
            excluded_tool_patterns,
            endpoint: raw.endpoint,
            delegates: raw.delegates,
            sections: raw.sections,
            registry: raw.registry,
        }
    }
}

/// Double-Option deserialize helper: distinguishes a missing JSON key
/// from one explicitly set to `null`.
///
/// * key absent → `None` (combined with `#[serde(default)]`)
/// * key present as `null` → `Some(None)`
/// * key present as `T` → `Some(Some(T))`
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Apply the legacy "absent catalog = allow all" default.
///
/// The shim fires only when BOTH allow fields were truly absent from the
/// input (`(None, None)`). An explicit `null` on either field is now
/// observable here as `Some(None)` and is preserved as written — that's
/// the user expressing "no allow rules", which the matcher honours as
/// deny-all. Explicit lists likewise pass through unchanged.
///
/// Coupled to `Default::default` below — flip both together when the
/// legacy "absent = allow all" default is retired.
fn inject_legacy_allow_default(
    literals: Option<Option<Vec<String>>>,
    patterns: Option<Option<Vec<String>>>,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    match (literals, patterns) {
        (None, None) => (None, Some(vec!["*".to_string()])),
        (l, p) => (l.flatten(), p.flatten()),
    }
}

/// Serializable agent definition referencing registries by ID.
///
/// Can be saved to JSON, loaded from config files, or transmitted over the network.
/// Resolved at runtime via the resolve pipeline into a `ResolvedAgent`.
///
/// Also serves as the runtime behavior configuration passed to hooks via
/// `PhaseContext.agent_spec`. Plugins read their typed config via `spec.config::<K>()`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(from = "AgentSpecRaw")]
pub struct AgentSpec {
    /// Unique agent identifier.
    pub id: String,
    /// ModelRegistry ID — resolved to a `ModelSpec` carrying provider, upstream model, capabilities, and pricing.
    pub model_id: String,
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
    /// Literal tool IDs explicitly allowed. Never parsed as patterns; `*`
    /// here matches nothing (flagged by validation).
    /// `None` is equivalent to an empty list and means "no literal allow
    /// rules"; it does NOT mean "allow all" — see `allowed_tool_patterns`.
    ///
    /// Back-compat: when both `allowed_tools` and `allowed_tool_patterns`
    /// are absent in the input, a deserialize migration shim injects
    /// `allowed_tool_patterns = vec!["*"]` so existing configs allow all.
    ///
    /// Serialized as JSON `null` when `None` (no `skip_serializing_if`),
    /// so a `None` allow field survives a serialize→deserialize round
    /// trip as `Some(None)` at the raw level rather than collapsing to
    /// "absent" and re-firing the legacy shim.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Glob patterns matched against tool IDs (anchored, `*` is the only
    /// wildcard, `\` escapes). `["*"]` matches every tool. Union with
    /// `allowed_tools` forms the final allow set.
    ///
    /// Serialized as JSON `null` when `None` (no `skip_serializing_if`)
    /// for the same round-trip reason as `allowed_tools`.
    #[serde(default)]
    pub allowed_tool_patterns: Option<Vec<String>>,
    /// Literal tool IDs explicitly excluded. Applied after the allow set.
    /// `None` = "no literal exclude rules".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_tools: Option<Vec<String>>,
    /// Glob patterns matched against tool IDs for exclusion. Applied
    /// after the allow set; deny is always stronger than allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_tool_patterns: Option<Vec<String>>,
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
    ///
    /// Wrapped in [`crate::RedactedString`] so it does not leak through
    /// `Debug` / `Display`. The wire format remains a plain JSON string;
    /// empty-string input deserializes to `None`.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<crate::RedactedString>,
    /// Base URL override for proxy or self-hosted deployments. Empty-string
    /// input deserializes to `None`.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_url: Option<String>,
    /// Request timeout in seconds.
    #[serde(default = "default_provider_timeout_secs")]
    pub timeout_secs: u64,
    /// Adapter-specific non-secret options consumed by
    /// `build_genai_provider_executor` (for example
    /// `{"headers": {"OpenAI-Organization": "org-xxx"}}`).
    ///
    /// Secrets must use [`ProviderSpec::api_key`]; do not store credentials
    /// here. Unrecognised keys are accepted by the schema but ignored at
    /// build time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adapter_options: BTreeMap<String, Value>,
}

/// Treat an absent field, JSON `null`, or `""` as `None`. Used by spec types
/// that accept optional textual configuration so callers do not have to
/// strip/convert empty values themselves.
fn deserialize_optional_non_empty<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: From<String>,
{
    Ok(Option::<String>::deserialize(deserializer)?
        .filter(|value| !value.is_empty())
        .map(T::from))
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
            adapter_options: BTreeMap::new(),
        }
    }
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            model_id: String::new(),
            system_prompt: String::new(),
            max_rounds: default_max_rounds(),
            max_continuation_retries: default_max_continuation_retries(),
            context_policy: None,
            reasoning_effort: None,
            plugin_ids: Vec::new(),
            active_hook_filter: HashSet::new(),
            allowed_tools: None,
            // Mirror the deserialize migration shim: a spec with no
            // explicit catalog input allows every tool. Without this,
            // `AgentSpec::new` / `..Default::default()` would silently
            // produce a no-tools agent under the new catalog semantics
            // (empty allow set blocks all). The shim is the contract for
            // legacy configs; `Default` must match it so the three
            // construction paths (JSON deserialize, `Default`, builder)
            // agree.
            //
            // Coupled to `inject_legacy_allow_default` above — flip both
            // together when the legacy "absent = allow all" default is
            // retired.
            allowed_tool_patterns: Some(vec!["*".into()]),
            excluded_tools: None,
            excluded_tool_patterns: None,
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
    ///     .with_model_id("gpt-4o-mini")
    ///     .with_system_prompt("You are helpful.")
    ///     .with_max_rounds(10);
    /// assert_eq!(spec.id, "assistant");
    /// assert_eq!(spec.model_id, "gpt-4o-mini");
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
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = model_id.into();
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

mod catalog_match;
mod catalog_validation;
mod model_spec;
pub use catalog_validation::{IssueSeverity, ValidationIssue};
pub use model_spec::{Modalities, Modality, ModelSpec};

#[cfg(test)]
mod tests;
