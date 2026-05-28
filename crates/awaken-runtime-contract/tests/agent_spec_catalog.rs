//! Migration shim coverage for `AgentSpec` tool catalog fields.
//!
//! These tests pin down the legacy "absent catalog = allow all" default
//! applied by `AgentSpec`'s `Deserialize` impl via `AgentSpecRaw`.
//!
//! Uses JSON (already a contract dep) rather than YAML to avoid pulling in
//! a new transitive dev-dep just for shape tests.

use awaken_runtime_contract::registry_spec::{AgentSpec, IssueSeverity, ValidationIssue};

fn spec_from_json(src: &str) -> AgentSpec {
    serde_json::from_str(src).expect("json parses to AgentSpec")
}

const MIN_SPEC: &str = r#"{
    "id": "a",
    "model_id": "m",
    "system_prompt": ""
}"#;

/// Field assignments only (no enclosing braces) so tests can splice in
/// additional fields via `format!`.
const MIN_SPEC_FIELDS: &str = r#""id": "a", "model_id": "m", "system_prompt": """#;

#[test]
fn absent_catalog_defaults_to_allow_all_patterns() {
    let spec = spec_from_json(MIN_SPEC);
    assert_eq!(spec.allowed_tools, None);
    assert_eq!(
        spec.allowed_tool_patterns.as_deref(),
        Some(&["*".to_string()][..]),
        "absent catalog should inject [*] for back-compat",
    );
    assert_eq!(spec.excluded_tools, None);
    assert_eq!(
        spec.excluded_tool_patterns, None,
        "exclusion default is empty, not [*]",
    );
}

#[test]
fn explicit_empty_literals_does_not_trigger_default() {
    let src = r#"{
        "id": "a",
        "model_id": "m",
        "system_prompt": "",
        "allowed_tools": []
    }"#;
    let spec = spec_from_json(src);
    assert_eq!(spec.allowed_tools.as_deref(), Some(&[] as &[String]));
    assert_eq!(
        spec.allowed_tool_patterns, None,
        "user explicitly opted in to literal-only catalog",
    );
}

#[test]
fn explicit_patterns_does_not_trigger_default() {
    let src = r#"{
        "id": "a",
        "model_id": "m",
        "system_prompt": "",
        "allowed_tool_patterns": ["mcp:*"]
    }"#;
    let spec = spec_from_json(src);
    assert_eq!(spec.allowed_tools, None);
    assert_eq!(
        spec.allowed_tool_patterns.as_deref(),
        Some(&["mcp:*".to_string()][..]),
    );
}

#[test]
fn excluded_defaults_remain_none() {
    let spec = spec_from_json(MIN_SPEC);
    assert!(spec.excluded_tools.is_none());
    assert!(spec.excluded_tool_patterns.is_none());
}

#[test]
fn legacy_literal_only_config_round_trips() {
    let src = r#"{
        "id": "a",
        "model_id": "m",
        "system_prompt": "",
        "allowed_tools": ["Bash", "Read"]
    }"#;
    let spec = spec_from_json(src);
    assert_eq!(
        spec.allowed_tools.as_deref(),
        Some(&["Bash".to_string(), "Read".to_string()][..]),
    );
    assert_eq!(
        spec.allowed_tool_patterns, None,
        "presence of literals = explicit, no default needed",
    );
}

// ---------------------------------------------------------------------------
// validate_catalog() — Task 6 diagnostic surface
// ---------------------------------------------------------------------------

#[test]
fn star_in_literal_field_yields_warning() {
    let spec = spec_from_json(&format!(
        r#"{{ {MIN_SPEC_FIELDS}, "allowed_tools": ["mcp:*"] }}"#
    ));
    let issues: Vec<ValidationIssue> = spec.validate_catalog();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].severity, IssueSeverity::Warning);
    assert_eq!(issues[0].field, "allowed_tools");
    assert_eq!(issues[0].entry, "mcp:*");
}

#[test]
fn escaped_star_in_literal_field_is_fine() {
    let spec = spec_from_json(&format!(
        r#"{{ {MIN_SPEC_FIELDS}, "allowed_tools": ["foo\\*bar"] }}"#
    ));
    assert!(
        spec.validate_catalog().is_empty(),
        r"escaped \* is a literal *, not a wildcard"
    );
}

#[test]
fn dangling_escape_in_pattern_field_yields_error() {
    let spec = spec_from_json(&format!(
        r#"{{ {MIN_SPEC_FIELDS}, "allowed_tool_patterns": ["foo\\"] }}"#
    ));
    let issues = spec.validate_catalog();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].severity, IssueSeverity::Error);
    assert_eq!(issues[0].field, "allowed_tool_patterns");
}

#[test]
fn empty_pattern_yields_error() {
    let spec = spec_from_json(&format!(
        r#"{{ {MIN_SPEC_FIELDS}, "allowed_tool_patterns": [""] }}"#
    ));
    let issues = spec.validate_catalog();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].severity, IssueSeverity::Error);
}

