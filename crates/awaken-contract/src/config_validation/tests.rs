use serde_json::json;

use super::*;

#[test]
fn validate_agent_spec_rejects_unknown_fields() {
    let err = validate_agent_spec(json!({
        "id": "a",
        "model_id": "m",
        "system_prompt": "s",
        "model": "legacy"
    }))
    .expect_err("unknown field must be rejected");
    assert!(err.to_string().contains("invalid agent spec"));
}

#[test]
fn validate_agent_spec_patch_rejects_unknown_fields() {
    let err = validate_agent_spec_patch(json!({"bogus": true}))
        .expect_err("unknown patch field must be rejected");
    assert!(err.to_string().contains("invalid agent spec patch"));
}

#[test]
fn validate_config_record_accepts_legacy_bare_spec() {
    let record = validate_config_record::<AgentSpec>(json!({
        "id": "a",
        "model_id": "m",
        "system_prompt": "s"
    }))
    .expect("legacy bare spec must decode");
    assert_eq!(record.spec.id, "a");
}

#[test]
fn validate_config_record_rejects_invalid_user_overrides() {
    let err = validate_config_record::<AgentSpec>(json!({
        "spec": {
            "id": "a",
            "model_id": "m",
            "system_prompt": "s"
        },
        "meta": {
            "source": {"kind": "builtin", "binary_version": "test"},
            "user_overrides": {"unknown_patch_field": true}
        }
    }))
    .expect_err("invalid overrides must fail validation");
    assert!(err.to_string().contains("invalid config record"));
}

#[test]
fn validate_provider_spec_rejects_unknown_and_empty_fields() {
    let err = validate_provider_spec(json!({
        "id": "p",
        "adapter": "openai",
        "future_top_level": true
    }))
    .expect_err("unknown provider fields must be rejected on write surfaces");
    assert!(err.to_string().contains("unknown field 'future_top_level'"));

    let err = validate_provider_spec(json!({
        "id": " ",
        "adapter": "openai"
    }))
    .expect_err("empty provider id must be rejected");
    assert!(err.to_string().contains("field 'id' cannot be empty"));

    let err = validate_provider_spec(json!({
        "id": "p",
        "adapter": ""
    }))
    .expect_err("empty provider adapter must be rejected");
    assert!(err.to_string().contains("field 'adapter' cannot be empty"));
}

#[test]
fn validate_model_spec_rejects_unknown_and_empty_fields() {
    let err = validate_model_spec(json!({
        "id": "m",
        "provider_id": "p",
        "upstream_model": "gpt-4",
        "future_top_level": true
    }))
    .expect_err("unknown model fields must be rejected");
    assert!(err.to_string().contains("unknown field 'future_top_level'"));

    let err = validate_model_spec(json!({
        "id": "m",
        "provider_id": " ",
        "upstream_model": "gpt-4"
    }))
    .expect_err("empty provider_id must be rejected");
    assert!(
        err.to_string()
            .contains("field 'provider_id' cannot be empty")
    );
}

#[test]
fn validate_model_spec_rejects_zero_context_window() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "context_window": 0
    }))
    .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("context_window"),
        "got: {err}"
    );
}

#[test]
fn validate_model_spec_rejects_zero_max_output_tokens() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "max_output_tokens": 0
    }))
    .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("max_output_tokens"),
        "got: {err}"
    );
}

#[test]
fn validate_model_spec_rejects_output_exceeding_context() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "context_window": 4000,
        "max_output_tokens": 8000
    }))
    .unwrap_err();
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("max_output_tokens") || msg.contains("context_window"),
        "expected error mentioning the offending fields, got: {err}"
    );
}

#[test]
fn validate_model_spec_accepts_output_equal_to_context() {
    validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "context_window": 4000,
        "max_output_tokens": 4000
    }))
    .expect("output_tokens == context_window must be allowed");
}

#[test]
fn validate_model_spec_rejects_blank_knowledge_cutoff() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m",
        "provider_id": "p",
        "upstream_model": "u",
        "knowledge_cutoff": "   "
    }))
    .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("knowledge_cutoff"),
        "expected error to mention knowledge_cutoff, got: {err}"
    );
}

#[test]
fn validate_skill_spec_accepts_valid_spec() {
    let spec = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": ["db_query", "mcp__db__*"],
        "arguments": [{"name": "dialect", "required": false}]
    }))
    .expect("valid skill spec");
    assert_eq!(spec.id, "db-management");
}

#[test]
fn validate_skill_spec_accepts_unicode_id_aligned_with_skill_md() {
    let spec = validate_skill_spec(json!({
        "id": "数据库",
        "name": "数据库",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL."
    }))
    .expect("unicode skill names accepted by SKILL.md should import");
    assert_eq!(spec.id, "数据库");
}

