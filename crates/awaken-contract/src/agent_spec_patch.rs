//! Field-level override for [`AgentSpec`].
//!
//! Stored as JSON inside [`RecordMeta::user_overrides`] for built-in agents.
//! Missing fields inherit from the base spec. JSON `null` clears fields whose
//! base `AgentSpec` representation is optional.
//! Merge happens at read time via [`merge_agent_spec`].

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::inference::{ContextWindowPolicy, ReasoningEffort};
use crate::registry_spec::{AgentSpec, RemoteEndpoint};

/// Patch value for `AgentSpec` fields that are optional in the base spec.
///
/// - `None` = field is missing from the patch, inherit the base value.
/// - `Some(None)` = field is present as JSON `null`, clear the base value.
/// - `Some(Some(value))` = field is present as a JSON value, override.
pub type NullablePatch<T> = Option<Option<T>>;

/// Patch for built-in agent customization.
///
/// Override support covers runtime-safe AgentSpec fields. Adding more fields
/// later is purely additive because missing fields decode as "inherit".
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
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub context_policy: NullablePatch<ContextWindowPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_hook_filter: Option<HashSet<String>>,
    /// Per-key shallow merge: patch keys override base keys; un-patched
    /// keys preserved from base. To delete a base key, set its value to
    /// JSON `null` in this map (handled at merge time).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<HashMap<String, Value>>,
    /// Whitelist of tool IDs. `Some([..])` overrides; `None` keeps base.
    /// JSON `null` clears to "all tools"; missing inherits base.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub allowed_tools: NullablePatch<Vec<String>>,
    /// Blacklist of tool IDs. Same semantics as `allowed_tools`.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub excluded_tools: NullablePatch<Vec<String>>,
    /// Sub-agent IDs this agent can delegate to. `Some([..])` overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegates: Option<Vec<String>>,
    /// Reasoning effort override. JSON `null` clears the base value.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub reasoning_effort: NullablePatch<ReasoningEffort>,
    /// Remote endpoint override. JSON `null` clears the base value.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub endpoint: NullablePatch<RemoteEndpoint>,
}

impl AgentSpecPatch {
    /// True when no field is set — equivalent to "no override".
    pub fn is_empty(&self) -> bool {
        self.model_id.is_none()
            && self.system_prompt.is_none()
            && self.max_rounds.is_none()
            && self.max_continuation_retries.is_none()
            && self.context_policy.is_none()
            && self.plugin_ids.is_none()
            && self.active_hook_filter.is_none()
            && self.sections.is_none()
            && self.allowed_tools.is_none()
            && self.excluded_tools.is_none()
            && self.delegates.is_none()
            && self.reasoning_effort.is_none()
            && self.endpoint.is_none()
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
/// - Patch-supported option fields (`allowed_tools`, `excluded_tools`,
///   `reasoning_effort`, `context_policy`, `endpoint`) are tri-state:
///   missing inherits, JSON `null` clears, and a JSON value overrides.
/// - Metadata fields pass through from `base` unchanged (id, registry).
pub fn merge_agent_spec(base: AgentSpec, patch: AgentSpecPatch) -> AgentSpec {
    AgentSpec {
        id: base.id,
        model_id: patch.model_id.unwrap_or(base.model_id),
        system_prompt: patch.system_prompt.unwrap_or(base.system_prompt),
        max_rounds: patch.max_rounds.unwrap_or(base.max_rounds),
        max_continuation_retries: patch
            .max_continuation_retries
            .unwrap_or(base.max_continuation_retries),
        context_policy: merge_nullable(base.context_policy, patch.context_policy),
        plugin_ids: patch.plugin_ids.unwrap_or(base.plugin_ids),
        active_hook_filter: patch.active_hook_filter.unwrap_or(base.active_hook_filter),
        sections: merge_sections(base.sections, patch.sections),
        allowed_tools: merge_nullable(base.allowed_tools, patch.allowed_tools),
        excluded_tools: merge_nullable(base.excluded_tools, patch.excluded_tools),
        delegates: patch.delegates.unwrap_or(base.delegates),
        reasoning_effort: merge_nullable(base.reasoning_effort, patch.reasoning_effort),
        endpoint: merge_nullable(base.endpoint, patch.endpoint),
        // Pass-through metadata:
        registry: base.registry,
    }
}

fn merge_nullable<T>(base: Option<T>, patch: NullablePatch<T>) -> Option<T> {
    patch.unwrap_or(base)
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

mod nullable_patch {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            None => serializer.serialize_none(),
            Some(inner) => inner.serialize(serializer),
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }

    pub fn is_missing<T>(value: &Option<Option<T>>) -> bool {
        value.is_none()
    }
}
