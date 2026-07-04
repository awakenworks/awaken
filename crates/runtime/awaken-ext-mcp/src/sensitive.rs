//! Sensitive-field marking and redaction for MCP tool schemas.
//!
//! Sensitivity lives **in the tool's input schema** so it travels with the
//! [`ToolDescriptor`](awaken_runtime_contract::resolved::ToolDescriptor): any
//! holder of a descriptor and a call's arguments can redact without extra
//! plumbing, and the descriptor content hash pins the marking (a marking change
//! is a face change). Two sources merge into the same representation:
//!
//! - the server's own schema markers — the JSON Schema standard
//!   `"writeOnly": true` and the common `"format": "password"` hint;
//! - host configuration — per-tool field paths applied with
//!   [`mark_sensitive`], which injects the explicit `"x-sensitive": true`
//!   extension (a server cannot be trusted to mark everything, and the host
//!   may know better).
//!
//! The special handling is [`redact_arguments`]: a copy of the call arguments
//! with every sensitive value replaced by [`REDACTED`], for anything that
//! leaves the invocation path — logs, session event history, telemetry, UI.
//! The real arguments still flow to the server; only the recorded projection
//! is scrubbed.

use serde_json::Value;

/// The placeholder written over a sensitive value by [`redact_arguments`].
pub const REDACTED: &str = "[redacted]";

/// Whether a (sub)schema marks its value sensitive.
fn is_sensitive(schema: &Value) -> bool {
    schema.get("x-sensitive").and_then(Value::as_bool) == Some(true)
        || schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || schema.get("format").and_then(Value::as_str) == Some("password")
}

/// Inject `"x-sensitive": true` at each dotted property `path` (e.g.
/// `"auth.token"`). A segment names a key under `properties`; an array schema
/// is traversed through `items` transparently. Unknown paths are ignored — the
/// host may declare fields a server version no longer exposes.
pub fn mark_sensitive<S: AsRef<str>>(schema: &mut Value, paths: &[S]) {
    for path in paths {
        let segments: Vec<&str> = path.as_ref().split('.').collect();
        mark_path(schema, &segments);
    }
}

fn mark_path(schema: &mut Value, segments: &[&str]) {
    let Some(first) = segments.first() else {
        if let Some(obj) = schema.as_object_mut() {
            obj.insert("x-sensitive".to_string(), Value::Bool(true));
        }
        return;
    };
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    if let Some(child) = obj
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|props| props.get_mut(*first))
    {
        mark_path(child, &segments[1..]);
        return;
    }
    // Pass through an array schema and retry the same segment on its items.
    if let Some(items) = obj.get_mut("items") {
        mark_path(items, segments);
    }
}

/// The dotted paths of every sensitive field in `schema` (array items render
/// as `[]`, e.g. `keys[].value`). A sensitive subtree is reported once at its
/// root. Useful for policy decisions, e.g. forcing confirmation on any tool
/// that takes a secret.
pub fn sensitive_paths(schema: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_paths(schema, String::new(), &mut paths);
    paths
}

fn collect_paths(schema: &Value, prefix: String, paths: &mut Vec<String>) {
    if is_sensitive(schema) {
        if !prefix.is_empty() {
            paths.push(prefix);
        }
        return;
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in props {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            collect_paths(child, path, paths);
        }
    }
    if let Some(items) = schema.get("items") {
        collect_paths(items, format!("{prefix}[]"), paths);
    }
}

/// A copy of `arguments` with every value under a sensitive schema node
/// replaced by [`REDACTED`]. Values without a matching schema node (unknown
/// keys, missing `items`) pass through unchanged — redaction never invents
/// structure, it only scrubs what the schema declares.
pub fn redact_arguments(schema: &Value, arguments: &Value) -> Value {
    if is_sensitive(schema) {
        return Value::String(REDACTED.to_string());
    }
    match arguments {
        Value::Object(map) => {
            let props = schema.get("properties").and_then(Value::as_object);
            Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        let redacted = match props.and_then(|p| p.get(key)) {
                            Some(child) => redact_arguments(child, value),
                            None => value.clone(),
                        };
                        (key.clone(), redacted)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => match schema.get("items") {
            Some(child) => Value::Array(
                items
                    .iter()
                    .map(|item| redact_arguments(child, item))
                    .collect(),
            ),
            None => arguments.clone(),
        },
        _ => arguments.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "q": { "type": "string" },
                "token": { "type": "string", "writeOnly": true },
                "password": { "type": "string", "format": "password" },
                "auth": {
                    "type": "object",
                    "properties": {
                        "api_key": { "type": "string" },
                        "note": { "type": "string" }
                    }
                },
                "keys": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "value": { "type": "string" } }
                    }
                }
            }
        })
    }

    #[test]
    fn server_markers_are_recognized() {
        let mut paths = sensitive_paths(&schema());
        paths.sort();
        assert_eq!(paths, vec!["password".to_string(), "token".to_string()]);
    }

    #[test]
    fn mark_sensitive_injects_the_extension_at_nested_paths() {
        let mut schema = schema();
        mark_sensitive(&mut schema, &["auth.api_key", "keys.value"]);
        assert_eq!(
            schema["properties"]["auth"]["properties"]["api_key"]["x-sensitive"],
            true
        );
        // `keys.value` traverses the array's items transparently.
        assert_eq!(
            schema["properties"]["keys"]["items"]["properties"]["value"]["x-sensitive"],
            true
        );
        let paths = sensitive_paths(&schema);
        assert!(paths.contains(&"auth.api_key".to_string()));
        assert!(paths.contains(&"keys[].value".to_string()));
    }

    #[test]
    fn unknown_paths_are_ignored() {
        let mut before = schema();
        mark_sensitive(&mut before, &["missing.field"]);
        assert_eq!(before, schema());
    }

    #[test]
    fn redacts_marked_values_and_keeps_the_rest() {
        let mut schema = schema();
        mark_sensitive(&mut schema, &["auth.api_key"]);
        let arguments = json!({
            "q": "hello",
            "token": "tok-123",
            "password": "hunter2",
            "auth": { "api_key": "sk-live", "note": "keep" },
            "keys": [ { "value": "k1" } ],
            "extra": "unknown-key"
        });
        let redacted = redact_arguments(&schema, &arguments);
        assert_eq!(redacted["q"], "hello");
        assert_eq!(redacted["token"], REDACTED);
        assert_eq!(redacted["password"], REDACTED);
        assert_eq!(redacted["auth"]["api_key"], REDACTED);
        assert_eq!(redacted["auth"]["note"], "keep");
        assert_eq!(redacted["keys"][0]["value"], "k1");
        assert_eq!(redacted["extra"], "unknown-key");
    }

    #[test]
    fn a_sensitive_object_subtree_is_redacted_whole() {
        let mut schema = schema();
        mark_sensitive(&mut schema, &["auth"]);
        let arguments = json!({ "auth": { "api_key": "sk", "note": "n" } });
        let redacted = redact_arguments(&schema, &arguments);
        assert_eq!(redacted["auth"], REDACTED);
        // The subtree reports once at its root.
        assert!(sensitive_paths(&schema).contains(&"auth".to_string()));
    }

    #[test]
    fn sensitive_array_items_redact_per_element() {
        let mut schema = schema();
        mark_sensitive(&mut schema, &["keys.value"]);
        let arguments = json!({ "keys": [ { "value": "k1" }, { "value": "k2" } ] });
        let redacted = redact_arguments(&schema, &arguments);
        assert_eq!(redacted["keys"][0]["value"], REDACTED);
        assert_eq!(redacted["keys"][1]["value"], REDACTED);
    }
}
