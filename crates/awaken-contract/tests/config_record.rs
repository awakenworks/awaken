use awaken_contract::{
    AgentSpec, ConfigRecord, ProviderSpec, RecordMeta, RecordSource, decode_config_record,
    effective_config_record, effective_visible_config_records, validate_config_record,
};
use serde_json::json;

#[test]
fn record_meta_user_overrides_serde_round_trip() {
    let meta = RecordMeta {
        source: RecordSource::User,
        hidden: false,
        user_overrides: Some(json!({"x": 1})),
        created_at: 100,
        updated_at: 200,
        revision: 0,
    };
    let value = serde_json::to_value(&meta).expect("serialize must succeed");
    let decoded: RecordMeta = serde_json::from_value(value).expect("deserialize must succeed");
    assert_eq!(decoded.user_overrides, Some(json!({"x": 1})));
}

#[test]
fn record_meta_user_overrides_omitted_when_none() {
    let meta = RecordMeta::new_user();
    let value = serde_json::to_value(&meta).expect("serialize must succeed");
    let obj = value.as_object().expect("must be object");
    assert!(
        !obj.contains_key("user_overrides"),
        "user_overrides must be absent from JSON when None"
    );
}

#[test]
fn legacy_envelope_without_user_overrides_decodes_cleanly() {
    let value = json!({
        "spec": {
            "id": "x",
            "model_id": "y",
            "system_prompt": "z",
        },
        "meta": {
            "source": { "kind": "user" },
            "hidden": false,
            "created_at": 100,
            "updated_at": 200,
        }
    });

    let decoded =
        ConfigRecord::<AgentSpec>::from_value(value).expect("legacy envelope must decode cleanly");
    assert_eq!(
        decoded.meta.user_overrides, None,
        "user_overrides must default to None when absent from JSON"
    );
}

fn sample_agent_spec() -> AgentSpec {
    // `Default::default()` already applies the legacy "allow all" shim
    // (`allowed_tool_patterns = ["*"]`), so JSON round-trips are stable
    // without extra opt-in here.
    AgentSpec {
        id: "x".into(),
        model_id: "y".into(),
        system_prompt: "z".into(),
        ..Default::default()
    }
}

#[test]
fn envelope_round_trip_preserves_all_fields() {
    let meta = RecordMeta {
        source: RecordSource::Builtin {
            binary_version: "1.2.3".into(),
        },
        hidden: true,
        user_overrides: None,
        created_at: 1_000,
        updated_at: 2_000,
        revision: 3,
    };
    let record = ConfigRecord {
        spec: sample_agent_spec(),
        meta,
    };

    let value = record.to_value().expect("to_value must succeed");
    let decoded = ConfigRecord::<AgentSpec>::from_value(value).expect("from_value must succeed");

    // AgentSpec does not implement PartialEq; compare via JSON instead.
    let original_spec_json = serde_json::to_value(&record.spec).unwrap();
    let decoded_spec_json = serde_json::to_value(&decoded.spec).unwrap();
    assert_eq!(decoded_spec_json, original_spec_json);
    assert_eq!(decoded.meta, record.meta);
}

#[test]
fn decode_envelope_json_containing_spec_and_meta() {
    let value = json!({
        "spec": {
            "id": "x",
            "model_id": "y",
            "system_prompt": "z",
        },
        "meta": {
            "source": { "kind": "user" },
            "hidden": false,
            "created_at": 500,
            "updated_at": 600,
        }
    });

    let decoded = ConfigRecord::<AgentSpec>::from_value(value).expect("from_value must succeed");
    assert_eq!(decoded.spec.id, "x");
    assert_eq!(decoded.spec.model_id, "y");
    assert_eq!(decoded.meta.source, RecordSource::User);
    assert!(!decoded.meta.hidden);
    assert_eq!(decoded.meta.created_at, 500);
    assert_eq!(decoded.meta.updated_at, 600);
}

#[test]
fn decode_bare_spec_json_yields_legacy_user_record() {
    let spec = sample_agent_spec();
    let bare = serde_json::to_value(&spec).expect("serialize must succeed");

    let decoded = ConfigRecord::<AgentSpec>::from_value(bare).expect("from_value must succeed");
    assert_eq!(decoded.meta.source, RecordSource::User);
    assert!(!decoded.meta.hidden);
    assert_eq!(decoded.meta.created_at, 0);
    assert_eq!(decoded.meta.updated_at, 0);
}

#[test]
fn decode_envelope_tolerates_unknown_extra_fields_under_meta() {
    let value = json!({
        "spec": {
            "id": "x",
            "model_id": "y",
            "system_prompt": "z",
        },
        "meta": {
            "source": { "kind": "user" },
            "hidden": false,
            "created_at": 100,
            "updated_at": 200,
            "some_future_field": "x",
        }
    });

    let result = ConfigRecord::<AgentSpec>::from_value(value);
    assert!(
        result.is_ok(),
        "unknown fields under meta must not cause failure"
    );
}