#[test]
fn validate_skill_spec_rejects_invalid_id_and_tools() {
    let err = validate_skill_spec(json!({
        "id": "DB",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL."
    }))
    .expect_err("uppercase id must fail");
    assert!(err.to_string().contains("must be lowercase"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": ["bad token"]
    }))
    .expect_err("whitespace in tool token must fail");
    assert!(err.to_string().contains("exactly one token"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": ["()"]
    }))
    .expect_err("empty scoped tool id must fail");
    assert!(err.to_string().contains("invalid allowed-tools token"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": ["Bash(command: \"git status\")"]
    }))
    .expect_err("DB-managed scoped tool grants are not supported yet");
    assert!(err.to_string().contains("scoped allowed_tools entry"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": ["/[invalid/"]
    }))
    .expect_err("invalid regex matcher must fail");
    assert!(err.to_string().contains("invalid allowed-tools pattern"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "allowed_tools": [r"mcp__db__*\"]
    }))
    .expect_err("invalid glob matcher must fail");
    assert!(err.to_string().contains("dangling escape"));
}

#[test]
fn validate_skill_spec_rejects_paths_and_duplicate_arguments() {
    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "paths": ["migrations/**"]
    }))
    .expect_err("paths are not supported yet");
    assert!(err.to_string().contains("paths are not supported"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "arguments": [{"name": "dialect"}, {"name": "dialect"}]
    }))
    .expect_err("duplicate arguments must fail");
    assert!(err.to_string().contains("duplicate argument name"));

    let err = validate_skill_spec(json!({
        "id": "db-management",
        "name": "Database Management",
        "description": "Helps with database operations",
        "instructions_md": "Inspect schema before running SQL.",
        "arguments": [{"name": " dialect "}]
    }))
    .expect_err("argument names must be trim-stable");
    assert!(err.to_string().contains("surrounding whitespace"));
}

#[test]
fn validate_unique_model_ids_accepts_distinct() {
    let specs = vec![
        ModelSpec::new("a", "p", "u1"),
        ModelSpec::new("b", "p", "u2"),
        ModelSpec::new("c", "p", "u3"),
    ];
    validate_unique_model_ids(&specs).expect("distinct ids must validate");
}

#[test]
fn validate_unique_model_ids_accepts_empty() {
    validate_unique_model_ids(&[]).expect("empty slice must validate");
}

#[test]
fn validate_unique_model_ids_rejects_duplicate() {
    let specs = vec![
        ModelSpec::new("dup", "p", "u1"),
        ModelSpec::new("dup", "p", "u2"),
    ];
    let err = validate_unique_model_ids(&specs).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("duplicate model id"), "got: {msg}");
    assert!(msg.contains("'dup'"), "expected id in error, got: {msg}");
}

#[test]
fn validate_unique_model_ids_returns_first_duplicate() {
    let specs = vec![
        ModelSpec::new("a", "p", "u1"),
        ModelSpec::new("b", "p", "u2"),
        ModelSpec::new("a", "p", "u3"),
        ModelSpec::new("b", "p", "u4"),
    ];
    let err = validate_unique_model_ids(&specs).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'a'"),
        "first dup must be reported, got: {msg}"
    );
    assert!(
        !msg.contains("'b'"),
        "only the first duplicate should be reported; got both: {msg}"
    );
}

#[test]
fn validate_unique_model_ids_keys_on_id_only_not_on_provider_or_upstream() {
    let specs = vec![
        ModelSpec::new("dup", "provider-a", "upstream-x"),
        ModelSpec::new("dup", "provider-b", "upstream-y"),
    ];
    let err = validate_unique_model_ids(&specs).unwrap_err();
    assert!(
        err.to_string().contains("duplicate model id"),
        "differing provider/upstream must not suppress the duplicate-id error, got: {err}"
    );
    assert!(err.to_string().contains("'dup'"));
}

#[test]
fn validate_unique_model_ids_reports_second_occurrence_in_n_way_duplicate() {
    let specs = vec![
        ModelSpec::new("x", "p", "u1"),
        ModelSpec::new("x", "p", "u2"), // duplicate detected here
        ModelSpec::new("x", "p", "u3"),
    ];
    let err = validate_unique_model_ids(&specs).unwrap_err();
    assert!(err.to_string().contains("'x'"));
}

#[test]
fn validate_model_spec_rejects_negative_input_price() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "input_token_price_per_million_usd": -10.0
    }))
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("input_token_price_per_million_usd")
    );
}

#[test]
fn validate_model_spec_rejects_negative_output_price() {
    let err = validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "output_token_price_per_million_usd": -1.0
    }))
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("output_token_price_per_million_usd")
    );
}

