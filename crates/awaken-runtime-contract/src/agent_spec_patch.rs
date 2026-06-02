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
use crate::contract::lifecycle::StopConditionSpec;
use crate::registry_spec::{AgentBackendSpec, AgentSpec, BackendConfigError, RemoteEndpoint};

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
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub description: NullablePatch<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<AgentBackendSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_continuation_retries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_conditions: Option<Vec<StopConditionSpec>>,
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
    /// Literal whitelist of tool IDs. Tri-state at merge time: missing
    /// inherits the base value, JSON `null` sets the field to `None`, a
    /// JSON value overrides.
    ///
    /// A PATCH that clears `allowed_tools` to `null` does NOT re-fire the
    /// legacy "absent = allow all" shim — that shim runs only on initial
    /// deserialize of a full `AgentSpec`, not on patch merge. A merged
    /// spec with neither `allowed_tools` nor `allowed_tool_patterns` set
    /// has no allow rules and the matcher denies every tool.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub allowed_tools: NullablePatch<Vec<String>>,
    /// Glob patterns matched against tool IDs for the allow set. Same
    /// tri-state merge semantics as `allowed_tools`: missing inherits
    /// base, JSON `null` sets the field to `None`, value overrides. The
    /// "absent = allow all" shim does not run on patch merge — clearing
    /// both allow fields via PATCH yields a deny-all matcher.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub allowed_tool_patterns: NullablePatch<Vec<String>>,
    /// Blacklist of tool IDs. Tri-state at merge time: missing inherits
    /// base, JSON `null` sets the field to `None` (i.e. no literal
    /// exclude rules), value overrides. Unlike the allow fields, clearing
    /// the exclude fields is the safe "nothing excluded" state.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub excluded_tools: NullablePatch<Vec<String>>,
    /// Glob patterns matched against tool IDs for the exclude set. Same
    /// tri-state merge semantics as `excluded_tools`; clearing to `None`
    /// means no pattern-based exclusions.
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub excluded_tool_patterns: NullablePatch<Vec<String>>,
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
    ///
    /// **Contract**: endpoint is a *patchable* runtime-safe field at the
    /// API layer. The admin-console editor intentionally treats endpoint
    /// as a locked / read-only field and does NOT expose UI for editing
    /// it — that's a UX simplification ("most operators shouldn't be
    /// rebinding the remote backend of a builtin agent through the
    /// graphical editor"), not an immutability or security boundary.
    /// Programmatic clients (CLI, scripts, other admin tooling)
    /// retain the ability to override endpoint via this PATCH field.
    ///
    /// `patch_overrides_null_clears_nullable_base_field` (in
    /// `crates/awaken-server/tests/config_api.rs`) pins this contract:
    /// `PATCH /v1/config/agents/:id/overrides` with `{"endpoint": null}`
    /// must return 200 and clear the base endpoint. Changing this to a
    /// reject would be a breaking API change requiring a dedicated ADR.
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
            && self.description.is_none()
            && self.backend.is_none()
            && self.system_prompt.is_none()
            && self.max_rounds.is_none()
            && self.max_continuation_retries.is_none()
            && self.stop_conditions.is_none()
            && self.context_policy.is_none()
            && self.plugin_ids.is_none()
            && self.active_hook_filter.is_none()
            && self.sections.is_none()
            && self.allowed_tools.is_none()
            && self.allowed_tool_patterns.is_none()
            && self.excluded_tools.is_none()
            && self.excluded_tool_patterns.is_none()
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
/// - Patch-supported option fields (`allowed_tools`,
///   `allowed_tool_patterns`, `excluded_tools`, `excluded_tool_patterns`,
///   `reasoning_effort`, `context_policy`, `endpoint`) are tri-state:
///   missing inherits, JSON `null` clears, and a JSON value overrides.
///   The legacy "absent catalog = allow all" shim runs only on initial
///   `AgentSpec` deserialize, never on merge — a PATCH that clears both
///   allow fields to `null` produces a deny-all matcher.
/// - Metadata fields pass through from `base` unchanged (id, registry).
///
/// Round-trip normalization (defense in depth): if BOTH `allowed_tools`
/// and `allowed_tool_patterns` end up as `None` after the merge, they
/// are rewritten to `Some(vec![])`. The deserialize side now distinguishes
/// absent from explicit `null` via the double-Option pattern in
/// `AgentSpecRaw`, so a `None`-`None` merged spec serialized as
/// `{"allowed_tools": null, "allowed_tool_patterns": null}` would already
/// re-parse as deny-all without this normalization. Kept anyway because
/// it makes the merge path independently robust: any future caller that
/// serializes via a path which drops nulls (custom formatters, lossy
/// transcoding) still gets explicit `[]` to anchor the deny-all intent.
/// Explicit empty lists serialize as `[]` and survive every round-trip.
pub fn merge_agent_spec(
    base: AgentSpec,
    patch: AgentSpecPatch,
) -> Result<AgentSpec, BackendConfigError> {
    let endpoint_patch = patch.endpoint.clone();
    let backend_patched = patch.backend.is_some();
    let awaken_fields_patched =
        patch.model_id.is_some() || patch.system_prompt.is_some() || patch.max_rounds.is_some();
    let mut merged = AgentSpec {
        id: base.id,
        description: merge_nullable(base.description, patch.description),
        backend: patch.backend.unwrap_or(base.backend),
        model_id: patch.model_id.unwrap_or(base.model_id),
        system_prompt: patch.system_prompt.unwrap_or(base.system_prompt),
        max_rounds: patch.max_rounds.unwrap_or(base.max_rounds),
        max_continuation_retries: patch
            .max_continuation_retries
            .unwrap_or(base.max_continuation_retries),
        stop_conditions: patch.stop_conditions.unwrap_or(base.stop_conditions),
        context_policy: merge_nullable(base.context_policy, patch.context_policy),
        plugin_ids: patch.plugin_ids.unwrap_or(base.plugin_ids),
        active_hook_filter: patch.active_hook_filter.unwrap_or(base.active_hook_filter),
        sections: merge_sections(base.sections, patch.sections),
        allowed_tools: merge_nullable(base.allowed_tools, patch.allowed_tools),
        allowed_tool_patterns: merge_nullable(
            base.allowed_tool_patterns,
            patch.allowed_tool_patterns,
        ),
        excluded_tools: merge_nullable(base.excluded_tools, patch.excluded_tools),
        excluded_tool_patterns: merge_nullable(
            base.excluded_tool_patterns,
            patch.excluded_tool_patterns,
        ),
        delegates: patch.delegates.unwrap_or(base.delegates),
        reasoning_effort: merge_nullable(base.reasoning_effort, patch.reasoning_effort),
        endpoint: merge_nullable(base.endpoint, patch.endpoint),
        // Pass-through metadata:
        registry: base.registry,
    };

    if backend_patched {
        if merged.backend.is_awaken() {
            merged.endpoint = None;
            if let Some(model_id) = merged.backend.awaken_model_id() {
                merged.model_id = model_id;
            }
            if let Some(system_prompt) = merged.backend.awaken_system_prompt() {
                merged.system_prompt = system_prompt;
            }
        } else {
            merged.endpoint = merged.backend.remote_endpoint()?;
        }
    } else {
        match endpoint_patch {
            Some(Some(ref endpoint)) => {
                merged.backend = AgentBackendSpec::from_remote_endpoint(endpoint);
            }
            Some(None) => {
                merged.backend = AgentBackendSpec::awaken_from_fields(
                    &merged.model_id,
                    &merged.system_prompt,
                    merged.max_rounds,
                );
            }
            None if awaken_fields_patched && merged.backend.is_awaken() => {
                merged.backend = AgentBackendSpec::awaken_from_fields(
                    &merged.model_id,
                    &merged.system_prompt,
                    merged.max_rounds,
                );
            }
            None => {}
        }
    }

    // Pin the deny-all intent across a JSON round-trip. See doc comment.
    if merged.allowed_tools.is_none() && merged.allowed_tool_patterns.is_none() {
        merged.allowed_tools = Some(Vec::new());
        merged.allowed_tool_patterns = Some(Vec::new());
    }

    Ok(merged)
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
