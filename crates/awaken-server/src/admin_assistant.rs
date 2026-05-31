use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::registry::RegistrySet;
use awaken_runtime::registry::memory::MapToolRegistry;
use awaken_runtime::registry::{ModelRegistry, ProviderRegistry, ToolRegistry};
use awaken_server_contract::AuditAction;
use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_server_contract::{AgentSpec, ConfigRecord, RecordMeta};
use axum::http::HeaderMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{ConfigModuleState, ConfigRoutesState};
use crate::services::config_service::{ConfigNamespace, ConfigService, ConfigServiceError};

pub(crate) const ADMIN_ASSISTANT_AGENT_ID: &str = "__admin_assistant";
pub(crate) const ADMIN_ASSISTANT_CONFIG_NAMESPACE: &str = "admin-assistant";
pub(crate) const ADMIN_ASSISTANT_CONFIG_ID: &str = "default";
const ADMIN_TOOL_CATEGORY: &str = "admin_assistant";

const TOOL_PLATFORM_CAPABILITIES: &str = "admin_get_platform_capabilities";
const TOOL_CREATE_AGENT_DRAFT: &str = "admin_create_agent_draft";
const TOOL_VALIDATE_AGENT: &str = "admin_validate_agent";
pub(crate) const ADMIN_ASSISTANT_POLICY_PROMPT_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct AdminAssistantConfig {
    pub id: String,
    #[serde(default)]
    pub policy_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

impl Default for AdminAssistantConfig {
    fn default() -> Self {
        Self {
            id: ADMIN_ASSISTANT_CONFIG_ID.to_string(),
            policy_prompt: String::new(),
            model_id: None,
            revision: Some(0),
        }
    }
}

pub(crate) fn admin_assistant_tools_metadata() -> Vec<Value> {
    vec![
        admin_tool_metadata(
            TOOL_PLATFORM_CAPABILITIES,
            "Read platform capabilities",
            "Returns the redacted, scope-aware platform capability snapshot used by the admin console.",
            false,
        ),
        admin_tool_metadata(
            TOOL_CREATE_AGENT_DRAFT,
            "Create agent draft",
            "Creates a draft AgentSpec from an operator intent without publishing it.",
            false,
        ),
        admin_tool_metadata(
            TOOL_VALIDATE_AGENT,
            "Validate agent draft",
            "Runs the same server-side AgentSpec validation as the config API without writing to storage.",
            false,
        ),
    ]
}

pub(crate) fn admin_assistant_capability(
    model_ids: &[String],
    selected_model_id: Option<String>,
) -> Value {
    let enabled = selected_model_id.is_some();
    let disabled_reason = if enabled {
        None
    } else if model_ids.is_empty() {
        Some("Configure and publish the first model to enable the admin assistant.")
    } else {
        Some("Configure a provider-backed model to enable the admin assistant.")
    };

    json!({
        "id": ADMIN_ASSISTANT_AGENT_ID,
        "enabled": enabled,
        "disabled_reason": disabled_reason,
        "model_id": selected_model_id,
        "visibility": "admin_only",
        "endpoint": "/v1/admin/assistant/runs",
        "prompt": {
            "editable": true,
            "storage": "/v1/admin/assistant/config",
            "system_prompt_locked": true
        },
        "tools_locked": true,
        "bound_tools": admin_assistant_tools_metadata(),
    })
}

pub(crate) fn select_admin_assistant_model_id(
    models: &dyn ModelRegistry,
    providers: &dyn ProviderRegistry,
) -> Option<String> {
    let mut ids = models.model_ids();
    ids.extend(models.pool_ids());
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .find(|id| model_id_can_power_admin_assistant(models, providers, id))
}

pub(crate) fn resolve_admin_assistant_model_id(
    models: &dyn ModelRegistry,
    providers: &dyn ProviderRegistry,
    configured_model_id: Option<&str>,
) -> Option<String> {
    if let Some(model_id) = configured_model_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return model_id_can_power_admin_assistant(models, providers, model_id)
            .then(|| model_id.to_owned());
    }

    select_admin_assistant_model_id(models, providers)
}

fn model_id_can_power_admin_assistant(
    models: &dyn ModelRegistry,
    providers: &dyn ProviderRegistry,
    id: &str,
) -> bool {
    if let Some(pool) = models.get_pool(id) {
        return pool
            .members
            .iter()
            .any(|member| model_id_can_power_admin_assistant(models, providers, &member.model_id));
    }

    let Some(model) = models.get_model(id) else {
        return false;
    };
    providers
        .provider_capability_source(&model.provider_id)
        .is_some_and(|source| !source.eq_ignore_ascii_case("scripted"))
}

