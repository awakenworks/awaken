//! Shared helpers for ConfigRecord envelope reading/writing.
//!
//! Both the ConfigService writer path and the ConfigRuntimeManager bootstrap
//! path operate on the envelope; centralizing the helpers here prevents
//! drift and lets future Phase 3 patch logic reuse the same primitives.

use awaken_contract::{
    AgentSpec, AgentSpecPatch, ConfigRecord, McpServerSpec, ModelBindingSpec, ProviderSpec,
    RecordMeta, ToolSpec, ToolSpecPatch, merge_agent_spec, merge_tool_spec,
};
use serde_json::Value;

// ── ApplyPatch trait ──────────────────────────────────────────────────────────

/// Spec types that support field-level patch overrides at read time.
///
/// Phase 3 implements this for `AgentSpec` (with `AgentSpecPatch`).
/// Other spec types use [`NoPatch`] which is a no-op.
pub(crate) trait ApplyPatch: Sized {
    type Patch: serde::de::DeserializeOwned;
    fn apply(self, patch: Self::Patch) -> Self;
}

/// No-op patch type for spec types that don't yet support overrides.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct NoPatch;

impl ApplyPatch for AgentSpec {
    type Patch = AgentSpecPatch;
    fn apply(self, patch: AgentSpecPatch) -> Self {
        merge_agent_spec(self, patch)
    }
}

impl ApplyPatch for ToolSpec {
    type Patch = ToolSpecPatch;
    fn apply(self, patch: ToolSpecPatch) -> Self {
        merge_tool_spec(self, patch)
    }
}

impl ApplyPatch for ProviderSpec {
    type Patch = NoPatch;
    fn apply(self, _patch: NoPatch) -> Self {
        self
    }
}

impl ApplyPatch for ModelBindingSpec {
    type Patch = NoPatch;
    fn apply(self, _patch: NoPatch) -> Self {
        self
    }
}

impl ApplyPatch for McpServerSpec {
    type Patch = NoPatch;
    fn apply(self, _patch: NoPatch) -> Self {
        self
    }
}

/// If `overrides` is `Some(json)`, decode it as `T::Patch` and merge into
/// `spec`. Returns `Err` only if decode fails (forward compat: unknown
/// fields cause `deny_unknown_fields` rejection — surfaces as decode error).
///
/// `None` overrides → `spec` returned unchanged.
pub(crate) fn apply_overrides<T>(spec: T, overrides: Option<&Value>) -> Result<T, serde_json::Error>
where
    T: ApplyPatch,
{
    let Some(overrides) = overrides else {
        return Ok(spec);
    };
    let patch: T::Patch = serde_json::from_value(overrides.clone())?;
    Ok(spec.apply(patch))
}

/// Pull `created_at` and `updated_at` from a bare-spec or envelope-shaped Value.
/// Returns (0, 0) when the spec layer doesn't carry timestamps.
pub(crate) fn extract_timestamps(spec: &Value) -> (u64, u64) {
    let created = spec.get("created_at").and_then(Value::as_u64).unwrap_or(0);
    let updated = spec.get("updated_at").and_then(Value::as_u64).unwrap_or(0);
    (created, updated)
}

/// Wrap a bare spec Value into a User-source envelope, lifting timestamps
/// from the spec body if present.
///
/// The spec's own `created_at`/`updated_at` are lifted into `RecordMeta` for
/// provenance. This does **not** modify the spec itself — the spec timestamps
/// remain authoritative for UI display.
#[cfg(test)]
pub(crate) fn wrap_user(spec: &Value) -> Result<Value, serde_json::Error> {
    let (created_at, updated_at) = extract_timestamps(spec);
    let mut meta = RecordMeta::new_user();
    if created_at != 0 {
        meta.created_at = created_at;
    }
    if updated_at != 0 {
        meta.updated_at = updated_at;
    }
    let record = ConfigRecord {
        spec: spec.clone(),
        meta,
    };
    record.to_value()
}

/// Wrap a bare spec Value into a Builtin-source envelope.
// Not yet called from production code; reserved for Task 130 patch logic.
#[allow(dead_code)]
pub(crate) fn wrap_builtin(spec: &Value, binary_version: &str) -> Result<Value, serde_json::Error> {
    let record = ConfigRecord {
        spec: spec.clone(),
        meta: RecordMeta::new_builtin(binary_version),
    };
    record.to_value()
}

