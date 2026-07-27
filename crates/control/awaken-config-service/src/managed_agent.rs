//! Managed Agent wire projection for the config authoring aggregate.
//!
//! This is the sole translation seam between the SDK-shaped Agent object and
//! [`AgentConfig`]. Keeping it separate from CRUD/publication prevents protocol
//! projection details from growing the config-plane orchestration module.

use awaken_config_store::{AgentConfig, ModelSelection, MultiagentConfig, ToolOverride};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::resolved::ToolDescriptor;
use serde_json::{Value, json};

fn managed_tool_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(object) if object.get("type").and_then(Value::as_str) != Some("custom") => {
            object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        _ => None,
    }
}

fn managed_client_tool(value: &Value) -> Result<Option<ToolDescriptor>, String> {
    if value.get("type").and_then(Value::as_str) != Some("custom") {
        return Ok(None);
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "custom tool requires a non-empty name".to_string())?;
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("custom tool {name:?} requires description"))?;
    let schema = value
        .get("input_schema")
        .cloned()
        .ok_or_else(|| format!("custom tool {name:?} requires input_schema"))?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "custom tool {name:?} input_schema must be an object schema"
        ));
    }
    Ok(Some(ToolDescriptor::client_executed(
        name,
        description,
        schema,
    )))
}

fn managed_toolset(value: &Value) -> Result<Option<awaken_session_contract::AgentTool>, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("agent_toolset_20260401" | "mcp_toolset") => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| error.to_string()),
        _ => Ok(None),
    }
}