#[test]
fn validate_model_spec_rejects_non_finite_prices() {
    // JSON literally cannot encode NaN/Infinity (serde_json's `json!` macro
    // silently converts them to `null`, and strict parsing rejects the
    // `NaN`/`Infinity` tokens). Either layer is acceptable defense — the
    // invariant is "a non-finite f64 never reaches a validated ModelSpec".
    // First, prove the parse layer rejects raw `NaN`/`Infinity` tokens:
    for raw in [
        r#"{"id":"m","provider_id":"p","upstream_model":"u","input_token_price_per_million_usd": NaN}"#,
        r#"{"id":"m","provider_id":"p","upstream_model":"u","input_token_price_per_million_usd": Infinity}"#,
        r#"{"id":"m","provider_id":"p","upstream_model":"u","output_token_price_per_million_usd": -Infinity}"#,
    ] {
        assert!(
            serde_json::from_str::<serde_json::Value>(raw).is_err(),
            "JSON parser must reject non-finite literal: {raw}"
        );
    }
    // Second, prove the validation layer rejects non-finite values that
    // somehow reach it via direct `Value::Number` construction (defense
    // against in-memory construction by code that bypasses the parser).
    // `Number::from_f64` itself returns None for non-finite — confirm and
    // also exercise the validator helper directly with raw f64.
    assert!(serde_json::Number::from_f64(f64::NAN).is_none());
    assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
    assert!(
        super::reject_invalid_price("input_token_price_per_million_usd", Some(f64::NAN)).is_err()
    );
    assert!(
        super::reject_invalid_price("input_token_price_per_million_usd", Some(f64::INFINITY))
            .is_err()
    );
    assert!(
        super::reject_invalid_price(
            "output_token_price_per_million_usd",
            Some(f64::NEG_INFINITY)
        )
        .is_err()
    );
}

#[test]
fn validate_model_spec_accepts_zero_price() {
    validate_model_spec(serde_json::json!({
        "id": "m", "provider_id": "p", "upstream_model": "u",
        "input_token_price_per_million_usd": 0.0,
        "output_token_price_per_million_usd": 0.0
    }))
    .expect("zero is a valid price (free tier)");
}

#[test]
fn validate_model_spec_rejects_malformed_knowledge_cutoff() {
    for cutoff in [
        "yesterday",
        "2026",
        "2026-13",
        "2026-00-15",
        "2026-1-1",
        "2026/01",
        "2026-01-32",
    ] {
        let err = validate_model_spec(serde_json::json!({
            "id":"m","provider_id":"p","upstream_model":"u","knowledge_cutoff": cutoff
        }))
        .unwrap_err();
        assert!(
            err.to_string().contains("knowledge_cutoff"),
            "expected knowledge_cutoff rejection for {cutoff:?}, got: {err}"
        );
    }
}

#[test]
fn validate_model_spec_accepts_iso_cutoff_year_month_and_full_date() {
    for cutoff in ["2026-01", "2026-12", "2026-01-15", "2026-12-31"] {
        validate_model_spec(serde_json::json!({
            "id":"m","provider_id":"p","upstream_model":"u","knowledge_cutoff": cutoff
        }))
        .unwrap_or_else(|e| panic!("expected '{cutoff}' to validate, got: {e}"));
    }
}

#[test]
fn validate_model_spec_rejects_calendar_invalid_cutoff_dates() {
    // Shape-valid but calendar-invalid dates must be rejected.
    for cutoff in [
        "2026-02-31", // Feb never has 31
        "2026-02-30", // Feb never has 30
        "2026-04-31", // Apr has 30
        "2026-06-31", // Jun has 30
        "2026-09-31", // Sep has 30
        "2026-11-31", // Nov has 30
        "2026-02-29", // 2026 is not a leap year
        "2100-02-29", // century non-leap year
    ] {
        let err = validate_model_spec(serde_json::json!({
            "id":"m","provider_id":"p","upstream_model":"u","knowledge_cutoff": cutoff
        }))
        .unwrap_err();
        assert!(
            err.to_string().contains("knowledge_cutoff"),
            "expected calendar-invalid rejection for {cutoff:?}, got: {err}"
        );
    }
}

#[test]
fn validate_model_spec_accepts_valid_leap_day_cutoff() {
    // Feb 29 only in real leap years (div-by-4, except centuries unless div-by-400).
    for cutoff in ["2024-02-29", "2000-02-29", "2026-02-28"] {
        validate_model_spec(serde_json::json!({
            "id":"m","provider_id":"p","upstream_model":"u","knowledge_cutoff": cutoff
        }))
        .unwrap_or_else(|e| panic!("expected leap-valid '{cutoff}' to validate, got: {e}"));
    }
}

#[test]
fn validate_model_spec_rejects_duplicate_modalities_in_input() {
    let err = validate_model_spec(serde_json::json!({
        "id":"m","provider_id":"p","upstream_model":"u",
        "modalities": {"input": ["text", "text"]}
    }))
    .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"));
}

#[test]
fn validate_model_spec_rejects_duplicate_modalities_in_output() {
    let err = validate_model_spec(serde_json::json!({
        "id":"m","provider_id":"p","upstream_model":"u",
        "modalities": {"output": ["image", "image"]}
    }))
    .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"));
}

#[test]
fn validate_model_spec_accepts_empty_modalities_as_unspecified() {
    // Empty == "unspecified", explicitly OK
    validate_model_spec(serde_json::json!({
        "id":"m","provider_id":"p","upstream_model":"u",
        "modalities": {"input": [], "output": []}
    }))
    .expect("empty modalities means unspecified, must be allowed");
}
