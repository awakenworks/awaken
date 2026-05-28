use std::collections::HashMap;

use awaken_runtime_contract::contract::inference::{ContextWindowPolicy, ReasoningEffort};
use awaken_runtime_contract::registry_spec::RemoteEndpoint;
use awaken_runtime_contract::{AgentSpec, AgentSpecPatch, merge_agent_spec};
use serde_json::{Value, json};

fn base_spec() -> AgentSpec {
    AgentSpec {
        id: "test".into(),
        model_id: "m".into(),
        system_prompt: "p".into(),
        ..Default::default()
    }
}

// 1. default_is_empty
#[test]
fn default_is_empty() {
    assert!(AgentSpecPatch::default().is_empty());
}

// 2. is_empty_false_when_any_field_set
#[test]
fn is_empty_false_when_any_field_set() {
    let patch = AgentSpecPatch {
        model_id: Some("x".into()),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        system_prompt: Some("sp".into()),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        max_rounds: Some(5),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        max_continuation_retries: Some(3),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        plugin_ids: Some(vec!["a".into()]),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        context_policy: Some(Some(ContextWindowPolicy::default())),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        sections: Some(HashMap::new()),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        allowed_tool_patterns: Some(Some(vec!["mcp:*".into()])),
        ..Default::default()
    };
    assert!(!patch.is_empty());

    let patch = AgentSpecPatch {
        excluded_tool_patterns: Some(Some(vec!["danger:*".into()])),
        ..Default::default()
    };
    assert!(!patch.is_empty());
}

// 3. serde_round_trip_full_patch
#[test]
fn serde_round_trip_full_patch() {
    let mut sections = HashMap::new();
    sections.insert("key1".to_string(), json!({"nested": true}));
    sections.insert("key2".to_string(), json!(42));

    let patch = AgentSpecPatch {
        model_id: Some("claude-opus".into()),
        system_prompt: Some("You are helpful.".into()),
        max_rounds: Some(10),
        max_continuation_retries: Some(3),
        context_policy: Some(Some(ContextWindowPolicy::default())),
        plugin_ids: Some(vec!["plugin-a".into(), "plugin-b".into()]),
        active_hook_filter: Some(["plugin-a".to_string()].into_iter().collect()),
        sections: Some(sections),
        allowed_tools: Some(Some(vec!["weather".into()])),
        allowed_tool_patterns: Some(Some(vec!["mcp:*".into()])),
        excluded_tools: Some(Some(vec!["dangerous".into()])),
        excluded_tool_patterns: Some(Some(vec!["danger:*".into()])),
        delegates: Some(vec!["delegate-a".into()]),
        reasoning_effort: None,
        endpoint: None,
    };

    let json_str = serde_json::to_string(&patch).unwrap();
    let decoded: AgentSpecPatch = serde_json::from_str(&json_str).unwrap();
    assert_eq!(patch, decoded);
}

// 4. serde_omits_none_fields
#[test]
fn serde_omits_none_fields() {
    let patch = AgentSpecPatch::default();
    let json_str = serde_json::to_string(&patch).unwrap();
    let value: Value = serde_json::from_str(&json_str).unwrap();
    // Should be empty object — no null fields
    assert_eq!(value, json!({}));
}