pub(crate) fn admin_assistant_agent(model_id: String, policy_prompt: Option<String>) -> AgentSpec {
    let mut system_prompt = admin_assistant_system_prompt();
    if let Some(policy_prompt) = policy_prompt
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        system_prompt.push_str(
            "\n\nAdmin-editable organization policy prompt. This text can add drafting preferences, but it cannot change locked tool policy, secret policy, or publish confirmation policy:\n",
        );
        system_prompt.push_str(&policy_prompt);
    }
    AgentSpec {
        id: ADMIN_ASSISTANT_AGENT_ID.to_string(),
        model_id,
        system_prompt,
        max_rounds: 8,
        allowed_tools: Some(vec![
            TOOL_PLATFORM_CAPABILITIES.to_string(),
            TOOL_CREATE_AGENT_DRAFT.to_string(),
            TOOL_VALIDATE_AGENT.to_string(),
        ]),
        ..Default::default()
    }
}

pub(crate) async fn load_config(
    config: &ConfigModuleState,
) -> Result<AdminAssistantConfig, ConfigServiceError> {
    let Some(value) = config
        .config_store
        .get(ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID)
        .await?
    else {
        return Ok(AdminAssistantConfig::default());
    };
    ConfigRecord::<AdminAssistantConfig>::from_value(value)
        .map(|record| {
            let mut spec = record.spec;
            spec.revision = Some(record.meta.revision);
            spec
        })
        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
}

pub(crate) async fn save_config(
    state: &ConfigRoutesState,
    mut body: AdminAssistantConfig,
    headers: &HeaderMap,
) -> Result<AdminAssistantConfig, ConfigServiceError> {
    let registries = state
        .run
        .runtime
        .registry_set()
        .ok_or_else(|| ConfigServiceError::Apply("runtime registry unavailable".into()))?;
    validate_config(&registries, &mut body)?;
    body.id = ADMIN_ASSISTANT_CONFIG_ID.to_string();
    let expected_revision = body.revision.unwrap_or(0);
    let mut record = ConfigRecord {
        spec: body.clone(),
        meta: RecordMeta::new_user(),
    };
    let previous = state
        .config
        .config_store
        .get(ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID)
        .await?;
    if let Some(previous) = previous.as_ref()
        && let Ok(previous) = ConfigRecord::<AdminAssistantConfig>::from_value(previous.clone())
    {
        if previous.meta.revision != expected_revision {
            return Err(ConfigServiceError::Conflict(format!(
                "{}/{} was modified by another writer (expected revision {expected_revision}, found {}); retry the mutation",
                ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID, previous.meta.revision
            )));
        }
        record.meta.created_at = previous.meta.created_at;
    }
    record.meta.revision = expected_revision.saturating_add(1);
    record.spec.revision = Some(record.meta.revision);
    let value = serde_json::to_value(record)
        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))?;
    state
        .config
        .config_store
        .put_if_revision(
            ADMIN_ASSISTANT_CONFIG_NAMESPACE,
            ADMIN_ASSISTANT_CONFIG_ID,
            &value,
            expected_revision,
        )
        .await
        .map_err(admin_assistant_storage_error)?;
    body.revision = Some(expected_revision.saturating_add(1));
    if let Some(audit) = &state.config.audit_log {
        let action = if previous.is_some() {
            AuditAction::Update
        } else {
            AuditAction::Create
        };
        audit
            .emit(
                action,
                "admin-assistant/default",
                previous.map(|value| {
                    ConfigRecord::<Value>::from_value(value)
                        .map_or(Value::Null, |record| record.spec)
                }),
                Some(
                    serde_json::to_value(&body)
                        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))?,
                ),
                headers,
            )
            .await;
    }
    Ok(body)
}

fn validate_config(
    registries: &RegistrySet,
    body: &mut AdminAssistantConfig,
) -> Result<(), ConfigServiceError> {
    body.policy_prompt = body.policy_prompt.trim().to_string();
    if body.policy_prompt.len() > ADMIN_ASSISTANT_POLICY_PROMPT_MAX_BYTES {
        return Err(ConfigServiceError::InvalidPayload(format!(
            "policy_prompt exceeds {} bytes",
            ADMIN_ASSISTANT_POLICY_PROMPT_MAX_BYTES
        )));
    }
    body.model_id = body
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if let Some(model_id) = body.model_id.as_deref()
        && !model_id_can_power_admin_assistant(
            registries.models.as_ref(),
            registries.providers.as_ref(),
            model_id,
        )
    {
        return Err(ConfigServiceError::InvalidPayload(format!(
            "model_id '{model_id}' is not a provider-backed admin assistant model"
        )));
    }
    Ok(())
}

