//! Managed Agent wire projection for the config authoring aggregate.
//!
//! This is the sole translation seam between the SDK-shaped Agent object and
//! [`AgentConfig`]. Keeping it separate from CRUD/publication prevents protocol
//! projection details from growing the config-plane orchestration module.

use awaken_agent_config::{AgentConfig, ModelSelection, MultiagentConfig, ToolOverride};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::agent_bindings::InferenceOptions;
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use serde_json::{Value, json};

use crate::{parse_managed_model_id, render_managed_model_id};

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

/// Every top-level key this reader consumes: each one is read below and lands in
/// the returned [`AgentConfig`]. A key here is authored intent with an effect.
const MODELLED_KEYS: [&str; 22] = [
    "compaction",
    "context_policy",
    "delegation_limits",
    "description",
    "inference",
    "max_steps",
    "mcp_servers",
    "metadata",
    "model",
    "model_candidates",
    "multiagent",
    "name",
    "plugin_config",
    "plugins",
    "recovery_policies",
    "skills",
    "system",
    "tool_discovery",
    "tool_exposure",
    "tool_overrides",
    "tool_patterns",
    "tools",
];

/// Keys the surrounding config routes own, not this projection: `generation` is
/// the optimistic-concurrency fence read after the parse, and
/// `permission_preset` is an authoring macro the route removes and applies to
/// the compiled config. Accepted here so the one reader can be shared by the
/// preview, validate and PUT routes without each stripping the other's keys.
const ROUTE_OWNED_KEYS: [&str; 2] = ["generation", "permission_preset"];

/// Server-owned keys echoed by [`managed_from_agent_config`] and therefore
/// present in any `GET` → edit → `PUT` round trip. They are accepted and
/// ignored, never authored: `id` comes from the request path, lifecycle
/// (`disabled_at` / `archived_at`) belongs to `ManagedAgentRepository`, and
/// `type` / `published` are projection metadata. Silently ignoring these is
/// correct precisely because a client cannot set them by sending them —
/// unlike an unknown key, whose only possible meaning is authored intent.
const SERVER_OWNED_KEYS: [&str; 5] = ["archived_at", "disabled_at", "id", "published", "type"];

/// Refuse a body carrying a key this projection does not model.
///
/// The PUT this feeds is a **full replace**: it builds a fresh `AgentConfig`
/// with no merge against the stored one. So a key the reader has never heard of
/// is not an ignorable extra — the section it was meant to address is rebuilt
/// from its default and the authored content is destroyed, with a 200 and no
/// diagnostic. Writing the domain field name `instructions` instead of the wire
/// name `system` blanked the entire system prompt exactly that way.
///
/// This closes the asymmetry where a *malformed* section was reported while an
/// *unaddressable* one was not. Unknown means unknown: only keys that are read
/// here, owned by the surrounding route, or echoed back by this module's own
/// projection are accepted.
fn reject_unmodelled_keys(body: &Value) -> Result<(), String> {
    // A non-object body reads as "every key absent", which would build an
    // entirely default config and replace the stored one with it.
    let Some(object) = body.as_object() else {
        return Err("agent configuration must be a JSON object".to_string());
    };
    let mut unknown: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| {
            !MODELLED_KEYS.contains(key)
                && !ROUTE_OWNED_KEYS.contains(key)
                && !SERVER_OWNED_KEYS.contains(key)
        })
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    let mut accepted: Vec<&str> = MODELLED_KEYS
        .iter()
        .chain(ROUTE_OWNED_KEYS.iter())
        .chain(SERVER_OWNED_KEYS.iter())
        .copied()
        .collect();
    accepted.sort_unstable();
    Err(format!(
        "unknown agent configuration field(s): {}. This request replaces the \
         whole configuration, so an unrecognised field would silently blank the \
         section it was meant to set. Accepted fields: {}",
        unknown.join(", "),
        accepted.join(", "),
    ))
}

