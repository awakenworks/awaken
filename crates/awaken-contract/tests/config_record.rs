use awaken_contract::{AgentSpec, ConfigRecord, RecordMeta, RecordSource};
use serde_json::json;

fn sample_agent_spec() -> AgentSpec {
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
        created_at: 1_000,
        updated_at: 2_000,
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
