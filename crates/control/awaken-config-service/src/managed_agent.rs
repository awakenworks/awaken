//! Managed Agent wire projection for the config authoring aggregate.
//!
//! This is the sole translation seam between the SDK-shaped Agent object and
//! [`AgentConfig`]. Keeping it separate from CRUD/publication prevents protocol
//! projection details from growing the config-plane orchestration module.

use awaken_config_store::{AgentConfig, ModelSelection, MultiagentConfig, ToolOverride};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
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
    Ok(AgentConfig {
        id,
        instructions: string("system").unwrap_or_default(),
        max_steps: body.get("max_steps").and_then(Value::as_u64).unwrap_or(8) as usize,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned(provider_identity_ref, model_ref, backend_ref),
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
    json!({
        "id": config.id,
        "type": "agent",
        "name": config.name,
        "description": config.description,
        "model": {
            "id": binding.map(|binding| binding.model_ref.clone()).unwrap_or_default(),
            "model_ref": binding.map(|binding| binding.model_ref.clone()).unwrap_or_default(),
            "provider_identity_ref": binding.map(|binding| binding.provider_identity_ref.clone()).unwrap_or_default(),
            "backend_ref": binding.map(|binding| binding.backend_ref.clone()).unwrap_or_default(),
        },
        "system": config.instructions,
        "metadata": config.metadata,
        "tools": config.tool_ids,
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