/// Parse a managed-shaped Agent object into the domain compile input.
pub fn agent_config_from_managed(id: String, body: &Value) -> Result<AgentConfig, String> {
    let string = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    let (provider_identity_ref, model_ref, backend_ref) = match body.get("model") {
        Some(Value::String(model)) => (String::new(), model.clone(), String::new()),
        Some(Value::Object(model)) => (
            model
                .get("provider_identity_ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            model
                .get("model_ref")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            model
                .get("backend_ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => (String::new(), String::new(), String::new()),
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
    let mcp_servers = array("mcp_servers")
        .into_iter()
        .map(|value| {
            serde_json::from_value::<AgentMcpServerBinding>(value)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let skill_ids = array("skills")
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .ok_or_else(|| "Skill must be a non-empty id or object with `id`".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let multiagent = body
        .get("multiagent")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value::<MultiagentConfig>)
        .transpose()
        .map_err(|error| format!("invalid multiagent roster: {error}"))?;
    let tools = array("tools");
    let client_tools = tools
        .iter()
        .map(managed_client_tool)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    let authored_toolsets = tools
        .iter()
        .map(managed_toolset)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let model_binding = if body
        .get("model")
        .and_then(|model| model.get("mode"))
        .and_then(Value::as_str)
        == Some("auto")
    {
        ModelSelection::Auto
    } else {
        ModelSelection::pinned(provider_identity_ref, model_ref, backend_ref)
    };
    Ok(AgentConfig {
        id,
        instructions: string("system").unwrap_or_default(),
        max_steps: body.get("max_steps").and_then(Value::as_u64).unwrap_or(8) as usize,
        delegation_limits: Default::default(),
        model_binding,
        inference: Default::default(),
        tool_ids: tools.iter().filter_map(managed_tool_id).collect(),
        toolsets: awaken_session_contract::toolset_policies(&authored_toolsets),
        client_tools,
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
        mcp_servers,
        skill_ids,
        multiagent,
        archived_at: string("archived_at"),
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
pub fn managed_from_agent_config(config: &AgentConfig, published: bool) -> Value {
    let binding = config.model_binding.resolved();
    let mut tools = config
        .tool_ids
        .iter()
        .map(|id| Value::String(id.clone()))
        .collect::<Vec<_>>();
    tools.extend(
        awaken_session_contract::resolved_toolsets(&config.toolsets)
            .into_iter()
            .map(|tool| serde_json::to_value(tool).expect("AgentTool serializes")),
    );
    tools.extend(config.client_tools.iter().map(|tool| {
        json!({
            "type": "custom",
            "name": tool.id,
            "description": tool.description,
            "input_schema": tool.parameters,
        })
    }));
    let model = binding.map_or_else(
        || json!({ "mode": "auto" }),
        |binding| {
            json!({
                "id": binding.model_ref,
                "model_ref": binding.model_ref,
                "provider_identity_ref": binding.provider_identity_ref,
                "backend_ref": binding.backend_ref,
            })
        },
    );
    json!({
        "id": config.id,
        "type": "agent",
        "name": config.name,
        "description": config.description,
        "model": model,
        "system": config.instructions,
        "metadata": config.metadata,
        "tools": tools,
        "recovery_policies": config.recovery_policies,
        "mcp_servers": config.mcp_servers,
        "skills": config.skill_ids,
        "multiagent": config.multiagent,
        "archived_at": config.archived_at,
        "max_steps": config.max_steps,
        "plugins": config.plugin_ids,
        "plugin_config": config.plugin_config,
        "context_policy": config.context_policy,
        "tool_overrides": config.tool_overrides,
        "compaction": config.compaction,
        "published": published,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_managed_model_shape() {
        // Cause graph: C1 `model.mode` is `auto`; C2 an exact model coordinate is
        // present. E1 preserve Auto for Workspace Profile resolution; E2 preserve
        // the exact pinned triple. Decision table: C1=Y -> E1; C1=N,C2=Y -> E2;
        // C1=N,C2=N -> legacy empty pin.
        let auto = agent_config_from_managed(
            "a".into(),
            &json!({
                "model": { "mode": "auto" }
            }),
        )
        .expect("auto model");
        assert!(auto.model_binding.is_auto());
        assert_eq!(
            managed_from_agent_config(&auto, false)["model"]["mode"],
            "auto"
        );

        let from_string = agent_config_from_managed("a".into(), &json!({ "model": "gpt-x" }))
            .expect("string model");
        assert_eq!(
            from_string.model_binding.resolved().unwrap().model_ref,
            "gpt-x"
        );

        let from_object =
            agent_config_from_managed("a".into(), &json!({ "model": { "id": "claude" } }))
                .expect("object model");
        assert_eq!(
            from_object.model_binding.resolved().unwrap().model_ref,
            "claude"
        );

        let missing = agent_config_from_managed("a".into(), &json!({})).expect("absent model");
        assert_eq!(missing.model_binding.resolved().unwrap().model_ref, "");
    }

    #[test]
    fn auto_model_and_config_tool_authoring_projection_stays_lossless() {
        // Cause graph / decision table: auto model preserves `{mode:auto}`;
        // config authoring keeps tool ids as ids; both present must keep both
        // effects (Managed wire typing belongs to its repository adapter).
        let config = agent_config_from_managed(
            "a".into(),
            &json!({
                "model": { "mode": "auto" },
                "tools": ["bash"]
            }),
        )
        .expect("auto model with config tool");
        let projected = managed_from_agent_config(&config, true);
        assert_eq!(projected["model"], json!({ "mode": "auto" }));
        assert_eq!(projected["tools"], json!(["bash"]));
    }

    #[test]
    fn complete_runtime_binding_round_trips_losslessly() {
        let config = agent_config_from_managed(
            "remote".into(),
            &json!({
                "model": {
                    "id": "remote-model",
                    "provider_identity_ref": "peer-a",
                    "backend_ref": "a2a:https://peer.example/v1/a2a"
                }
            }),
        )
        .unwrap();
        let binding = config.model_binding.resolved().unwrap();
        assert_eq!(binding.provider_identity_ref, "peer-a");
        assert_eq!(binding.model_ref, "remote-model");
        assert_eq!(binding.backend_ref, "a2a:https://peer.example/v1/a2a");

        let projected = managed_from_agent_config(&config, false);
        assert_eq!(projected["model"]["id"], "remote-model");
        assert_eq!(projected["model"]["model_ref"], "remote-model");
        assert_eq!(projected["model"]["provider_identity_ref"], "peer-a");
        assert_eq!(
            projected["model"]["backend_ref"],
            "a2a:https://peer.example/v1/a2a"
        );
    }

    #[test]
    fn malformed_context_policy_fails_closed() {
        assert!(agent_config_from_managed("a".into(), &json!({ "context_policy": 123 })).is_err());
    }

    #[test]
    fn malformed_tool_overrides_fail_closed() {
        assert!(agent_config_from_managed("a".into(), &json!({ "tool_overrides": 123 })).is_err());
    }

    #[test]
    fn managed_toolsets_round_trip_through_the_single_contract_normalizer() {
        let tools = json!([{
            "type": "mcp_toolset",
            "mcp_server_name": "calc",
            "configs": [],
            "default_config": {
                "enabled": true,
                "permission_policy": { "type": "always_allow" }
            }
        }]);
        let config = agent_config_from_managed(
            "calculator".into(),
            &json!({ "model": "test", "tools": tools }),
        )
        .expect("typed toolset parses");
        assert_eq!(config.toolsets.len(), 1);
        assert_eq!(managed_from_agent_config(&config, false)["tools"], tools);
    }

    #[test]
    fn compaction_round_trips_for_lossless_editing() {
        let config = agent_config_from_managed(
            "a".into(),
            &json!({
                "system": "compact carefully",
                "compaction": { "window": 32000, "keep_recent": 12 }
            }),
        )
        .unwrap();
        let projected = managed_from_agent_config(&config, false);
        assert_eq!(projected["compaction"]["window"], json!(32000));
        assert_eq!(projected["compaction"]["keep_recent"], json!(12));
    }
}