fn admin_assistant_storage_error(error: StorageError) -> ConfigServiceError {
    match error {
        StorageError::VersionConflict { expected, actual } => {
            ConfigServiceError::Conflict(format!(
                "{}/{} was modified by another writer (expected revision {expected}, found {actual}); retry the mutation",
                ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID
            ))
        }
        StorageError::AlreadyExists(_) => ConfigServiceError::Conflict(format!(
            "{}/{} already exists",
            ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID
        )),
        other => ConfigServiceError::Storage(other),
    }
}

pub(crate) fn admin_tool_registry(state: ConfigRoutesState) -> Arc<dyn ToolRegistry> {
    let mut registry = MapToolRegistry::new();
    registry
        .register_tool(
            TOOL_PLATFORM_CAPABILITIES,
            Arc::new(GetPlatformCapabilitiesTool {
                state: state.clone(),
            }),
        )
        .expect("fresh admin tool registry accepts platform capability tool");
    registry
        .register_tool(
            TOOL_CREATE_AGENT_DRAFT,
            Arc::new(CreateAgentDraftTool {
                state: state.clone(),
            }),
        )
        .expect("fresh admin tool registry accepts agent draft tool");
    registry
        .register_tool(TOOL_VALIDATE_AGENT, Arc::new(ValidateAgentTool { state }))
        .expect("fresh admin tool registry accepts validation tool");

    Arc::new(registry)
}

fn admin_tool_metadata(
    id: &str,
    label: &str,
    description: &str,
    requires_confirmation: bool,
) -> Value {
    json!({
        "id": id,
        "label": label,
        "description": description,
        "visibility": "admin_assistant_only",
        "selectable_by_agents": false,
        "exposable_to_protocols": false,
        "requires_confirmation": requires_confirmation,
    })
}

fn admin_assistant_system_prompt() -> String {
    [
        "You are the Awaken Admin Assistant.",
        "You are only available inside the authenticated admin console.",
        "Use admin_get_platform_capabilities before recommending concrete models, plugins, tools, MCP servers, skills, delegates, scopes, traces, datasets, or evals.",
        "Do not invent registry ids. If a requested capability is missing, say what must be configured first.",
        "When the operator asks you to create an agent, use admin_create_agent_draft and admin_validate_agent. Never publish or claim an agent has been published; final publish must happen through the Admin Console.",
        "Create minimal AgentSpecs that explain model choice, prompt intent, enabled plugins, allowed tools, MCP bindings, skills, delegates, scope, trace, dataset, and eval implications.",
        "Admin tools are locked by the server and are not assignable to user agents.",
        "Never request or reveal secrets. Treat provider keys, MCP credentials, and headers as redacted.",
    ]
    .join("\n")
}

struct GetPlatformCapabilitiesTool {
    state: ConfigRoutesState,
}

#[async_trait]
impl Tool for GetPlatformCapabilitiesTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TOOL_PLATFORM_CAPABILITIES,
            "Read platform capabilities",
            "Read the redacted admin capability snapshot: agents, models, providers, registry plugins, tools, MCP, skills, delegates, schemas, and admin-assistant constraints.",
        )
        .with_category(ADMIN_TOOL_CATEGORY)
        .with_parameters(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }

    async fn execute(&self, _args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let service = service_for_tool(&self.state)?;
        let capabilities = service
            .capabilities()
            .await
            .map_err(config_error_to_tool_error)?;
        Ok(ToolResult::success(TOOL_PLATFORM_CAPABILITIES, capabilities).into())
    }
}

struct CreateAgentDraftTool {
    state: ConfigRoutesState,
}

#[async_trait]
impl Tool for CreateAgentDraftTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TOOL_CREATE_AGENT_DRAFT,
            "Create agent draft",
            "Create a draft AgentSpec from an operator intent. This does not publish or write storage.",
        )
        .with_category(ADMIN_TOOL_CATEGORY)
        .with_parameters(agent_create_parameters_schema())
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let service = service_for_tool(&self.state)?;
        let draft = agent_draft_from_args(&service, &args).await?;
        match service
            .validate(ConfigNamespace::Agents, None, draft.clone())
            .await
        {
            Ok(normalized) => Ok(ToolResult::success(
                TOOL_CREATE_AGENT_DRAFT,
                json!({
                    "ok": true,
                    "draft": normalized,
                    "published": false,
                    "next_step": "Review the draft in the Agent editor, then publish it from the admin console.",
                }),
            )
            .into()),
            Err(error) => Ok(ToolResult::success(
                TOOL_CREATE_AGENT_DRAFT,
                json!({
                    "ok": false,
                    "published": false,
                    "draft": draft,
                    "errors": [error.to_string()],
                }),
            )
            .into()),
        }
    }
}

