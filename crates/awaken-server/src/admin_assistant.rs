use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime::registry::memory::MapToolRegistry;
use awaken_runtime::registry::{ModelRegistry, ToolRegistry};
use awaken_server_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken_server_contract::{AgentSpec, ConfigRecord, RecordMeta};
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct AdminAssistantConfig {
    pub id: String,
    #[serde(default)]
    pub policy_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

impl Default for AdminAssistantConfig {
    fn default() -> Self {
        Self {
            id: ADMIN_ASSISTANT_CONFIG_ID.to_string(),
            policy_prompt: String::new(),
            model_id: None,
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
        Some("No usable admin assistant model is available.")
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

pub(crate) fn select_admin_assistant_model_id(models: &dyn ModelRegistry) -> Option<String> {
    let mut ids = models.model_ids();
    ids.sort();
    ids.into_iter()
        .find(|id| models.get_model(id).is_some() || models.get_pool(id).is_some())
}

pub(crate) fn admin_assistant_agent(model_id: String, policy_prompt: Option<String>) -> AgentSpec {
    let mut system_prompt = admin_assistant_system_prompt();
    if let Some(policy_prompt) = policy_prompt
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        system_prompt.push_str("\n\nAdmin-editable policy prompt:\n");
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
        .map(|record| record.spec)
        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
}

pub(crate) async fn save_config(
    config: &ConfigModuleState,
    mut body: AdminAssistantConfig,
) -> Result<AdminAssistantConfig, ConfigServiceError> {
    body.id = ADMIN_ASSISTANT_CONFIG_ID.to_string();
    let mut record = ConfigRecord {
        spec: body.clone(),
        meta: RecordMeta::new_user(),
    };
    if let Some(previous) = config
        .config_store
        .get(ADMIN_ASSISTANT_CONFIG_NAMESPACE, ADMIN_ASSISTANT_CONFIG_ID)
        .await?
        && let Ok(previous) = ConfigRecord::<AdminAssistantConfig>::from_value(previous)
    {
        record.meta.created_at = previous.meta.created_at;
    }
    let value = serde_json::to_value(record)
        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))?;
    config
        .config_store
        .put(
            ADMIN_ASSISTANT_CONFIG_NAMESPACE,
            ADMIN_ASSISTANT_CONFIG_ID,
            &value,
        )
        .await?;
    Ok(body)
}

pub(crate) fn admin_tool_overlay(
    fallback: Arc<dyn ToolRegistry>,
    state: ConfigRoutesState,
) -> Arc<dyn ToolRegistry> {
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

    Arc::new(OverlayToolRegistry {
        admin: Arc::new(registry),
        fallback,
    })
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
        "Create minimal AgentSpec drafts that explain model choice, prompt intent, enabled plugins, allowed tools, MCP bindings, skills, delegates, scope, trace, dataset, and eval implications.",
        "Admin tools are locked by the server and are not assignable to user agents.",
        "Never request or reveal secrets. Treat provider keys, MCP credentials, and headers as redacted.",
    ]
    .join("\n")
}

struct OverlayToolRegistry {
    admin: Arc<dyn ToolRegistry>,
    fallback: Arc<dyn ToolRegistry>,
}

impl ToolRegistry for OverlayToolRegistry {
    fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.admin
            .get_tool(id)
            .or_else(|| self.fallback.get_tool(id))
    }

    fn tool_ids(&self) -> Vec<String> {
        let mut ids = self.fallback.tool_ids();
        for id in self.admin.tool_ids() {
            if !ids.iter().any(|existing| existing == &id) {
                ids.push(id);
            }
        }
        ids
    }
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
        .with_parameters(json!({
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
                    "description": "Optional model id. Defaults to the first configured model."
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
        }))
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let intent = arg_string(&args, "intent")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("intent is required".into()))?;
        let service = service_for_tool(&self.state)?;
        let capabilities = service
            .capabilities()
            .await
            .map_err(config_error_to_tool_error)?;
        let model_id = arg_string(&args, "model_id")
            .or_else(|| first_id(&capabilities["models"]))
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "no configured model is available for the draft agent".into(),
                )
            })?;
        let id = arg_string(&args, "id").unwrap_or_else(|| draft_agent_id_from_intent(&intent));
        let system_prompt = arg_string(&args, "system_prompt").unwrap_or_else(|| {
            format!(
                "You are an Awaken agent created for this operator intent: {intent}. Keep responses concise, use only configured platform capabilities, and explain when a requested capability is unavailable."
            )
        });
        let mut draft = json!({
            "id": id,
            "model_id": model_id,
            "system_prompt": system_prompt,
            "max_rounds": 8,
            "plugin_ids": string_array_arg(&args, "plugin_ids"),
            "delegates": string_array_arg(&args, "delegates"),
        });
        if let Some(allowed_tools) = optional_string_array_arg(&args, "allowed_tools") {
            draft["allowed_tools"] = json!(allowed_tools);
        }
        let normalized = service
            .validate(ConfigNamespace::Agents, None, draft.clone())
            .await
            .unwrap_or(draft);
        Ok(ToolResult::success(
            TOOL_CREATE_AGENT_DRAFT,
            json!({
                "draft": normalized,
                "published": false,
                "next_step": "Review the draft in the Agent editor, then publish it from the admin console.",
            }),
        )
        .into())
    }
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
    id
}
