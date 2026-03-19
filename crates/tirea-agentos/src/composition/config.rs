use super::{
    A2aAgentBinding, AgentDefinition, AgentDefinitionSpec, AgentDescriptor, ModelDefinition,
    RemoteSecurityConfig, StopConditionSpec, ToolExecutionMode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[cfg(feature = "skills")]
#[derive(Debug, Clone)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub advertise_catalog: bool,
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

#[cfg(feature = "skills")]
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentToolsConfig {
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

impl Default for AgentToolsConfig {
    fn default() -> Self {
        Self {
            discovery_max_entries: 64,
            discovery_max_chars: 16 * 1024,
        }
    }
}

/// Authentication method for a declared provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderAuthConfig {
    /// Read the API key from an environment variable.
    Env { name: String },
    /// Use a literal token value.
    Token { value: String },
}

/// A provider endpoint declaration in agent config JSON.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderConfig {
    pub endpoint: String,
    #[serde(default)]
    pub auth: Option<ProviderAuthConfig>,
}

impl ProviderConfig {
    /// Build a `genai::Client` configured for this provider.
    pub fn into_client(&self, provider_id: &str) -> Result<genai::Client, AgentConfigError> {
        let endpoint =
            normalize_required_field(Some(provider_id), "endpoint", self.endpoint.clone())?;
        let auth = match &self.auth {
            None => genai::resolver::AuthData::None,
            Some(ProviderAuthConfig::Env { name }) => {
                let name = normalize_required_field(Some(provider_id), "auth.name", name.clone())?;
                genai::resolver::AuthData::from_env(name)
            }
            Some(ProviderAuthConfig::Token { value }) => {
                let value =
                    normalize_required_field(Some(provider_id), "auth.value", value.clone())?;
                genai::resolver::AuthData::from_single(value)
            }
        };
        let client = genai::Client::builder()
            .with_service_target_resolver_fn(move |mut t: genai::ServiceTarget| {
                t.endpoint = genai::resolver::Endpoint::from_owned(&*endpoint);
                t.auth = auth.clone();
                Ok(t)
            })
            .build();
        Ok(client)
    }
}

/// Inference parameters exposed in agent config JSON.
///
/// Universal fields are strongly typed. Provider-specific fields (e.g.
/// `verbosity`, `service_tier`) go in `extra` and are merged into
/// `genai::chat::ChatOptions` via serde passthrough.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ChatOptionsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Provider-specific options passed through to `genai::chat::ChatOptions`.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ChatOptionsConfig {
    fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.top_p.is_none()
            && self.stop_sequences.is_empty()
            && self.reasoning_effort.is_none()
            && self.seed.is_none()
            && self.extra.is_empty()
    }

    /// Convert into `genai::chat::ChatOptions`.
    ///
    /// Strongly-typed fields are set directly, then `extra` entries are
    /// merged via JSON round-trip so any `ChatOptions` field recognized
    /// by the genai crate can be set from config.
    pub fn into_chat_options(
        &self,
        model_id: &str,
    ) -> Result<genai::chat::ChatOptions, AgentConfigError> {
        use genai::chat::ReasoningEffort;

        let mut opts = genai::chat::ChatOptions {
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            top_p: self.top_p,
            stop_sequences: self.stop_sequences.clone(),
            seed: self.seed,
            ..Default::default()
        };

        if let Some(ref v) = self.reasoning_effort {
            opts.reasoning_effort = Some(v.parse::<ReasoningEffort>().map_err(|_| {
                AgentConfigError::InvalidFieldValue {
                    context_id: model_id.to_string(),
                    field: "chat_options.reasoning_effort",
                    value: v.clone(),
                }
            })?);
        }

        // Merge provider-specific extras via JSON round-trip.
        if !self.extra.is_empty() {
            let mut base =
                serde_json::to_value(&opts).map_err(|e| AgentConfigError::ExtraMerge {
                    context_id: model_id.to_string(),
                    detail: e.to_string(),
                })?;
            if let serde_json::Value::Object(ref mut map) = base {
                for (k, v) in &self.extra {
                    map.insert(k.clone(), v.clone());
                }
            }
            opts = serde_json::from_value(base).map_err(|e| AgentConfigError::ExtraMerge {
                context_id: model_id.to_string(),
                detail: e.to_string(),
            })?;
        }

        Ok(opts)
    }
}