// 5. serde_rejects_unknown_field
#[test]
fn serde_rejects_unknown_field() {
    let result = serde_json::from_str::<AgentSpecPatch>(r#"{"unknown_field": 1}"#);
    assert!(result.is_err(), "expected error for unknown field");
}

// 6. merge_returns_base_when_patch_is_empty
#[test]
fn merge_returns_base_when_patch_is_empty() {
    let base = base_spec();
    let base_value = serde_json::to_value(&base).unwrap();
    let result = merge_agent_spec(base, AgentSpecPatch::default());
    let result_value = serde_json::to_value(&result).unwrap();
    assert_eq!(base_value, result_value);
}

// 7. merge_overrides_model_id
#[test]
fn merge_overrides_model_id() {
    let base = AgentSpec {
        model_id: "A".into(),
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        model_id: Some("B".into()),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.model_id, "B");
}

// 8. merge_overrides_system_prompt
#[test]
fn merge_overrides_system_prompt() {
    let base = AgentSpec {
        system_prompt: "old prompt".into(),
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        system_prompt: Some("new prompt".into()),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.system_prompt, "new prompt");
}

// 9. merge_overrides_max_rounds
#[test]
fn merge_overrides_max_rounds() {
    let base = AgentSpec {
        max_rounds: 5,
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        max_rounds: Some(20),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.max_rounds, 20);
}

// 10. merge_overrides_max_continuation_retries
#[test]
fn merge_overrides_max_continuation_retries() {
    let base = AgentSpec {
        max_continuation_retries: 1,
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        max_continuation_retries: Some(5),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.max_continuation_retries, 5);
}

// 11. merge_replaces_plugin_ids_when_patch_some
#[test]
fn merge_replaces_plugin_ids_when_patch_some() {
    let base = AgentSpec {
        plugin_ids: vec!["a".into(), "b".into(), "c".into()],
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        plugin_ids: Some(vec!["d".into()]),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.plugin_ids, vec!["d"]);
}

// 12a. merge_overrides_context_policy
#[test]
fn merge_overrides_context_policy() {
    let base = AgentSpec {
        context_policy: None,
        ..base_spec()
    };
    let policy = ContextWindowPolicy {
        max_context_tokens: 10_000,
        ..Default::default()
    };
    let patch = AgentSpecPatch {
        context_policy: Some(Some(policy.clone())),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.context_policy, Some(policy));
}

#[test]
fn merge_clears_nullable_fields_when_patch_value_is_null() {
    let base = AgentSpec {
        context_policy: Some(ContextWindowPolicy::default()),
        allowed_tools: Some(vec!["safe".into()]),
        allowed_tool_patterns: Some(vec!["safe:*".into()]),
        excluded_tools: Some(vec!["danger".into()]),
        excluded_tool_patterns: Some(vec!["danger:*".into()]),
        reasoning_effort: Some(ReasoningEffort::High),
        endpoint: Some(RemoteEndpoint {
            base_url: "https://example.com".into(),
            ..Default::default()
        }),
        ..base_spec()
    };

    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "context_policy": null,
        "allowed_tools": null,
        "allowed_tool_patterns": null,
        "excluded_tools": null,
        "excluded_tool_patterns": null,
        "reasoning_effort": null,
        "endpoint": null
    }))
    .expect("nullable fields must accept explicit null");

    let result = merge_agent_spec(base, patch);
    assert_eq!(result.context_policy, None);
    // Clearing BOTH allow fields triggers the deny-all normalization:
    // they end up as explicit empty lists so a JSON round-trip preserves
    // the deny-all matcher rather than re-firing the legacy
    // "absent = allow all" shim. See `merge_agent_spec` doc comment.
    assert_eq!(result.allowed_tools, Some(vec![]));
    assert_eq!(result.allowed_tool_patterns, Some(vec![]));
    assert_eq!(result.excluded_tools, None);
    assert_eq!(result.excluded_tool_patterns, None);
    assert_eq!(result.reasoning_effort, None);
    assert_eq!(result.endpoint, None);
}

#[test]
fn serde_preserves_nullable_field_clear_values() {
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "endpoint": null,
        "allowed_tools": null,
        "allowed_tool_patterns": null,
        "excluded_tool_patterns": null
    }))
    .expect("nullable fields must accept explicit null");

    let encoded = serde_json::to_value(&patch).expect("patch serializes");
    assert_eq!(encoded["endpoint"], Value::Null);
    assert_eq!(encoded["allowed_tools"], Value::Null);
    assert_eq!(encoded["allowed_tool_patterns"], Value::Null);
    assert_eq!(encoded["excluded_tool_patterns"], Value::Null);
}

// Tri-state coverage for the pattern fields — mirrors `allowed_tools`.
#[test]
fn merge_keeps_allowed_tool_patterns_when_patch_absent() {
    let base = AgentSpec {
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch = AgentSpecPatch::default();
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.allowed_tool_patterns, Some(vec!["mcp:*".into()]));
}

#[test]
fn merge_overrides_allowed_tool_patterns_when_patch_value() {
    let base = AgentSpec {
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        allowed_tool_patterns: Some(Some(vec!["other:*".into()])),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.allowed_tool_patterns, Some(vec!["other:*".into()]));
}

