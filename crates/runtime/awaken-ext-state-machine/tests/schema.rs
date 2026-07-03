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
}