/// If `value` is already an envelope (object containing `spec` and `meta`),
/// return it unchanged. Otherwise wrap as User per `wrap_user`.
///
/// Used for rollback paths where the value being restored may have been
/// written by an earlier writer (envelope) or an older binary (bare spec).
#[cfg(test)]
pub(crate) fn ensure_envelope(value: Value) -> Result<Value, serde_json::Error> {
    if value.is_object()
        && value
            .as_object()
            .is_some_and(|m| m.contains_key("spec") && m.contains_key("meta"))
    {
        Ok(value)
    } else {
        wrap_user(&value)
    }
}

/// If `value` is an envelope, return its `spec` field; otherwise return
/// `value` unchanged. Used by callers that internally operate on bare specs.
///
/// Used to ensure audit `before`/`after` payloads always contain bare specs.
pub(crate) fn unwrap_spec(value: Value) -> Value {
    if value
        .as_object()
        .is_some_and(|m| m.contains_key("spec") && m.contains_key("meta"))
    {
        value.get("spec").cloned().unwrap_or(value)
    } else {
        value
    }
}

/// Look up a top-level field on a value that may be either a bare spec or an envelope.
pub(crate) fn spec_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    if value
        .as_object()
        .is_some_and(|m| m.contains_key("spec") && m.contains_key("meta"))
    {
        value.get("spec").and_then(|s| s.get(field))
    } else {
        value.get(field)
    }
}

#[cfg(test)]
mod tests {
    use awaken_contract::{ConfigRecord, RecordSource};
    use serde_json::json;

    use super::{
        ensure_envelope, extract_timestamps, spec_field, unwrap_spec, wrap_builtin, wrap_user,
    };

    #[test]
    fn wrap_user_creates_envelope_with_user_source() {
        let spec = json!({"name": "test-agent"});
        let result = wrap_user(&spec).unwrap();
        let record: ConfigRecord<serde_json::Value> = serde_json::from_value(result).unwrap();
        assert_eq!(record.meta.source, RecordSource::User);
        assert_ne!(record.meta.created_at, 0);
    }

    #[test]
    fn wrap_user_lifts_timestamps_from_spec() {
        let spec = json!({"name": "test-agent", "created_at": 100u64, "updated_at": 200u64});
        let result = wrap_user(&spec).unwrap();
        let record: ConfigRecord<serde_json::Value> = serde_json::from_value(result).unwrap();
        assert_eq!(record.meta.created_at, 100);
        assert_eq!(record.meta.updated_at, 200);
    }

    #[test]
    fn wrap_user_uses_now_when_spec_lacks_timestamps() {
        let spec = json!({"name": "test-agent"});
        let result = wrap_user(&spec).unwrap();
        let record: ConfigRecord<serde_json::Value> = serde_json::from_value(result).unwrap();
        assert_ne!(record.meta.created_at, 0);
        assert_ne!(record.meta.updated_at, 0);
    }

    #[test]
    fn wrap_builtin_creates_envelope_with_binary_version() {
        let spec = json!({"name": "builtin-model"});
        let result = wrap_builtin(&spec, "v9.9.9").unwrap();
        let record: ConfigRecord<serde_json::Value> = serde_json::from_value(result).unwrap();
        match &record.meta.source {
            RecordSource::Builtin { binary_version } => {
                assert_eq!(binary_version, "v9.9.9");
            }
            other => panic!("expected Builtin source, got {:?}", other),
        }
    }

    #[test]
    fn ensure_envelope_passthrough_for_envelope() {
        let spec = json!({"name": "test"});
        let envelope = wrap_user(&spec).unwrap();
        let result = ensure_envelope(envelope.clone()).unwrap();
        assert_eq!(result, envelope);
    }

    #[test]
    fn ensure_envelope_wraps_bare_spec() {
        let spec = json!({"name": "test"});
        let result = ensure_envelope(spec).unwrap();
        assert!(result.get("spec").is_some());
        assert!(result.get("meta").is_some());
    }

