//! The config JSON Schema is derived from the strong config type (run with
//! `--features schema`).
#![cfg(feature = "schema")]

#[test]
fn config_schema_is_derived_from_the_config_type() {
    let schema = awaken_ext_state_machine::config_schema();
    assert!(schema.is_object(), "a JSON Schema object");
    let text = schema.to_string();
    // The schema reflects the config type's fields — a single source of truth.
    assert!(
        text.contains("machines"),
        "schema exposes the machines field"
    );
    assert!(
        text.contains("continuation"),
        "schema exposes the continuation field"
    );
    let guide = schema["description"].as_str().expect("authoring guide");
    assert!(guide.contains("INCLUDE THE DESTINATION"));
    assert!(guide.contains("[read, written]"));
    assert!(guide.contains("cooldown_steps"));
    assert!(guide.contains("installed fact adapter"));

    let write = &schema["examples"][0]["machines"][0]["transitions"][1];
    assert_eq!(write["from"], serde_json::json!(["read", "written"]));
}