fn agent_create_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "intent": {
                "type": "string",
                "description": "The operator's goal for the agent."
            },
            "id": {
                "type": "string",
                "description": "Optional desired agent id."
            },
            "model_id": {
                "type": "string",
                "description": "Optional model id. Defaults to the Admin Assistant's configured model."
            },
            "system_prompt": {
                "type": "string",
                "description": "Optional system prompt. Defaults to a concise prompt derived from intent."
            },
            "plugin_ids": {
                "type": "array",
                "items": { "type": "string" }
            },
            "allowed_tools": {
                "type": "array",
                "items": { "type": "string" }
            },
            "delegates": {
                "type": "array",
                "items": { "type": "string" }
            }
        },
        "required": ["intent"],
        "additionalProperties": false
    })
}

async fn agent_draft_from_args(service: &ConfigService, args: &Value) -> Result<Value, ToolError> {
    let intent = arg_string(args, "intent")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArguments("intent is required".into()))?;
    let capabilities = service
        .capabilities()
        .await
        .map_err(config_error_to_tool_error)?;
    let model_id = arg_string(args, "model_id")
        .or_else(|| admin_assistant_model_id(&capabilities))
        .or_else(|| first_id(&capabilities["models"]))
        .ok_or_else(|| {
            ToolError::ExecutionFailed("no configured model is available for the agent".into())
        })?;
    let id = arg_string(args, "id").unwrap_or_else(|| draft_agent_id_from_intent(&intent));
    let system_prompt = arg_string(args, "system_prompt").unwrap_or_else(|| {
        format!(
            "You are an Awaken agent created for this operator intent: {intent}. Keep responses concise, use only configured platform capabilities, and explain when a requested capability is unavailable."
        )
    });
    let mut draft = json!({
        "id": id,
        "model_id": model_id,
        "system_prompt": system_prompt,
        "max_rounds": 8,
        "plugin_ids": string_array_arg(args, "plugin_ids"),
        "delegates": string_array_arg(args, "delegates"),
    });
    if let Some(allowed_tools) = optional_string_array_arg(args, "allowed_tools") {
        draft["allowed_tools"] = json!(allowed_tools);
    }
    Ok(draft)
}

struct ValidateAgentTool {
    state: ConfigRoutesState,
}

#[async_trait]
impl Tool for ValidateAgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TOOL_VALIDATE_AGENT,
            "Validate agent draft",
            "Validate a draft AgentSpec with the same server-side checks as POST /v1/config/agents/validate.",
        )
        .with_category(ADMIN_TOOL_CATEGORY)
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "object",
                    "description": "AgentSpec draft to validate."
                },
                "id": {
                    "type": "string",
                    "description": "Optional path id when validating an update."
                }
            },
            "required": ["agent"],
            "additionalProperties": false
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let agent = args
            .get("agent")
            .cloned()
            .ok_or_else(|| ToolError::InvalidArguments("agent is required".into()))?;
        let path_id = args.get("id").and_then(Value::as_str);
        let service = service_for_tool(&self.state)?;
        match service
            .validate(ConfigNamespace::Agents, path_id, agent)
            .await
        {
            Ok(normalized) => Ok(ToolResult::success(
                TOOL_VALIDATE_AGENT,
                json!({
                    "ok": true,
                    "normalized": normalized,
                    "warnings": [],
                }),
            )
            .into()),
            Err(error) => Ok(ToolResult::success(
                TOOL_VALIDATE_AGENT,
                json!({
                    "ok": false,
                    "errors": [error.to_string()],
                }),
            )
            .into()),
        }
    }
}

fn service_for_tool(state: &ConfigRoutesState) -> Result<ConfigService, ToolError> {
    ConfigService::new(state).map_err(config_error_to_tool_error)
}

fn config_error_to_tool_error(error: ConfigServiceError) -> ToolError {
    ToolError::ExecutionFailed(error.to_string())
}

fn arg_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn optional_string_array_arg(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|value| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
    })
}

fn string_array_arg(args: &Value, key: &str) -> Vec<String> {
    optional_string_array_arg(args, key).unwrap_or_default()
}

fn first_id(items: &Value) -> Option<String> {
    items
        .as_array()?
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .next()
}

