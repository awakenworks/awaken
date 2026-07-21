//! Managed Agent wire-object mapping for the configuration HTTP adapter.
//!
//! The domain service owns [`AgentConfig`]; this module is the single seam that
//! accepts and projects the SDK-compatible JSON shape plus Awaken extensions.

use awaken_config_store::{AgentConfig, ModelSelection, ToolOverride};
use serde_json::{Value, json};

fn managed_tool_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(object) => object
            .get("id")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Parse a managed-shaped Agent object into the domain compile input.
pub(crate) fn agent_config_from_managed(id: String, body: &Value) -> Result<AgentConfig, String> {
    let string = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    let model_ref = match body.get("model") {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Object(object)) => object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    let array = |key: &str| {
        body.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let context_policy = match body.get("context_policy").cloned() {
        Some(value) => serde_json::from_value(value).map_err(|error| error.to_string())?,
        None => Default::default(),
    };
    let tool_overrides: Vec<ToolOverride> = match body.get("tool_overrides").cloned() {
        Some(value) => serde_json::from_value(value).map_err(|error| error.to_string())?,
        None => Vec::new(),
    };
    let metadata = body
        .get("metadata")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AgentConfig {
        id,
        instructions: string("system").unwrap_or_default(),
        max_steps: body.get("max_steps").and_then(Value::as_u64).unwrap_or(8) as usize,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("", model_ref, ""),
        tool_ids: array("tools").iter().filter_map(managed_tool_id).collect(),
        plugin_ids: array("plugins")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect(),
        plugin_config: body
            .get("plugin_config")
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        context_policy,
        tool_patterns: Vec::new(),
        model_candidates: Vec::new(),
        name: string("name"),
        description: string("description"),
        metadata,
        mcp_servers: array("mcp_servers"),
        skills: array("skills"),
        multiagent: body.get("multiagent").filter(|v| !v.is_null()).cloned(),
        tool_overrides,
        recovery_policies: body
            .get("recovery_policies")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default(),
        compaction: body
            .get("compaction")
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    })
}

/// Project a stored config into the managed-shaped object and its live state.
pub(crate) fn managed_from_agent_config(config: &AgentConfig, published: bool) -> Value {
    json!({
        "id": config.id,
        "type": "agent",
        "name": config.name,
        "description": config.description,
        "model": { "id": config.model_binding.resolved().map(|binding| binding.model_ref.clone()).unwrap_or_default() },
        "system": config.instructions,
        "metadata": config.metadata,
        "tools": config.tool_ids,
        "recovery_policies": config.recovery_policies,
        "mcp_servers": config.mcp_servers,
        "skills": config.skills,
        "multiagent": config.multiagent,
        "max_steps": config.max_steps,
        "plugins": config.plugin_ids,
        "plugin_config": config.plugin_config,
        "context_policy": config.context_policy,
        "tool_overrides": config.tool_overrides,
        "compaction": config.compaction,
        "published": published,
    })
}
