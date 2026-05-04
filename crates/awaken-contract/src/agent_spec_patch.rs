//! Field-level override for [`AgentSpec`].
//!
//! Stored as JSON inside [`RecordMeta::user_overrides`] for built-in agents.
//! Each field is `Option<T>`: `None` = inherit base, `Some(_)` = override.
//! Merge happens at read time via [`merge_agent_spec`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::inference::ReasoningEffort;
use crate::registry_spec::AgentSpec;

/// Patch for built-in agent customization.
///
/// Phase 3 ships override support for the most commonly tweaked fields.
/// Adding more fields later is purely additive — `Option` defaults to `None`,
/// existing patches decode unchanged.
///
/// `#[serde(deny_unknown_fields)]` rejects payloads containing field names
/// that don't exist on this struct, preventing silent drift when callers
/// misspell or target deprecated fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_continuation_retries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_ids: Option<Vec<String>>,
    /// Per-key shallow merge: patch keys override base keys; un-patched
    /// keys preserved from base. To delete a base key, set its value to
    /// JSON `null` in this map (handled at merge time).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<HashMap<String, Value>>,
    /// Whitelist of tool IDs. `Some([..])` overrides; `None` keeps base.
    /// To revert to base "all tools", DELETE the override field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Blacklist of tool IDs. Same semantics as `allowed_tools`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_tools: Option<Vec<String>>,
    /// Sub-agent IDs this agent can delegate to. `Some([..])` overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegates: Option<Vec<String>>,
    /// Reasoning effort override. `Some(_)` overrides; `None` keeps base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl AgentSpecPatch {
    /// True when no field is set — equivalent to "no override".
    pub fn is_empty(&self) -> bool {
        self.model_id.is_none()
            && self.system_prompt.is_none()
            && self.max_rounds.is_none()
            && self.max_continuation_retries.is_none()
            && self.plugin_ids.is_none()
            && self.sections.is_none()
            && self.allowed_tools.is_none()
            && self.excluded_tools.is_none()
            && self.delegates.is_none()
            && self.reasoning_effort.is_none()
    }
}

/// Apply a [`AgentSpecPatch`] on top of a base [`AgentSpec`], producing the
/// effective spec passed to the resolver.
///
/// Semantics:
/// - Scalar fields (`model_id`, `system_prompt`, `max_rounds`,
///   `max_continuation_retries`): patch's value if `Some`, else base.
/// - `plugin_ids`: replace whole list when patch is `Some`.
/// - `sections`: per-key shallow merge. Patch keys override base keys.
///   A patch value of JSON `null` deletes the corresponding base key.
/// - All other fields pass through from `base` unchanged (id, allowed_tools,
///   excluded_tools, reasoning_effort, context_policy, endpoint, delegates,
///   active_hook_filter, registry, created_at, updated_at).
pub fn merge_agent_spec(base: AgentSpec, patch: AgentSpecPatch) -> AgentSpec {
    AgentSpec {
        id: base.id,
        model_id: patch.model_id.unwrap_or(base.model_id),
        system_prompt: patch.system_prompt.unwrap_or(base.system_prompt),
        max_rounds: patch.max_rounds.unwrap_or(base.max_rounds),
        max_continuation_retries: patch
            .max_continuation_retries
            .unwrap_or(base.max_continuation_retries),
        plugin_ids: patch.plugin_ids.unwrap_or(base.plugin_ids),
        sections: merge_sections(base.sections, patch.sections),
        // Override-aware fields: patch's value when present, else base.
        // For Option<T> fields, patch=Some(value) sets the override; to
        // revert to base's None, callers use the per-field DELETE endpoint.
        allowed_tools: patch.allowed_tools.map(Some).unwrap_or(base.allowed_tools),
        excluded_tools: patch
            .excluded_tools
            .map(Some)
            .unwrap_or(base.excluded_tools),
        delegates: patch.delegates.unwrap_or(base.delegates),
        reasoning_effort: patch
            .reasoning_effort
            .map(Some)
            .unwrap_or(base.reasoning_effort),
        // Pass-through (no patch support):
        context_policy: base.context_policy,
        active_hook_filter: base.active_hook_filter,
        endpoint: base.endpoint,
        registry: base.registry,
        created_at: base.created_at,
        updated_at: base.updated_at,
    }
}

fn merge_sections(
    mut base: HashMap<String, Value>,
    patch: Option<HashMap<String, Value>>,
) -> HashMap<String, Value> {
    let Some(patch) = patch else { return base };
    for (key, value) in patch {
        if value.is_null() {
            base.remove(&key);
        } else {
            base.insert(key, value);
        }
    }
    base
}