fn admin_assistant_model_id(capabilities: &Value) -> Option<String> {
    capabilities
        .get("admin_assistant")?
        .get("model_id")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn draft_agent_id_from_intent(intent: &str) -> String {
    let mut id = String::from("agent");
    for token in intent
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| !token.is_empty())
        .take(4)
    {
        id.push('-');
        id.push_str(&token);
    }
    if id == "agent" {
        id.push_str("-draft");
    }
    let mut hasher = DefaultHasher::new();
    intent.hash(&mut hasher);
    id.push('-');
    id.push_str(&format!("{:08x}", hasher.finish() as u32));
    id
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_runtime::engine::MockLlmExecutor;
    use awaken_runtime::registry::memory::{MapModelRegistry, MapProviderRegistry};
    use awaken_server_contract::{ModelPoolSpec, ModelSpec};

    use super::{resolve_admin_assistant_model_id, select_admin_assistant_model_id};

    fn register_provider(providers: &mut MapProviderRegistry, id: &str, source: &str) {
        providers
            .register_provider_with_signature_and_capability_source(
                id,
                Arc::new(MockLlmExecutor::new()),
                format!("{source}-signature"),
                source,
            )
            .expect("test provider should register");
    }

    fn register_model(models: &mut MapModelRegistry, id: &str, provider_id: &str) {
        models
            .register_model(ModelSpec::new(id, provider_id, "upstream"))
            .expect("test model should register");
    }

    #[test]
    fn admin_assistant_auto_select_skips_scripted_models() {
        let mut models = MapModelRegistry::new();
        let mut providers = MapProviderRegistry::new();
        register_provider(&mut providers, "scripted-provider", "scripted");
        register_provider(&mut providers, "live-provider", "vertex");
        register_model(&mut models, "default", "scripted-provider");
        register_model(&mut models, "gemini-flash", "live-provider");

        assert_eq!(
            select_admin_assistant_model_id(&models, &providers).as_deref(),
            Some("gemini-flash")
        );
    }

    #[test]
    fn admin_assistant_auto_selects_pool_with_live_member() {
        let mut models = MapModelRegistry::new();
        let mut providers = MapProviderRegistry::new();
        register_provider(&mut providers, "scripted-provider", "scripted");
        register_provider(&mut providers, "live-provider", "openai");
        register_model(&mut models, "scripted-model", "scripted-provider");
        register_model(&mut models, "live-model", "live-provider");
        models
            .register_model_pool(ModelPoolSpec::new(
                "assistant-pool",
                ["scripted-model", "live-model"],
            ))
            .expect("test pool should register");

        assert_eq!(
            select_admin_assistant_model_id(&models, &providers).as_deref(),
            Some("assistant-pool")
        );
    }

    #[test]
    fn admin_assistant_rejects_models_without_provider_capability_source() {
        let mut models = MapModelRegistry::new();
        let mut providers = MapProviderRegistry::new();
        providers
            .register_provider("unmarked-provider", Arc::new(MockLlmExecutor::new()))
            .expect("provider should register");
        register_model(&mut models, "unmarked-model", "unmarked-provider");

        assert_eq!(
            select_admin_assistant_model_id(&models, &providers).as_deref(),
            None
        );
        assert_eq!(
            resolve_admin_assistant_model_id(&models, &providers, Some("unmarked-model")),
            None
        );
    }

    #[test]
    fn configured_admin_assistant_model_does_not_fallback_when_invalid() {
        let mut models = MapModelRegistry::new();
        let mut providers = MapProviderRegistry::new();
        register_provider(&mut providers, "scripted-provider", "scripted");
        register_provider(&mut providers, "live-provider", "openai");
        register_model(&mut models, "scripted-model", "scripted-provider");
        register_model(&mut models, "live-model", "live-provider");

        assert_eq!(
            resolve_admin_assistant_model_id(&models, &providers, Some("scripted-model")),
            None
        );
        assert_eq!(
            resolve_admin_assistant_model_id(&models, &providers, Some("missing-model")),
            None
        );
    }

    #[test]
    fn admin_assistant_rejects_pool_with_only_scripted_members() {
        let mut models = MapModelRegistry::new();
        let mut providers = MapProviderRegistry::new();
        register_provider(&mut providers, "scripted-provider", "scripted");
        register_model(&mut models, "scripted-model", "scripted-provider");
        models
            .register_model_pool(ModelPoolSpec::new("scripted-pool", ["scripted-model"]))
            .expect("test pool should register");

        assert_eq!(
            select_admin_assistant_model_id(&models, &providers).as_deref(),
            None
        );
        assert_eq!(
            resolve_admin_assistant_model_id(&models, &providers, Some("scripted-pool")),
            None
        );
    }
}