/// A model alias declaration in agent config JSON.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_options: Option<ChatOptionsConfig>,
}

impl ModelConfig {
    /// Convert into a [`ModelDefinition`] suitable for the model registry.
    pub fn into_definition(&self, model_id: &str) -> Result<ModelDefinition, AgentConfigError> {
        let provider = normalize_required_field(Some(model_id), "provider", self.provider.clone())?;
        let model = normalize_required_field(Some(model_id), "model", self.model.clone())?;
        let mut def = ModelDefinition::new(provider, model);
        if let Some(ref opts) = self.chat_options {
            if !opts.is_empty() {
                def = def.with_chat_options(opts.into_chat_options(model_id)?);
            }
        }
        Ok(def)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    pub agents: Vec<AgentConfigEntry>,
}

impl AgentConfig {
    pub fn from_json_str(raw: &str) -> Result<Self, AgentConfigError> {
        serde_json::from_str(raw).map_err(AgentConfigError::ParseJson)
    }

    pub fn parse_specs_json(raw: &str) -> Result<Vec<AgentDefinitionSpec>, AgentConfigError> {
        Self::from_json_str(raw)?.into_specs()
    }

    #[must_use]
    pub fn json_schema() -> schemars::Schema {
        schemars::schema_for!(Self)
    }

    pub fn into_specs(self) -> Result<Vec<AgentDefinitionSpec>, AgentConfigError> {
        let mut seen = HashSet::new();
        let mut specs = Vec::with_capacity(self.agents.len());
        for entry in self.agents {
            let spec = entry.into_spec()?;
            let id = spec.id().to_string();
            if !seen.insert(id.clone()) {
                return Err(AgentConfigError::DuplicateAgentId(id));
            }
            specs.push(spec);
        }
        Ok(specs)
    }

    /// Build `genai::Client` instances for every declared provider.
    pub fn into_provider_clients(
        &self,
    ) -> Result<HashMap<String, genai::Client>, AgentConfigError> {
        let mut clients = HashMap::with_capacity(self.providers.len());
        for (id, cfg) in &self.providers {
            let id = normalize_required_field(None, "provider id", id.clone())?;
            if clients.contains_key(&id) {
                return Err(AgentConfigError::DuplicateProviderId(id));
            }
            clients.insert(id.clone(), cfg.into_client(&id)?);
        }
        Ok(clients)
    }