    #[test]
    fn unwrap_spec_extracts_spec_layer() {
        let spec = json!({"name": "test"});
        let envelope = wrap_user(&spec).unwrap();
        let result = unwrap_spec(envelope);
        assert_eq!(result, spec);
    }

    #[test]
    fn unwrap_spec_passthrough_for_bare() {
        let spec = json!({"name": "test"});
        let result = unwrap_spec(spec.clone());
        assert_eq!(result, spec);
    }

    #[test]
    fn spec_field_reads_envelope_spec() {
        let spec = json!({"api_key": "x", "name": "test"});
        let envelope = wrap_user(&spec).unwrap();
        let result = spec_field(&envelope, "api_key");
        assert_eq!(result, Some(&json!("x")));
    }

    #[test]
    fn spec_field_reads_bare_spec() {
        let spec = json!({"api_key": "x", "name": "test"});
        let result = spec_field(&spec, "api_key");
        assert_eq!(result, Some(&json!("x")));
    }

    #[test]
    fn extract_timestamps_returns_zeros_for_missing() {
        let spec = json!({"name": "test"});
        assert_eq!(extract_timestamps(&spec), (0, 0));
    }

    // ── apply_overrides tests ─────────────────────────────────────────────────

    use super::apply_overrides;
    use awaken_contract::{AgentSpec, ProviderSpec};

    fn minimal_agent_spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            model_id: "m".to_owned(),
            system_prompt: "base-prompt".to_owned(),
            max_rounds: 5,
            ..Default::default()
        }
    }

    #[test]
    fn apply_overrides_returns_spec_unchanged_when_overrides_none() {
        let spec = minimal_agent_spec("a");
        let result = apply_overrides(spec.clone(), None).unwrap();
        assert_eq!(result.system_prompt, "base-prompt");
        assert_eq!(result.max_rounds, 5);
    }

    #[test]
    fn apply_overrides_merges_agent_spec_when_overrides_some() {
        let spec = minimal_agent_spec("a");
        let overrides = json!({"system_prompt": "patched"});
        let result = apply_overrides(spec, Some(&overrides)).unwrap();
        assert_eq!(result.system_prompt, "patched");
        // Non-overridden field stays unchanged.
        assert_eq!(result.max_rounds, 5);
    }

    #[test]
    fn apply_overrides_for_provider_is_noop() {
        let spec = ProviderSpec {
            id: "p".to_owned(),
            adapter: "openai".to_owned(),
            ..Default::default()
        };
        // NoPatch deserializes an empty object; any non-empty JSON would fail
        // if NoPatch had deny_unknown_fields, but it doesn't — it's a unit
        // struct that ignores all fields.
        let result = apply_overrides(spec.clone(), None).unwrap();
        assert_eq!(result.id, "p");
    }

    #[test]
    fn apply_overrides_propagates_decode_error_for_unknown_field() {
        let spec = minimal_agent_spec("a");
        // AgentSpecPatch has deny_unknown_fields, so this must fail.
        let bad_overrides = json!({"unknown_field": 1});
        let err = apply_overrides(spec, Some(&bad_overrides));
        assert!(
            err.is_err(),
            "unknown field in overrides must produce a decode error"
        );
    }

    #[test]
    fn apply_overrides_for_tool_spec_replaces_description_when_some() {
        use awaken_contract::ToolSpec;
        let spec = ToolSpec {
            id: "echo".into(),
            name: "Echo".into(),
            description: "stock".into(),
            ..Default::default()
        };
        let overrides = json!({"description": "custom"});
        let result = apply_overrides(spec, Some(&overrides)).unwrap();
        assert_eq!(result.description, "custom");
    }

    #[test]
    fn apply_overrides_for_tool_spec_keeps_base_when_none() {
        use awaken_contract::ToolSpec;
        let spec = ToolSpec {
            id: "echo".into(),
            description: "stock".into(),
            ..Default::default()
        };
        let result = apply_overrides(spec, None).unwrap();
        assert_eq!(result.description, "stock");
    }

    #[test]
    fn apply_overrides_for_tool_spec_rejects_unknown_field() {
        use awaken_contract::ToolSpec;
        let spec = ToolSpec {
            id: "echo".into(),
            ..Default::default()
        };
        let bad = json!({"name": "renamed"});
        assert!(apply_overrides(spec, Some(&bad)).is_err());
    }
}