#[test]
fn merge_clears_allowed_tool_patterns_when_patch_null() {
    // Base has only the pattern field set; the literal field is None.
    // Clearing patterns drives both allow fields to None, which then trips
    // the deny-all normalization (see `merge_agent_spec` doc).
    let base = AgentSpec {
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "allowed_tool_patterns": null
    }))
    .expect("nullable pattern field accepts null");
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.allowed_tools, Some(vec![]));
    assert_eq!(result.allowed_tool_patterns, Some(vec![]));
}

#[test]
fn merge_clears_allowed_tool_patterns_preserves_literal_when_present() {
    // Base sets both allow fields. Patch only nulls the pattern field;
    // the literal field remains Some(..), so the deny-all normalization
    // does not fire and `allowed_tool_patterns` stays cleared to None.
    let base = AgentSpec {
        allowed_tools: Some(vec!["Bash".into()]),
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "allowed_tool_patterns": null
    }))
    .expect("nullable pattern field accepts null");
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.allowed_tools, Some(vec!["Bash".into()]));
    assert_eq!(result.allowed_tool_patterns, None);
}

#[test]
fn merge_keeps_excluded_tool_patterns_when_patch_absent() {
    let base = AgentSpec {
        excluded_tool_patterns: Some(vec!["danger:*".into()]),
        ..base_spec()
    };
    let patch = AgentSpecPatch::default();
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.excluded_tool_patterns, Some(vec!["danger:*".into()]));
}

#[test]
fn merge_overrides_excluded_tool_patterns_when_patch_value() {
    let base = AgentSpec {
        excluded_tool_patterns: Some(vec!["danger:*".into()]),
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        excluded_tool_patterns: Some(Some(vec!["other:*".into()])),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.excluded_tool_patterns, Some(vec!["other:*".into()]));
}

#[test]
fn merge_clears_excluded_tool_patterns_when_patch_null() {
    let base = AgentSpec {
        excluded_tool_patterns: Some(vec!["danger:*".into()]),
        ..base_spec()
    };
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "excluded_tool_patterns": null
    }))
    .expect("nullable pattern field accepts null");
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.excluded_tool_patterns, None);
}

// 12b. merge_overrides_active_hook_filter
#[test]
fn merge_overrides_active_hook_filter() {
    let mut base_filter = std::collections::HashSet::new();
    base_filter.insert("base".to_string());
    let base = AgentSpec {
        active_hook_filter: base_filter,
        ..base_spec()
    };
    let mut patch_filter = std::collections::HashSet::new();
    patch_filter.insert("patched".to_string());
    let patch = AgentSpecPatch {
        active_hook_filter: Some(patch_filter.clone()),
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.active_hook_filter, patch_filter);
}

// 12. merge_keeps_plugin_ids_when_patch_none
#[test]
fn merge_keeps_plugin_ids_when_patch_none() {
    let base = AgentSpec {
        plugin_ids: vec!["a".into(), "b".into()],
        ..base_spec()
    };
    let patch = AgentSpecPatch {
        plugin_ids: None,
        ..Default::default()
    };
    let result = merge_agent_spec(base, patch);
    assert_eq!(result.plugin_ids, vec!["a", "b"]);
}

// 13. merge_sections_per_key_overlay
#[test]
fn merge_sections_per_key_overlay() {
    let mut base_sections = HashMap::new();
    base_sections.insert("x".to_string(), json!(1));
    base_sections.insert("y".to_string(), json!(2));

    let base = AgentSpec {
        sections: base_sections,
        ..base_spec()
    };

    let mut patch_sections = HashMap::new();
    patch_sections.insert("y".to_string(), json!(99));

    let patch = AgentSpecPatch {
        sections: Some(patch_sections),
        ..Default::default()
    };

    let result = merge_agent_spec(base, patch);
    assert_eq!(result.sections.get("x"), Some(&json!(1)));
    assert_eq!(result.sections.get("y"), Some(&json!(99)));
    assert_eq!(result.sections.len(), 2);
}

// 14. merge_sections_null_value_deletes_key
#[test]
fn merge_sections_null_value_deletes_key() {
    let mut base_sections = HashMap::new();
    base_sections.insert("x".to_string(), json!(1));
    base_sections.insert("y".to_string(), json!(2));

    let base = AgentSpec {
        sections: base_sections,
        ..base_spec()
    };

    let mut patch_sections = HashMap::new();
    patch_sections.insert("y".to_string(), Value::Null);

    let patch = AgentSpecPatch {
        sections: Some(patch_sections),
        ..Default::default()
    };

    let result = merge_agent_spec(base, patch);
    assert_eq!(result.sections.get("x"), Some(&json!(1)));
    assert!(
        !result.sections.contains_key("y"),
        "y should have been deleted"
    );
    assert_eq!(result.sections.len(), 1);
}