/// Parse a managed-shaped Agent object into the domain compile input.
pub fn agent_config_from_managed(id: String, body: &Value) -> Result<AgentConfig, String> {
    reject_unmodelled_keys(body)?;
    let string = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
    let model_binding = managed_model_selection(body.get("model"))?;
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
    let inference: InferenceOptions = body
        .get("inference")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("invalid inference options: {error}"))?
        .unwrap_or_default();
    let tool_patterns = array("tool_patterns")
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "tool_patterns entries must be strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let model_fallbacks = array("model_candidates")
        .into_iter()
        .map(|value| {
            serde_json::from_value::<ModelBinding>(value)
                .map_err(|error| format!("invalid model candidate: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tool_overrides: Vec<ToolOverride> = match body.get("tool_overrides").cloned() {
        Some(value) => serde_json::from_value(value).map_err(|error| error.to_string())?,
        None => Vec::new(),
    };
    let metadata: std::collections::BTreeMap<String, String> = body
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
    let skills = array("skills")
        .into_iter()
        .map(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let multiagent = match body.get("multiagent") {
        Some(value) if value.is_null() => None,
        Some(value) => Some(
            serde_json::from_value::<MultiagentConfig>(value.clone())
                .map_err(|error| format!("invalid multiagent roster: {error}"))?,
        ),
        None => None,
    };
    let delegation_limits = body
        .get("delegation_limits")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("invalid delegation limits: {error}"))?
        .unwrap_or_default();
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
    awaken_session_contract::validate_agent_tools(&authored_toolsets)?;
    let config = AgentConfig {
        id,
        instructions: string("system").unwrap_or_default(),
        max_steps: body
            .get("max_steps")
            .and_then(Value::as_u64)
            .map_or(awaken_runtime_contract::DEFAULT_MAX_STEPS, |value| {
                value as usize
            }),
        delegation_limits,
        model_binding,
        inference,
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
        tool_patterns,
        model_fallbacks,
        name: string("name"),
        description: string("description"),
        metadata,
        mcp_servers,
        skills,
        multiagent,
        // Lifecycle commands are owned by ManagedAgentRepository. Generic
        // config authoring cannot forge a transition while leaving the live
        // installed-catalog pointer unreconciled.
        disabled_at: None,
        archived_at: None,
        tool_overrides,
        tool_exposure: body
            .get("tool_exposure")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("invalid tool_exposure: {error}"))?
            .unwrap_or_default(),
        tool_discovery: body
            .get("tool_discovery")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("invalid tool_discovery: {error}"))?
            .unwrap_or_default(),
        recovery_policies: body
            .get("recovery_policies")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default(),
        // Every other authored section reports its parse failure. This one used
        // to swallow it with `.ok()`, so a malformed compaction strategy became
        // `None` and the hardcoded default window silently took over with no
        // signal to the author that their configuration had been discarded.
        compaction: body
            .get("compaction")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("invalid compaction strategy: {error}"))?,
    };
    config.validate_managed_tool_bindings()?;
    config.canonicalize_mutable_authoring()
}

fn managed_model_selection(model: Option<&Value>) -> Result<ModelSelection, String> {
    let Some(model) = model else {
        return Ok(ModelSelection::Auto);
    };
    if let Some(model) = model.as_str() {
        return parse_managed_model_id(model).map_err(|error| error.to_string());
    }
    // Anything carrying the canonical `mode` discriminant IS a `ModelSelection`
    // in its own authoring wire, so `ModelSelection`'s own deserializer decides
    // it: every mode, `deny_unknown_fields`, and each per-mode invariant are
    // owned once, by the domain type. A hand-rolled transcription of the same
    // tagged enum used to live here, and it drifted exactly as a second parser
    // must — it knew nothing of `{mode:"target"}` and silently rejected every
    // catalog route the console authored. The parallel reader is deleted rather
    // than extended so the next mode cannot reintroduce the divergence.
    if model.get("mode").is_some() {
        return serde_json::from_value(model.clone())
            .map_err(|error| format!("invalid model selection: {error}"));
    }
    // Managed's own untagged model object is a genuinely different wire, not a
    // second spelling of `ModelSelection`: an opaque `{id}` model-id string in
    // a field, or a complete `{provider_identity_ref, model_ref|id,
    // backend_ref}` pin. Those keep their Managed-compatibility codec.
    let (provider_identity_ref, model_ref, backend_ref) = match model {
        Value::Object(model)
            if !model.contains_key("provider_identity_ref")
                && !model.contains_key("backend_ref") =>
        {
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "model object requires a non-empty id".to_string())?;
            return parse_managed_model_id(id).map_err(|error| error.to_string());
        }
        Value::Object(model) => {
            let required = |key: &str| {
                model
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("pinned model object requires non-empty `{key}`"))
            };
            (
                required("provider_identity_ref")?,
                model
                    .get("model_ref")
                    .or_else(|| model.get("id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        "pinned model object requires non-empty `model_ref` or `id`".to_string()
                    })?,
                required("backend_ref")?,
            )
        }
        _ => return Err("model must be a string or object".to_string()),
    };
    ModelSelection::pinned(provider_identity_ref, model_ref, backend_ref)
        .map_err(|error| format!("pinned model is not an executable binding: {error}"))
}