#[test]
fn hidden_defaults_to_false_when_absent() {
    let value = json!({
        "spec": {
            "id": "x",
            "model_id": "y",
            "system_prompt": "z",
        },
        "meta": {
            "source": { "kind": "user" },
            "created_at": 0,
            "updated_at": 0,
        }
    });

    let decoded = ConfigRecord::<AgentSpec>::from_value(value).expect("from_value must succeed");
    assert!(!decoded.meta.hidden);
}

#[test]
fn record_source_tagged_enum_round_trip() {
    let builtin = RecordSource::Builtin {
        binary_version: "X".into(),
    };
    let builtin_json = serde_json::to_value(&builtin).expect("serialize must succeed");
    assert_eq!(builtin_json["kind"], "builtin");
    assert_eq!(builtin_json["binary_version"], "X");

    let user = RecordSource::User;
    let user_json = serde_json::to_value(&user).expect("serialize must succeed");
    assert_eq!(user_json["kind"], "user");
    // User variant must not have extra fields
    assert!(user_json.as_object().is_some_and(|m| m.len() == 1));
}

#[test]
fn encoder_always_emits_envelope() {
    let record = ConfigRecord {
        spec: sample_agent_spec(),
        meta: RecordMeta::new_user(),
    };

    let value = record.to_value().expect("to_value must succeed");
    let obj = value.as_object().expect("must be an object");
    assert!(obj.contains_key("spec"), "envelope must contain 'spec' key");
    assert!(obj.contains_key("meta"), "envelope must contain 'meta' key");
}

#[test]
fn new_user_and_new_builtin_set_non_zero_timestamps() {
    let user_meta = RecordMeta::new_user();
    assert!(
        user_meta.created_at > 0,
        "new_user created_at must be non-zero"
    );
    assert!(
        user_meta.updated_at > 0,
        "new_user updated_at must be non-zero"
    );

    let builtin_meta = RecordMeta::new_builtin("2.0.0");
    assert!(
        builtin_meta.created_at > 0,
        "new_builtin created_at must be non-zero"
    );
    assert!(
        builtin_meta.updated_at > 0,
        "new_builtin updated_at must be non-zero"
    );
}

#[test]
fn effective_config_record_applies_agent_overrides() {
    let mut meta = RecordMeta::new_builtin("v1");
    meta.user_overrides = Some(json!({"system_prompt": "patched"}));
    let record = ConfigRecord {
        spec: sample_agent_spec(),
        meta,
    };

    let effective = effective_config_record(record).expect("overrides must merge");
    assert_eq!(effective.system_prompt, "patched");
    assert_eq!(effective.model_id, "y");
}

#[test]
fn decode_config_record_does_not_validate_user_overrides() {
    let mut meta = RecordMeta::new_builtin("v1");
    meta.user_overrides = Some(json!({"unknown_patch_field": true}));
    let record = ConfigRecord {
        spec: sample_agent_spec(),
        meta,
    };

    let decoded = decode_config_record::<AgentSpec>(record.to_value().unwrap())
        .expect("decode-only API accepts opaque overrides");
    assert_eq!(
        decoded.meta.user_overrides,
        Some(json!({"unknown_patch_field": true}))
    );
}

#[test]
fn validate_config_record_rejects_invalid_user_overrides() {
    let mut meta = RecordMeta::new_builtin("v1");
    meta.user_overrides = Some(json!({"unknown_patch_field": true}));
    let record = ConfigRecord {
        spec: sample_agent_spec(),
        meta,
    };

    let err = validate_config_record::<AgentSpec>(record.to_value().unwrap())
        .expect_err("invalid overrides must fail validation");
    assert!(err.to_string().contains("overrides"));
}

#[test]
fn effective_config_record_rejects_overrides_for_non_patchable_specs() {
    let mut meta = RecordMeta::new_builtin("v1");
    meta.user_overrides = Some(json!({"adapter": "patched"}));
    let record = ConfigRecord {
        spec: ProviderSpec {
            id: "p".into(),
            adapter: "openai".into(),
            ..Default::default()
        },
        meta,
    };

    let err = effective_config_record(record).expect_err("provider overrides must be rejected");
    assert!(err.to_string().contains("overrides"));
}

#[test]
fn effective_visible_config_records_skips_hidden_records() {
    let visible = ConfigRecord {
        spec: sample_agent_spec(),
        meta: RecordMeta::new_user(),
    };
    let mut hidden_meta = RecordMeta::new_user();
    hidden_meta.hidden = true;
    let hidden = ConfigRecord {
        spec: AgentSpec {
            id: "hidden".into(),
            ..sample_agent_spec()
        },
        meta: hidden_meta,
    };

    let values = vec![visible.to_value().unwrap(), hidden.to_value().unwrap()];
    let records: Vec<AgentSpec> =
        effective_visible_config_records(values).expect("records must decode");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "x");
}