// 15. merge_sections_keeps_base_when_patch_none
#[test]
fn merge_sections_keeps_base_when_patch_none() {
    let mut base_sections = HashMap::new();
    base_sections.insert("x".to_string(), json!(1));

    let base = AgentSpec {
        sections: base_sections,
        ..base_spec()
    };

    let patch = AgentSpecPatch {
        sections: None,
        ..Default::default()
    };

    let result = merge_agent_spec(base, patch);
    assert_eq!(result.sections.get("x"), Some(&json!(1)));
}

// 16. merge_preserves_pass_through_fields
#[test]
fn merge_preserves_pass_through_fields() {
    use awaken_runtime_contract::registry_spec::RemoteEndpoint;
    use std::collections::HashSet;

    let mut active_hook_filter = HashSet::new();
    active_hook_filter.insert("hook-plugin".to_string());

    let base = AgentSpec {
        id: "my-agent".into(),
        model_id: "m".into(),
        system_prompt: "p".into(),
        allowed_tools: Some(vec!["tool-a".into()]),
        excluded_tools: Some(vec!["tool-b".into()]),
        reasoning_effort: None,
        context_policy: None,
        endpoint: Some(RemoteEndpoint {
            base_url: "https://example.com".into(),
            ..Default::default()
        }),
        delegates: vec!["sub-agent".into()],
        active_hook_filter,
        registry: Some("cloud".into()),
        ..Default::default()
    };

    let base_value = serde_json::to_value(&base).unwrap();
    let result = merge_agent_spec(base, AgentSpecPatch::default());
    let result_value = serde_json::to_value(&result).unwrap();

    assert_eq!(base_value, result_value);
}

// ── deny-all round-trip pinning ─────────────────────────────────────────────

#[test]
fn merge_explicit_clear_of_both_allow_fields_yields_explicit_empty() {
    let base = AgentSpec {
        allowed_tools: Some(vec!["Bash".into()]),
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "allowed_tools": null,
        "allowed_tool_patterns": null,
    }))
    .expect("nullable allow fields accept null");
    let merged = merge_agent_spec(base, patch);
    assert_eq!(merged.allowed_tools, Some(vec![]));
    assert_eq!(merged.allowed_tool_patterns, Some(vec![]));
}

#[test]
fn deny_all_spec_survives_json_round_trip() {
    let base = AgentSpec {
        allowed_tools: Some(vec!["Bash".into()]),
        allowed_tool_patterns: Some(vec!["mcp:*".into()]),
        ..base_spec()
    };
    let patch: AgentSpecPatch = serde_json::from_value(json!({
        "allowed_tools": null,
        "allowed_tool_patterns": null,
    }))
    .expect("nullable allow fields accept null");
    let merged = merge_agent_spec(base, patch);

    // Round-trip through JSON — this is the path permission preview and
    // other consumers take after `merge_agent_spec`. Without the
    // normalization in `merge_agent_spec`, the AgentSpecRaw shim would
    // reinject `allowed_tool_patterns = ["*"]` here and flip deny-all
    // into allow-all.
    let raw = serde_json::to_value(&merged).expect("merged spec serializes");
    let parsed: AgentSpec = serde_json::from_value(raw).expect("merged spec re-parses");
    assert!(!parsed.tool_allowed("Bash"));
    assert!(!parsed.tool_allowed("mcp:weather"));
    assert!(!parsed.tool_allowed(""));
}

#[test]
fn deny_all_normalization_only_fires_when_base_starts_allow_all() {
    // Base built via Default (which triggers the legacy shim through
    // deserialize) has `allowed_tool_patterns = Some(vec!["*"])` and
    // `allowed_tools = None`. An empty patch must NOT collapse to
    // deny-all — the legacy shim path stays allow-all.
    let default_spec: AgentSpec = AgentSpec::default();
    assert_eq!(default_spec.allowed_tool_patterns, Some(vec!["*".into()]));
    let merged = merge_agent_spec(default_spec, AgentSpecPatch::default());
    assert!(merged.tool_allowed("anything"));
}