#[test]
fn well_formed_catalog_yields_no_issues() {
    let spec = spec_from_json(&format!(
        r#"{{ {MIN_SPEC_FIELDS},
              "allowed_tools": ["Bash"],
              "allowed_tool_patterns": ["mcp:*"],
              "excluded_tools": [],
              "excluded_tool_patterns": ["dangerous-*"] }}"#
    ));
    assert!(spec.validate_catalog().is_empty());
}

// ---------------------------------------------------------------------------
// Absent vs explicit-null distinction at deserialize time
//
// These exercise the DIRECT-PUT path through `AgentSpec::deserialize` (no
// merge involved) to pin down that the legacy shim only fires for truly
// absent allow fields, not for explicit `null`.
// ---------------------------------------------------------------------------

#[test]
fn legacy_yaml_with_absent_catalog_still_allows_all() {
    // No allow fields anywhere — legacy YAML / pre-catalog config. The
    // shim must fire so existing configs keep behaving as before.
    let spec = spec_from_json(MIN_SPEC);
    assert!(spec.tool_allowed("Bash"));
    assert!(spec.tool_allowed("mcp:weather"));
    assert!(spec.tool_allowed("anything-else"));
}

#[test]
fn direct_put_explicit_null_allow_fields_denies_all() {
    let json = serde_json::json!({
        "id": "a",
        "model_id": "m",
        "system_prompt": "",
        "allowed_tools": null,
        "allowed_tool_patterns": null,
    });
    let spec: AgentSpec = serde_json::from_value(json).expect("explicit null parses");
    // User wrote `null` on both allow fields — that's "no allow rules".
    // The legacy shim must NOT fire.
    assert_eq!(spec.allowed_tools, None);
    assert_eq!(spec.allowed_tool_patterns, None);
    assert!(
        !spec.tool_allowed("Bash"),
        "explicit null+null should deny all tools, not allow all"
    );
    assert!(!spec.tool_allowed("mcp:weather"));
    assert!(!spec.tool_allowed(""));
}

#[test]
fn explicit_null_round_trips_through_serialize_then_deserialize() {
    // Build a deny-all spec directly (no merge path), serialize, parse
    // the value back. Without the deserialize fix (or without dropping
    // `skip_serializing_if` from the allow fields), the round trip would
    // re-fire the legacy shim and flip deny-all into allow-all.
    let spec = AgentSpec {
        id: "deny".into(),
        model_id: "m".into(),
        system_prompt: String::new(),
        allowed_tools: None,
        allowed_tool_patterns: None,
        ..Default::default()
    };
    // Sanity: this raw spec already denies all because both allow fields
    // are None (the matcher requires a hit in at least one allow set).
    assert!(!spec.tool_allowed("Bash"));

    let value = serde_json::to_value(&spec).expect("spec serializes");
    // Both allow fields should be emitted as JSON `null`, not omitted —
    // that's what carries the deny-all intent across the round trip.
    assert_eq!(value["allowed_tools"], serde_json::Value::Null);
    assert_eq!(value["allowed_tool_patterns"], serde_json::Value::Null);

    let parsed: AgentSpec = serde_json::from_value(value).expect("spec re-parses");
    assert!(
        !parsed.tool_allowed("Bash"),
        "deny-all must survive a serialize→deserialize round trip"
    );
    assert!(!parsed.tool_allowed("mcp:anything"));
}

#[test]
fn explicit_empty_array_distinguishable_from_explicit_null() {
    // `Some(vec![])` after a round trip must still be `Some(vec![])`,
    // not `None`. Both deny all, but the structural difference matters
    // for downstream tooling that reasons about user intent (e.g. UIs
    // that show "empty list set" vs "field cleared").
    let spec = AgentSpec {
        id: "deny".into(),
        model_id: "m".into(),
        system_prompt: String::new(),
        allowed_tools: Some(Vec::new()),
        allowed_tool_patterns: Some(Vec::new()),
        ..Default::default()
    };
    let value = serde_json::to_value(&spec).expect("spec serializes");
    assert_eq!(value["allowed_tools"], serde_json::json!([]));
    assert_eq!(value["allowed_tool_patterns"], serde_json::json!([]));

    let parsed: AgentSpec = serde_json::from_value(value).expect("spec re-parses");
    assert_eq!(parsed.allowed_tools, Some(Vec::new()));
    assert_eq!(parsed.allowed_tool_patterns, Some(Vec::new()));
    assert!(!parsed.tool_allowed("Bash"));
}

#[test]
fn mixed_explicit_one_null_one_value() {
    // `null` on one allow field and a real list on the other must keep
    // both as the user wrote them — `None` literal, `Some(vec)` patterns.
    let json = serde_json::json!({
        "id": "mixed",
        "model_id": "m",
        "system_prompt": "",
        "allowed_tools": null,
        "allowed_tool_patterns": ["mcp:*"],
    });
    let spec: AgentSpec = serde_json::from_value(json).expect("mixed shape parses");
    assert_eq!(spec.allowed_tools, None);
    assert_eq!(
        spec.allowed_tool_patterns.as_deref(),
        Some(&["mcp:*".to_string()][..]),
    );
    // Pattern allow still works.
    assert!(spec.tool_allowed("mcp:weather"));
    // Literal allow path is empty — anything not matching the pattern
    // remains denied.
    assert!(!spec.tool_allowed("Bash"));
}
