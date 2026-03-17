use super::{
    A2aAgentBinding, AgentDefinition, AgentDefinitionSpec, AgentDescriptor, ModeOverlay,
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

/// Configuration for the plan mode extension.
#[cfg(feature = "plan")]
#[derive(Debug, Clone, Default)]
pub struct PlanConfig {
    /// Whether plan mode tools should be registered.
    pub enabled: bool,
}

/// Configuration for the mode switching extension.
#[cfg(feature = "mode")]
#[derive(Debug, Clone, Default)]
pub struct ModeConfig {
    /// Whether mode switching tools should be registered.
    pub enabled: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
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

/// JSON-deserializable mode overlay for agent config files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ModeOverlayConfig {
    /// Override model identifier.
    #[serde(default)]
    pub model: Option<String>,
    /// Override model fallbacks.
    #[serde(default)]
    pub model_fallbacks: Option<Vec<String>>,
    /// Override system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Override maximum rounds.
    #[serde(default)]
    pub max_rounds: Option<usize>,
}

impl ModeOverlayConfig {
    fn into_overlay(self, agent_id: &str) -> Result<ModeOverlay, AgentConfigError> {
        Ok(ModeOverlay {
            model: normalize_optional_field(agent_id, "mode.model", self.model)?,
            model_fallbacks: normalize_optional_list(
                agent_id,
                "mode.model_fallbacks",
                self.model_fallbacks,
            )?,
            system_prompt: self.system_prompt,
            max_rounds: self.max_rounds,
            ..Default::default()
        })
    }
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
    pub model_fallbacks: Option<Vec<String>>,
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
    /// Named mode overlays for runtime switching.
    #[serde(default)]
    pub modes: HashMap<String, ModeOverlayConfig>,
}

impl LocalAgentConfig {
    pub fn into_spec(self) -> Result<AgentDefinitionSpec, AgentConfigError> {
        let id = normalize_required_field(None, "id", self.id)?;
        let name = normalize_optional_text(self.name);
        let description = normalize_optional_text(self.description).unwrap_or_default();
        let model = normalize_optional_field(&id, "model", self.model)?;
        let model_fallbacks =
            normalize_optional_list(&id, "model_fallbacks", self.model_fallbacks)?
                .unwrap_or_default();
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
        definition.model_fallbacks = model_fallbacks;
        if let Some(max_rounds) = self.max_rounds {
            definition.max_rounds = max_rounds;
        }
        definition.tool_execution_mode = self.tool_execution_mode.into();
        definition.behavior_ids = behavior_ids;
        definition.stop_condition_specs = self.stop_condition_specs;
        for (mode_name, mode_config) in self.modes {
            let overlay = mode_config.into_overlay(&definition.id)?;
            definition = definition.with_mode(mode_name, overlay);
        }
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
    #[error("field '{field}' must not be blank")]
    BlankField { field: &'static str },
    #[error("agent '{agent_id}' field '{field}' must not be blank")]
    BlankAgentField {
        agent_id: String,
        field: &'static str,
    },
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

fn normalize_optional_list(
    agent_id: &str,
    field: &'static str,
    values: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, AgentConfigError> {
    match values {
        Some(values) => normalize_identifier_list(agent_id, field, values).map(Some),
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
    fn parses_agent_with_modes() {
        let specs = AgentConfig::parse_specs_json(
            &serde_json::json!({
                "agents": [{
                    "id": "coder",
                    "model": "claude-opus",
                    "model_fallbacks": ["claude-instant"],
                    "modes": {
                        "fast": {
                            "model": "claude-haiku",
                            "model_fallbacks": ["claude-sonnet"]
                        },
                        "deep": { "model": "claude-opus", "max_rounds": 50 }
                    }
                }]
            })
            .to_string(),
        )
        .expect("config should parse");

        assert_eq!(specs.len(), 1);
        let spec = &specs[0];
        assert_eq!(spec.id(), "coder");
        if let super::super::AgentDefinitionSpec::Local(def) = spec {
            assert_eq!(def.modes.len(), 2);
            assert_eq!(def.model_fallbacks, vec!["claude-instant".to_string()]);
            assert_eq!(def.modes["fast"].model.as_deref(), Some("claude-haiku"));
            assert_eq!(
                def.modes["fast"].model_fallbacks.as_ref(),
                Some(&vec!["claude-sonnet".to_string()])
            );
            assert_eq!(def.modes["deep"].max_rounds, Some(50));
        } else {
            panic!("expected local agent spec");
        }
    }

    #[test]
    fn emits_json_schema_for_external_tooling() {
        let schema = AgentConfig::json_schema();
        let schema_json = serde_json::to_value(&schema).expect("schema should serialize");
        assert_eq!(schema_json["type"], serde_json::json!("object"));
        assert!(schema_json["properties"]["agents"].is_object());
    }
}