    /// Build [`ModelDefinition`] instances for every declared model.
    pub fn into_model_definitions(
        &self,
    ) -> Result<HashMap<String, ModelDefinition>, AgentConfigError> {
        let mut defs = HashMap::with_capacity(self.models.len());
        for (id, cfg) in &self.models {
            let id = normalize_required_field(None, "model id", id.clone())?;
            if defs.contains_key(&id) {
                return Err(AgentConfigError::DuplicateModelId(id));
            }
            defs.insert(id.clone(), cfg.into_definition(&id)?);
        }
        Ok(defs)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AgentConfigEntry {
    Tagged(TaggedAgentConfigEntry),
    LegacyLocal(LocalAgentConfig),
}

impl AgentConfigEntry {
    pub fn into_spec(self) -> Result<AgentDefinitionSpec, AgentConfigError> {
        match self {
            Self::Tagged(TaggedAgentConfigEntry::Local(agent)) => agent.into_spec(),
            Self::Tagged(TaggedAgentConfigEntry::A2a(agent)) => agent.into_spec(),
            Self::LegacyLocal(agent) => agent.into_spec(),
        }
    }

    pub fn local_model(&self) -> Option<&str> {
        match self {
            Self::Tagged(TaggedAgentConfigEntry::Local(agent)) => agent.model.as_deref(),
            Self::Tagged(TaggedAgentConfigEntry::A2a(_)) => None,
            Self::LegacyLocal(agent) => agent.model.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaggedAgentConfigEntry {
    Local(LocalAgentConfig),
    A2a(A2aAgentConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocalAgentConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub tool_execution_mode: ToolExecutionModeConfig,
    #[serde(default)]
    pub behavior_ids: Vec<String>,
    #[serde(default)]
    pub stop_condition_specs: Vec<StopConditionSpec>,
}

impl LocalAgentConfig {
    pub fn into_spec(self) -> Result<AgentDefinitionSpec, AgentConfigError> {
        let id = normalize_required_field(None, "id", self.id)?;
        let name = normalize_optional_text(self.name);
        let description = normalize_optional_text(self.description).unwrap_or_default();
        let model = normalize_optional_field(&id, "model", self.model)?;
        let behavior_ids = normalize_identifier_list(&id, "behavior_ids", self.behavior_ids)?;

        let mut definition = AgentDefinition {
            id,
            system_prompt: self.system_prompt,
            ..Default::default()
        };
        if let Some(name) = name {
            definition = definition.with_name(name);
        }
        definition = definition.with_description(description);
        if let Some(model) = model {
            definition.model = model;
        }
        if let Some(max_rounds) = self.max_rounds {
            definition.max_rounds = max_rounds;
        }
        definition.tool_execution_mode = self.tool_execution_mode.into();
        definition.behavior_ids = behavior_ids;
        definition.stop_condition_specs = self.stop_condition_specs;
        Ok(AgentDefinitionSpec::local(definition))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct A2aAgentConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub endpoint: String,
    #[serde(default)]
    pub remote_agent_id: Option<String>,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub auth: Option<RemoteAuthConfig>,
}

impl A2aAgentConfig {
    pub fn into_spec(self) -> Result<AgentDefinitionSpec, AgentConfigError> {
        let id = normalize_required_field(None, "id", self.id)?;
        let endpoint = normalize_required_field(Some(&id), "endpoint", self.endpoint)?;
        let remote_agent_id =
            normalize_optional_field(&id, "remote_agent_id", self.remote_agent_id)?
                .unwrap_or_else(|| id.clone());
        let mut descriptor = AgentDescriptor::new(id.clone());
        if let Some(name) = normalize_optional_text(self.name) {
            descriptor = descriptor.with_name(name);
        }
        descriptor = descriptor
            .with_description(normalize_optional_text(self.description).unwrap_or_default());
        let mut binding = A2aAgentBinding::new(endpoint, remote_agent_id);
        if let Some(poll_interval_ms) = self.poll_interval_ms {
            binding = binding.with_poll_interval_ms(poll_interval_ms);
        }
        if let Some(auth) = self.auth {
            binding = binding.with_auth(auth.into_runtime_config(&id)?);
        }
        Ok(AgentDefinitionSpec::a2a(descriptor, binding))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteAuthConfig {
    BearerToken { token: String },
    Header { name: String, value: String },
}

impl RemoteAuthConfig {
    fn into_runtime_config(self, agent_id: &str) -> Result<RemoteSecurityConfig, AgentConfigError> {
        match self {
            Self::BearerToken { token } => Ok(RemoteSecurityConfig::BearerToken(
                normalize_required_field(Some(agent_id), "auth.token", token)?,
            )),
            Self::Header { name, value } => Ok(RemoteSecurityConfig::Header {
                name: normalize_required_field(Some(agent_id), "auth.name", name)?,
                value: normalize_required_field(Some(agent_id), "auth.value", value)?,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionModeConfig {
    Sequential,
    ParallelBatchApproval,
    #[default]
    ParallelStreaming,
}

impl From<ToolExecutionModeConfig> for ToolExecutionMode {
    fn from(value: ToolExecutionModeConfig) -> Self {
        match value {
            ToolExecutionModeConfig::Sequential => ToolExecutionMode::Sequential,
            ToolExecutionModeConfig::ParallelBatchApproval => {
                ToolExecutionMode::ParallelBatchApproval
            }
            ToolExecutionModeConfig::ParallelStreaming => ToolExecutionMode::ParallelStreaming,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentConfigError {
    #[error("failed to parse agent config JSON: {0}")]
    ParseJson(#[from] serde_json::Error),
    #[error("agent id already configured: {0}")]
    DuplicateAgentId(String),
    #[error("provider id already configured: {0}")]
    DuplicateProviderId(String),
    #[error("model id already configured: {0}")]
    DuplicateModelId(String),
    #[error("field '{field}' must not be blank")]
    BlankField { field: &'static str },
    #[error("agent '{agent_id}' field '{field}' must not be blank")]
    BlankAgentField {
        agent_id: String,
        field: &'static str,
    },
    #[error("'{context_id}' field '{field}' has invalid value: '{value}'")]
    InvalidFieldValue {
        context_id: String,
        field: &'static str,
        value: String,
    },
    #[error("'{context_id}' chat_options extra merge failed: {detail}")]
    ExtraMerge { context_id: String, detail: String },
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_required_field(
    agent_id: Option<&str>,
    field: &'static str,
    value: String,
) -> Result<String, AgentConfigError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(match agent_id {
            Some(agent_id) => AgentConfigError::BlankAgentField {
                agent_id: agent_id.to_string(),
                field,
            },
            None => AgentConfigError::BlankField { field },
        });
    }
    Ok(value)
}

fn normalize_optional_field(
    agent_id: &str,
    field: &'static str,
    value: Option<String>,
) -> Result<Option<String>, AgentConfigError> {
    match value {
        Some(value) => normalize_required_field(Some(agent_id), field, value).map(Some),
        None => Ok(None),
    }
}

fn normalize_identifier_list(
    agent_id: &str,
    field: &'static str,
    values: Vec<String>,
) -> Result<Vec<String>, AgentConfigError> {
    values
        .into_iter()
        .map(|value| normalize_required_field(Some(agent_id), field, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_local_and_tagged_a2a_into_specs() {
        let specs = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [
                    {
                        "id": "assistant",
                        "name": "Assistant",
                        "model": "gpt-4o-mini"
                    },
                    {
                        "kind": "a2a",
                        "id": "researcher",
                        "endpoint": "https://example.test/v1/a2a"
                    }
                ]
            })
            .to_string(),
        )
        .expect("config should parse");

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id(), "assistant");
        assert_eq!(specs[1].id(), "researcher");
    }

    #[test]
    fn rejects_duplicate_agent_ids_after_normalization() {
        let err = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [
                    { "id": "assistant" },
                    { "kind": "a2a", "id": "assistant", "endpoint": "https://example.test/v1/a2a" }
                ]
            })
            .to_string(),
        )
        .expect_err("duplicate ids should fail");

        assert!(matches!(err, AgentConfigError::DuplicateAgentId(id) if id == "assistant"));
    }

    #[test]
    fn rejects_blank_remote_endpoint_and_auth_header_values() {
        let err = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [{
                    "kind": "a2a",
                    "id": "researcher",
                    "endpoint": "   ",
                    "auth": { "kind": "header", "name": "X-Key", "value": "secret" }
                }]
            })
            .to_string(),
        )
        .expect_err("blank endpoint should fail");
        assert!(
            matches!(err, AgentConfigError::BlankAgentField { agent_id, field } if agent_id == "researcher" && field == "endpoint")
        );

        let err = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [{
                    "kind": "a2a",
                    "id": "researcher",
                    "endpoint": "https://example.test/v1/a2a",
                    "auth": { "kind": "header", "name": " ", "value": "secret" }
                }]
            })
            .to_string(),
        )
        .expect_err("blank auth header name should fail");
        assert!(
            matches!(err, AgentConfigError::BlankAgentField { agent_id, field } if agent_id == "researcher" && field == "auth.name")
        );
    }

    #[test]
    fn parses_multiple_agents_for_handoff() {
        let specs = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [
                    { "id": "coder", "model": "claude-opus" },
                    { "id": "fast", "model": "claude-haiku" }
                ]
            })
            .to_string(),
        )
        .expect("config should parse");

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id(), "coder");
        assert_eq!(specs[1].id(), "fast");
    }

    #[test]
    fn emits_json_schema_for_external_tooling() {
        let schema = AgentConfig::json_schema();
        let schema_json = serde_json::to_value(&schema).expect("schema should serialize");
        assert_eq!(schema_json["type"], serde_json::json!("object"));
        assert!(schema_json["properties"]["agents"].is_object());
        assert!(schema_json["properties"]["providers"].is_object());
        assert!(schema_json["properties"]["models"].is_object());
    }

    #[test]
    fn parses_config_with_providers_models_and_agents() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "oauth-proxy": { "endpoint": "http://127.0.0.1:10531/v1" },
                "openai": {
                    "endpoint": "https://api.openai.com/v1",
                    "auth": { "kind": "env", "name": "OPENAI_API_KEY" }
                }
            },
            "models": {
                "gpt-5": { "provider": "oauth-proxy", "model": "gpt-5.4" }
            },
            "agents": [
                { "id": "coder", "model": "gpt-5" }
            ]
        }))
        .expect("config should parse");

        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.agents.len(), 1);
    }

    #[test]
    fn parses_config_with_only_agents_backward_compat() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse without providers/models");

        assert!(cfg.providers.is_empty());
        assert!(cfg.models.is_empty());
        assert_eq!(cfg.agents.len(), 1);
    }

    #[test]
    fn rejects_blank_provider_endpoint() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "bad": { "endpoint": "   " }
            },
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse");

        let err = cfg
            .into_provider_clients()
            .expect_err("blank endpoint should fail");
        assert!(matches!(
            err,
            AgentConfigError::BlankAgentField { agent_id, field }
                if agent_id == "bad" && field == "endpoint"
        ));
    }

    #[test]
    fn rejects_blank_auth_env_name() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "bad": {
                    "endpoint": "http://localhost:8080",
                    "auth": { "kind": "env", "name": "  " }
                }
            },
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse");

        let err = cfg
            .into_provider_clients()
            .expect_err("blank auth env name should fail");
        assert!(matches!(
            err,
            AgentConfigError::BlankAgentField { agent_id, field }
                if agent_id == "bad" && field == "auth.name"
        ));
    }

    #[test]
    fn rejects_blank_auth_token_value() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "providers": {
                "bad": {
                    "endpoint": "http://localhost:8080",
                    "auth": { "kind": "token", "value": "" }
                }
            },
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse");

        let err = cfg
            .into_provider_clients()
            .expect_err("blank auth token value should fail");
        assert!(matches!(
            err,
            AgentConfigError::BlankAgentField { agent_id, field }
                if agent_id == "bad" && field == "auth.value"
        ));
    }

    #[test]
    fn rejects_blank_model_provider_or_model_name() {
        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "models": {
                "bad": { "provider": "  ", "model": "gpt-4" }
            },
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse");

        let err = cfg
            .into_model_definitions()
            .expect_err("blank model provider should fail");
        assert!(matches!(
            err,
            AgentConfigError::BlankAgentField { agent_id, field }
                if agent_id == "bad" && field == "provider"
        ));

        let cfg: AgentConfig = serde_json::from_value(serde_json::json!({
            "models": {
                "bad": { "provider": "openai", "model": "" }
            },
            "agents": [{ "id": "default" }]
        }))
        .expect("config should parse");

        let err = cfg
            .into_model_definitions()
            .expect_err("blank model name should fail");
        assert!(matches!(
            err,
            AgentConfigError::BlankAgentField { agent_id, field }
                if agent_id == "bad" && field == "model"
        ));
    }

    #[test]
    fn into_client_returns_genai_client() {
        let cfg = ProviderConfig {
            endpoint: "http://127.0.0.1:10531/v1".to_string(),
            auth: None,
        };
        let _client = cfg
            .into_client("test-proxy")
            .expect("should build a genai client");
    }

    #[test]
    fn into_client_with_token_auth() {
        let cfg = ProviderConfig {
            endpoint: "https://api.openai.com/v1".to_string(),
            auth: Some(ProviderAuthConfig::Token {
                value: "sk-test-key".to_string(),
            }),
        };
        let _client = cfg
            .into_client("openai")
            .expect("should build a genai client with token auth");
    }

    #[test]
    fn into_definition_returns_correct_model_definition() {
        let cfg = ModelConfig {
            provider: "my-proxy".to_string(),
            model: "gpt-5.4".to_string(),
            chat_options: None,
        };
        let def = cfg
            .into_definition("gpt-5")
            .expect("should build a model definition");
        assert_eq!(def.provider, "my-proxy");
        assert_eq!(def.model, "gpt-5.4");
        assert!(def.chat_options.is_none());
    }

    #[test]
    fn into_definition_with_chat_options() {
        let cfg = ModelConfig {
            provider: "openai".to_string(),
            model: "gpt-5.4".to_string(),
            chat_options: Some(ChatOptionsConfig {
                temperature: Some(0.7),
                max_tokens: Some(4096),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            }),
        };
        let def = cfg
            .into_definition("gpt-5")
            .expect("should build definition with chat options");
        let opts = def.chat_options.expect("chat_options should be set");
        assert_eq!(opts.temperature, Some(0.7));
        assert_eq!(opts.max_tokens, Some(4096));
        assert!(opts.reasoning_effort.is_some());
    }

    #[test]
    fn into_definition_without_chat_options_backward_compat() {
        let cfg: ModelConfig = serde_json::from_value(serde_json::json!({
            "provider": "proxy",
            "model": "gpt-4"
        }))
        .expect("should parse without chat_options");
        assert!(cfg.chat_options.is_none());
    }

    #[test]
    fn chat_options_parses_universal_fields() {
        let cfg: ModelConfig = serde_json::from_value(serde_json::json!({
            "provider": "proxy",
            "model": "gpt-5",
            "chat_options": {
                "temperature": 0.5,
                "max_tokens": 2048,
                "top_p": 0.9,
                "stop_sequences": ["END"],
                "reasoning_effort": "medium",
                "seed": 42
            }
        }))
        .expect("should parse chat_options");
        let opts = cfg
            .into_definition("test")
            .expect("should convert")
            .chat_options
            .expect("chat_options should be set");
        assert_eq!(opts.temperature, Some(0.5));
        assert_eq!(opts.max_tokens, Some(2048));
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(opts.stop_sequences, vec!["END".to_string()]);
        assert_eq!(opts.seed, Some(42));
        assert!(opts.reasoning_effort.is_some());
    }

    #[test]
    fn chat_options_extra_passes_through_provider_specific_fields() {
        let cfg: ModelConfig = serde_json::from_value(serde_json::json!({
            "provider": "proxy",
            "model": "gpt-5",
            "chat_options": {
                "temperature": 0.7,
                "extra": {
                    "verbosity": "Low",
                    "service_tier": "Flex"
                }
            }
        }))
        .expect("should parse chat_options with extra");
        let opts = cfg
            .into_definition("test")
            .expect("should convert with extra")
            .chat_options
            .expect("chat_options should be set");
        assert_eq!(opts.temperature, Some(0.7));
        assert!(opts.verbosity.is_some());
        assert!(opts.service_tier.is_some());
    }

    #[test]
    fn chat_options_extra_invalid_field_returns_error() {
        let cfg = ModelConfig {
            provider: "proxy".to_string(),
            model: "gpt-5".to_string(),
            chat_options: Some(ChatOptionsConfig {
                extra: {
                    let mut m = serde_json::Map::new();
                    m.insert(
                        "verbosity".to_string(),
                        serde_json::json!("not-a-valid-variant"),
                    );
                    m
                },
                ..Default::default()
            }),
        };
        let err = cfg
            .into_definition("test")
            .expect_err("invalid extra value should fail");
        assert!(matches!(err, AgentConfigError::ExtraMerge { .. }));
    }

    #[test]
    fn rejects_invalid_reasoning_effort() {
        let cfg = ModelConfig {
            provider: "proxy".to_string(),
            model: "gpt-5".to_string(),
            chat_options: Some(ChatOptionsConfig {
                reasoning_effort: Some("turbo".to_string()),
                ..Default::default()
            }),
        };
        let err = cfg
            .into_definition("test")
            .expect_err("invalid reasoning_effort should fail");
        assert!(matches!(
            err,
            AgentConfigError::InvalidFieldValue { field, .. }
                if field == "chat_options.reasoning_effort"
        ));
    }

    #[test]
    fn empty_chat_options_does_not_set_on_definition() {
        let cfg = ModelConfig {
            provider: "proxy".to_string(),
            model: "gpt-5".to_string(),
            chat_options: Some(ChatOptionsConfig::default()),
        };
        let def = cfg.into_definition("test").expect("should convert");
        assert!(def.chat_options.is_none());
    }
}
