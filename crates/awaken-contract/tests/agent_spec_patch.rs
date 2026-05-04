use std::collections::HashMap;

use awaken_contract::{AgentSpec, AgentSpecPatch, merge_agent_spec};
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
        sections: Some(HashMap::new()),
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
        plugin_ids: Some(vec!["plugin-a".into(), "plugin-b".into()]),
        sections: Some(sections),
        allowed_tools: Some(vec!["weather".into()]),
        excluded_tools: Some(vec!["dangerous".into()]),
        delegates: Some(vec!["delegate-a".into()]),
        reasoning_effort: None,
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
    use awaken_contract::registry_spec::RemoteEndpoint;
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
        created_at: Some(1_000_000),
        updated_at: Some(2_000_000),
        ..Default::default()
    };

    let base_value = serde_json::to_value(&base).unwrap();
    let result = merge_agent_spec(base, AgentSpecPatch::default());
    let result_value = serde_json::to_value(&result).unwrap();

    assert_eq!(base_value, result_value);
}