/// Project a stored config into the managed-shaped object and its live state.
pub fn managed_from_agent_config(config: &AgentConfig, published: bool) -> Value {
    let binding = config.model_binding.resolved();
    let model = match &config.model_binding {
        // The Managed model-id string stays the projection whenever it can
        // carry the whole selection. Qualifiers it cannot spell — a protocol
        // endpoint id, an adapter-native Session configuration — keep the
        // canonical authoring wire instead of being dropped on the way out.
        ModelSelection::Target { configuration, .. } => {
            match render_managed_model_id(&config.model_binding) {
                Ok(id) if configuration.is_empty() => Value::String(id),
                _ => {
                    serde_json::to_value(&config.model_binding).expect("ModelSelection serializes")
                }
            }
        }
        ModelSelection::BackendDefault { .. } | ModelSelection::BackendExact { .. } => {
            serde_json::to_value(&config.model_binding).expect("ModelSelection serializes")
        }
        ModelSelection::Auto => json!({ "mode": "auto" }),
        ModelSelection::Profile { profile_id } => json!({
            "mode": "profile",
            "profile_id": profile_id,
        }),
        ModelSelection::Pinned(_) => json!({
            "id": binding.map(|binding| binding.model_ref.clone()).unwrap_or_default(),
            "model_ref": binding.map(|binding| binding.model_ref.clone()).unwrap_or_default(),
            "provider_identity_ref": binding.map(|binding| binding.provider_identity_ref.clone()).unwrap_or_default(),
            "backend_ref": binding.map(|binding| binding.backend_ref.clone()).unwrap_or_default(),
        }),
    };
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
    json!({
        "id": config.id,
        "type": "agent",
        "name": config.name,
        "description": config.description,
        "model": model,
        "inference": config.inference,
        "model_candidates": config.model_fallbacks,
        "system": config.instructions,
        "metadata": config.metadata,
        "tools": tools,
        "tool_patterns": config.tool_patterns,
        "recovery_policies": config.recovery_policies,
        "mcp_servers": config.mcp_servers,
        "skills": config.skills,
        "multiagent": config.multiagent,
        "delegation_limits": config.delegation_limits,
        "disabled_at": config.disabled_at,
        "archived_at": config.archived_at,
        "max_steps": config.max_steps,
        "plugins": config.plugin_ids,
        "plugin_config": config.plugin_config,
        "context_policy": config.context_policy,
        "tool_overrides": config.tool_overrides,
        "tool_exposure": config.tool_exposure,
        "tool_discovery": config.tool_discovery,
        "compaction": config.compaction,
        "published": published,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_managed_model_shape() {
        // Causes: C1 explicit auto; C2 Managed string; C3 Managed object;
        // C4 absent model. Effects: E1 Auto intent; E2 unresolved Target intent;
        // E3 the object id uses the same parser. Rules: C1/C4 -> E1,
        // C2/C3 -> E2; incomplete Pinned is never a parallel selection syntax.
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
            from_string.model_binding.target().unwrap().0.model_id,
            "gpt-x"
        );

        let from_object =
            agent_config_from_managed("a".into(), &json!({ "model": { "id": "claude" } }))
                .expect("object model");
        assert_eq!(
            from_object.model_binding.target().unwrap().0.model_id,
            "claude"
        );

        let missing = agent_config_from_managed("a".into(), &json!({})).expect("absent model");
        assert!(missing.model_binding.is_auto());
        assert_eq!(
            missing.max_steps,
            awaken_runtime_contract::DEFAULT_MAX_STEPS
        );
    }

    #[test]
    fn generic_config_authoring_cannot_forge_agent_lifecycle() {
        // Cause graph: lifecycle timestamps arrive through the generic config
        // object -> the representation adapter discards them -> only the
        // ManagedAgentRepository transition can update lifecycle and reconcile
        // the installed executable pointer.
        //
        // Decision rule L1: disabled_at and/or archived_at in authoring JSON
        // always project to Published.
        let config = agent_config_from_managed(
            "a".into(),
            &json!({
                "disabled_at": "2026-07-29T00:00:00Z",
                "archived_at": "2026-07-29T00:00:01Z"
            }),
        )
        .expect("ordinary authoring remains valid");
        assert_eq!(
            config.lifecycle(),
            awaken_agent_config::AgentLifecycle::Published,
            "L1"
        );
    }

    #[test]
    fn target_model_authoring_round_trips_through_the_canonical_wire() {
        // Decision table for the console's catalog route authoring:
        // T1 a target naming a protocol endpoint the Managed model-id string
        //    cannot encode -> accepted and projected back unchanged;
        // T2 a target plus its ACP session configuration -> both survive;
        // T3 a target without a model id -> rejected, never silently pinned;
        // T4 a target the string can spell in full -> that string is kept.
        let wire = json!({
            "mode": "target",
            "backend_ref": "genai",
            "target": {
                "model_id": "selected",
                "provider_id": "selected-provider",
                "protocol_endpoint_id": "chat",
                "api_dialect": "open_ai_chat"
            }
        });
        let config = agent_config_from_managed("target".into(), &json!({ "model": wire.clone() }))
            .expect("T1");
        assert_eq!(config.model_binding.backend_ref(), Some("genai"), "T1");
        assert_eq!(
            managed_from_agent_config(&config, false)["model"],
            wire,
            "T1"
        );

        let configured = json!({
            "mode": "target",
            "backend_ref": "acp:codex",
            "target": { "model_id": "gpt-x" },
            "configuration": { "mode": "plan" }
        });
        let config =
            agent_config_from_managed("target-acp".into(), &json!({ "model": configured.clone() }))
                .expect("T2");
        assert_eq!(
            managed_from_agent_config(&config, false)["model"],
            configured,
            "T2"
        );

        assert!(
            agent_config_from_managed(
                "target-empty".into(),
                &json!({ "model": { "mode": "target", "backend_ref": "genai", "target": { "model_id": "" } } }),
            )
            .is_err(),
            "T3"
        );

        // T4 a target the Managed model-id string can spell in full keeps that
        // compatible projection, so object authoring never widens the wire.
        let config = agent_config_from_managed(
            "target-compatible".into(),
            &json!({
                "model": {
                    "mode": "target",
                    "backend_ref": "genai",
                    "target": { "model_id": "gpt-5", "provider_id": "openai", "api_dialect": "open_ai_chat" }
                }
            }),
        )
        .expect("T4");
        assert_eq!(
            managed_from_agent_config(&config, false)["model"],
            json!("gpt-5;provider=openai;api=open_ai_chat"),
            "T4"
        );
    }

    #[test]
    fn mode_tagged_model_authoring_is_decided_only_by_the_domain_deserializer() {
        // This seam must own no second transcription of `ModelSelection`'s
        // tagged wire. Decision table over shapes only the domain type can
        // judge, each of which a hand-rolled reader gets wrong in a different
        // direction:
        // D1 a mode the reader has never heard of -> rejected, never silently
        //    reinterpreted as an untagged pinned object;
        // D2 a field that belongs to no mode -> rejected by deny_unknown_fields
        //    rather than dropped, for `auto` as much as for every other mode;
        // D3 the domain's own per-mode invariant decides, and says so in the
        //    domain's words;
        // D4 the untagged Managed `{provider_identity_ref, id, backend_ref}`
        //    pin is NOT this wire and keeps working.
        let rejected = |model: Value| {
            agent_config_from_managed("m".into(), &json!({ "model": model })).expect_err("rejected")
        };
        assert!(
            rejected(json!({ "mode": "chaos", "backend_ref": "acp:codex" }))
                .contains("unknown variant"),
            "D1"
        );
        assert!(
            rejected(json!({ "mode": "auto", "backend_ref": "acp:codex" }))
                .contains("unknown field"),
            "D2"
        );
        assert!(
            rejected(json!({
                "mode": "backend_exact",
                "backend_ref": "acp:codex",
                "model_ref": ""
            }))
            .contains("exact model reference must be non-empty"),
            "D3"
        );
        let pinned = agent_config_from_managed(
            "pin".into(),
            &json!({
                "model": {
                    "id": "remote-model",
                    "provider_identity_ref": "peer-a",
                    "backend_ref": "genai"
                }
            }),
        )
        .expect("D4");
        assert_eq!(
            pinned
                .model_binding
                .resolved()
                .map(|b| b.model_ref.as_str()),
            Some("remote-model"),
            "D4"
        );
    }

    #[test]
    fn malformed_compaction_authoring_is_reported_not_silently_defaulted() {
        // A discarded compaction strategy is invisible at runtime: the agent
        // simply uses the hardcoded default window. The author must be told
        // their section was unusable instead of discovering it from behaviour.
        // C1 a well-formed strategy is kept; C2 a malformed one is an error;
        // C3 an absent/null section stays absent without complaint.
        let kept = agent_config_from_managed(
            "compaction-ok".into(),
            &json!({ "compaction": { "window": 32_000, "keep_recent": 4 } }),
        )
        .expect("C1");
        assert_eq!(
            kept.compaction.as_ref().and_then(|c| c.window),
            Some(32_000),
            "C1"
        );
        assert!(
            agent_config_from_managed(
                "compaction-bad".into(),
                // A console that sends its numbers as strings used to have the
                // whole section dropped in silence.
                &json!({ "compaction": { "window": "32000" } }),
            )
            .expect_err("C2")
            .contains("invalid compaction strategy"),
            "C2"
        );
        assert!(
            agent_config_from_managed("compaction-absent".into(), &json!({}))
                .expect("C3")
                .compaction
                .is_none(),
            "C3"
        );
        assert!(
            agent_config_from_managed(
                "compaction-null".into(),
                &json!({ "compaction": Value::Null })
            )
            .expect("C3")
            .compaction
            .is_none(),
            "C3"
        );
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
    fn backend_default_model_round_trips_without_creating_a_pinned_shadow() {
        // Cause graph: managed backend-default wire -> existing ModelSelection ->
        // publication resolver. Projection emits the same policy, never a fake
        // empty pinned model that competes with the domain source of truth.
        //
        // Decision table:
        // B1 exact backend, no model/provider -> BackendDefault + lossless wire
        // B2 missing backend                -> reject
        // B3 backend plus model/provider    -> reject as ambiguous
        let config = agent_config_from_managed(
            "local-codex".into(),
            &json!({
                "model": { "mode": "backend_default", "backend_ref": "acp:codex" }
            }),
        )
        .expect("B1");
        assert_eq!(
            config.model_binding.backend_default_ref(),
            Some("acp:codex"),
            "B1"
        );
        assert_eq!(
            managed_from_agent_config(&config, false)["model"],
            json!({ "mode": "backend_default", "backend_ref": "acp:codex" }),
            "B1"
        );
        assert!(
            agent_config_from_managed(
                "invalid".into(),
                &json!({ "model": { "mode": "backend_default" } }),
            )
            .is_err(),
            "B2"
        );
        assert!(
            agent_config_from_managed(
                "ambiguous".into(),
                &json!({
                    "model": {
                        "mode": "backend_default",
                        "backend_ref": "acp:codex",
                        "id": "gpt-x"
                    }
                }),
            )
            .is_err(),
            "B3"
        );

        // B4 exact backend + native configuration -> lossless canonical wire.
        let exact_wire = json!({
            "mode": "backend_exact",
            "backend_ref": "acp:codex",
            "model_ref": "gpt-exact",
            "configuration": {
                "mode": "plan",
                "options": { "reasoning_effort": "high" }
            }
        });
        let exact =
            agent_config_from_managed("exact".into(), &json!({ "model": exact_wire.clone() }))
                .expect("B4");
        assert_eq!(
            managed_from_agent_config(&exact, false)["model"],
            exact_wire,
            "B4"
        );
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
    fn typed_tool_exposure_and_discovery_policies_round_trip() {
        // Cause graph: Managed authoring supplies the two typed policy objects;
        // effect: AgentConfig and the lossless Managed projection preserve them
        // exactly. Malformed objects fail before publication rather than being
        // silently replaced by defaults.
        let exposure = json!({
            "rules": [{
                "selector": { "kind": "prefix", "value": "mcp__docs__" },
                "exposure": "on_demand"
            }]
        });
        let discovery = json!({
            "max_results": 5,
            "prompt": { "mode": "disabled" }
        });
        let config = agent_config_from_managed(
            "typed-tools".into(),
            &json!({
                "tool_exposure": exposure.clone(),
                "tool_discovery": discovery.clone()
            }),
        )
        .expect("typed policies parse");
        let projected = managed_from_agent_config(&config, false);
        assert_eq!(projected["tool_exposure"], exposure);
        assert_eq!(projected["tool_discovery"], discovery);
        assert!(
            agent_config_from_managed(
                "invalid".into(),
                &json!({ "tool_discovery": { "max_results": 0 } }),
            )
            .is_err()
        );
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
            &json!({
                "model": "test",
                "mcp_servers": [{
                    "type": "url",
                    "name": "calc",
                    "url": "https://calc.example.test/mcp"
                }],
                "tools": tools
            }),
        )
        .expect("typed toolset parses");
        assert_eq!(config.toolsets.len(), 1);
        assert_eq!(managed_from_agent_config(&config, false)["tools"], tools);
    }

    #[test]
    fn managed_authoring_accepts_typed_tools_and_rejects_legacy_permission() {
        // Cause/effect graph: C1 the Managed request contains the existing typed
        // Agent toolset wire; C2 it contains retired plugin_config.permission.
        // E1 C1 reaches the shared mutable AgentConfig authority unchanged; E2 C2
        // returns migration_required before a writable AgentConfig is produced.
        // Decision rows: M1=typed/no legacy -> accepted; M2=legacy -> rejected.
        let typed = agent_config_from_managed(
            "controlled".into(),
            &json!({
                "tools": [{
                    "type": "agent_toolset_20260401",
                    "default_config": {
                        "enabled": false,
                        "permission_policy": { "type": "always_allow" }
                    },
                    "configs": [{
                        "name": "write",
                        "enabled": true,
                        "permission_policy": { "type": "always_ask" }
                    }]
                }]
            }),
        )
        .expect("M1/E1");
        assert_eq!(typed.toolsets.len(), 1, "M1/E1");

        let error = agent_config_from_managed(
            "legacy".into(),
            &json!({
                "plugin_config": {
                    "permission": {
                        "default_behavior": "ask",
                        "rules": []
                    }
                }
            }),
        )
        .expect_err("M2/E2");
        assert!(
            error.contains("permission migration_required"),
            "M2/E2: {error}"
        );
    }

    #[test]
    fn managed_mcp_server_and_toolset_are_an_exact_pair_at_the_authoring_boundary() {
        // Causal decision table:
        // server/toolset = 0/0 valid, 1/1 matching valid, 1/0 missing policy,
        // 0/1 dangling policy, 1/2 duplicate policy. Every invalid row is
        // rejected before an AgentConfig can be stored or compiled.
        let server = json!({
            "type": "url", "name": "docs", "url": "https://docs.example.test/mcp"
        });
        let toolset = json!({ "type": "mcp_toolset", "mcp_server_name": "docs" });
        assert!(agent_config_from_managed("empty".into(), &json!({})).is_ok());
        assert!(
            agent_config_from_managed(
                "paired".into(),
                &json!({ "mcp_servers": [server.clone()], "tools": [toolset.clone()] }),
            )
            .is_ok()
        );
        assert!(
            agent_config_from_managed(
                "missing".into(),
                &json!({ "mcp_servers": [server.clone()] }),
            )
            .unwrap_err()
            .contains("every MCP server must have one mcp_toolset")
        );
        assert!(
            agent_config_from_managed("dangling".into(), &json!({ "tools": [toolset.clone()] }),)
                .unwrap_err()
                .contains("undeclared server")
        );
        assert!(
            agent_config_from_managed(
                "duplicate".into(),
                &json!({ "mcp_servers": [server], "tools": [toolset.clone(), toolset] }),
            )
            .unwrap_err()
            .contains("duplicated")
        );
    }

    #[test]
    fn managed_agent_toolset_rejects_unknown_members_before_normalization() {
        // Cause/effect graph: C1 an official Agent toolset contains a closed-set
        // member or C2 an unknown name. Effects: E1 C1 reaches the canonical
        // policy normalizer; E2 C2 is rejected before any lossy projection.
        // Decision rows: known=>normalize; unknown=>error/no AgentConfig.
        // Constraint: validation and normalization share the Session-contract
        // member authority; Config Service cannot admit a parallel tool path.
        let error = agent_config_from_managed(
            "invalid-toolset".into(),
            &json!({
                "model": "test",
                "tools": [{
                    "type": "agent_toolset_20260401",
                    "configs": [{"name": "parallel_web_search"}]
                }]
            }),
        )
        .expect_err("unknown Agent-tool member must fail before normalization");
        assert_eq!(error, "unknown agent tool `parallel_web_search`");
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

    #[test]
    fn advanced_model_and_tool_controls_round_trip_without_silent_reset() {
        // Cause/effect matrix:
        // C1 inference placement/effort/speed -> E1 exact options survive;
        // C2 tool patterns -> E2 exact ordered patterns survive;
        // C3 fallback candidates -> E3 exact provider/model/backend identities survive;
        // C4 malformed values on any axis -> E4 authoring fails closed.
        let body = json!({
            "model": "deepseek-v4-flash",
            "inference": {
                "effort": "high",
                "speed": "fast",
                "inference_geo": "cn"
            },
            "tool_patterns": ["mcp__release__*", "read"],
            "model_candidates": [{
                "provider_identity_ref": "provider-backup",
                "model_ref": "backup-model",
                "backend_ref": "genai"
            }]
        });
        let config = agent_config_from_managed("reviewer".into(), &body).expect("C1-C3");
        let projected = managed_from_agent_config(&config, false);
        assert_eq!(projected["inference"], body["inference"], "E1");
        assert_eq!(projected["tool_patterns"], body["tool_patterns"], "E2");
        assert_eq!(
            projected["model_candidates"], body["model_candidates"],
            "E3"
        );

        for malformed in [
            json!({ "inference": { "effort": "unbounded" } }),
            json!({ "tool_patterns": [7] }),
            json!({ "model_candidates": [{ "model_ref": "missing-identities" }] }),
        ] {
            assert!(
                agent_config_from_managed("invalid".into(), &malformed).is_err(),
                "E4"
            );
        }
    }

    #[test]
    fn multiagent_and_delegation_limits_round_trip_together() {
        let body = json!({
            "system": "coordinate specialists",
            "multiagent": {
                "type": "coordinator",
                "agents": [
                    "researcher",
                    { "type": "agent", "id": "coder", "version": 7 },
                    { "type": "self" }
                ]
            },
            "delegation_limits": {
                "max_depth": 3,
                "max_parallel": 4,
                "max_total": 16
            }
        });
        let config = agent_config_from_managed("coordinator".into(), &body)
            .expect("typed collaboration config parses");
        assert_eq!(config.delegation_limits.max_depth, 3);
        assert_eq!(config.delegation_limits.max_parallel, 4);
        assert_eq!(config.delegation_limits.max_total, 16);

        let projected = managed_from_agent_config(&config, false);
        assert_eq!(projected["multiagent"], body["multiagent"]);
        assert_eq!(projected["delegation_limits"], body["delegation_limits"]);
    }

    #[test]
    fn malformed_delegation_limits_fail_closed() {
        assert!(
            agent_config_from_managed(
                "a".into(),
                &json!({ "delegation_limits": { "max_depth": "many" } }),
            )
            .is_err()
        );
    }

    #[test]
    fn attached_auxiliaries_and_explicit_opt_out_do_not_gain_a_roster() {
        let auxiliary = agent_config_from_managed(
            "lead--research".into(),
            &json!({
                "system": "research",
                "metadata": {
                    "awaken.parent_agent_id": "lead",
                    "awaken.agent_role": "auxiliary"
                }
            }),
        )
        .expect("attached auxiliary");
        assert!(auxiliary.multiagent.is_none());

        let explicitly_disabled =
            agent_config_from_managed("manual".into(), &json!({ "multiagent": null }))
                .expect("explicit opt-out");
        assert!(explicitly_disabled.multiagent.is_none());
    }

    /// Unknown-key admission for the full-replace authoring body.
    ///
    /// | Rule | body key                                 | effect |
    /// | A1   | every key is modelled                    | accepted, sections bound |
    /// | A2   | the domain name `instructions` (wire: `system`) | rejected, naming it |
    /// | A3   | server-owned `disabled_at` / `archived_at` / `id` / `type` / `published` | accepted and ignored |
    /// | A4   | route-owned `generation` / `permission_preset` | accepted |
    /// | A5   | body is not a JSON object                | rejected |
    ///
    /// A2 is the live one. The PUT builds a fresh config with no merge, so an
    /// unaddressable key does not leave the section alone — it rebuilds it from
    /// the default. Authoring `instructions` used to return 200 with the system
    /// prompt replaced by `""`.
    ///
    /// A3 must stay *accepted*: those keys are echoed by this module's own
    /// projection, so rejecting them would break every `GET` → edit → `PUT`.
    /// Ignoring them is safe because authority for each lives elsewhere; that is
    /// exactly what an unknown key cannot claim.
    #[test]
    fn an_unmodelled_authoring_key_is_rejected_rather_than_blanking_a_section() {
        let accepted = agent_config_from_managed(
            "a".into(),
            &json!({ "system": "be helpful", "max_steps": 7 }),
        )
        .expect("A1");
        assert_eq!(accepted.instructions, "be helpful", "A1");

        let error = agent_config_from_managed(
            "a".into(),
            &json!({ "instructions": "be helpful", "max_steps": 7 }),
        )
        .expect_err("A2: the domain field name is not the wire name");
        assert!(error.contains("instructions"), "A2 names the key: {error}");
        assert!(
            error.contains("system"),
            "A2 lists the accepted fields so the author can find `system`: {error}"
        );

        let tolerated = agent_config_from_managed(
            "a".into(),
            &json!({
                "system": "be helpful",
                "id": "a",
                "type": "agent",
                "published": true,
                "disabled_at": "2026-01-01T00:00:00Z",
                "archived_at": null,
                "generation": 3,
                "permission_preset": "controlled_modifications",
            }),
        )
        .expect("A3/A4");
        assert_eq!(tolerated.instructions, "be helpful", "A3/A4");
        assert!(
            tolerated.disabled_at.is_none() && tolerated.archived_at.is_none(),
            "A3: lifecycle stays owned by the repository, never authored here"
        );

        for body in [json!("be helpful"), json!([]), json!(null)] {
            assert!(
                agent_config_from_managed("a".into(), &body).is_err(),
                "A5: a non-object body must not build an all-default config: {body}"
            );
        }
    }

    /// Round-trip closure: every key this module's own projection emits must be
    /// a key its reader accepts. Without this, adding a field to the projection
    /// (or renaming one) turns the next `GET` → edit → `PUT` from the console
    /// into a 400 — the allow-list would have to be remembered by hand.
    #[test]
    fn every_projected_key_survives_a_round_trip() {
        let config = agent_config_from_managed(
            "round-trip".into(),
            &json!({ "system": "be helpful", "name": "Helper" }),
        )
        .expect("seed config");
        let mut projected = managed_from_agent_config(&config, true);
        // `get_config` adds the concurrency fence to exactly this projection.
        projected
            .as_object_mut()
            .expect("projection is an object")
            .insert("generation".to_string(), json!(1));

        reject_unmodelled_keys(&projected).expect("a projected body must be re-authorable");
        let back = agent_config_from_managed("round-trip".into(), &projected)
            .expect("a projected body must parse back");
        assert_eq!(back.instructions, config.instructions);
        assert_eq!(back.name, config.name);
    }
}
