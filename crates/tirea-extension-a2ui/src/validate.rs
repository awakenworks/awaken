//! A2UI message validation.
//!
//! Validates structural constraints of A2UI v0.9 messages without requiring
//! a full catalog JSON Schema. This catches the most common LLM generation
//! errors (missing version, unknown message type, missing surfaceId, empty
//! component list) while remaining catalog-agnostic.

use serde_json::Value;
use std::fmt;

/// A2UI validation error.
#[derive(Debug, Clone)]
pub struct A2uiValidationError {
    pub index: usize,
    pub message: String,
}

impl fmt::Display for A2uiValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message[{}]: {}", self.index, self.message)
    }
}

const SUPPORTED_VERSION: &str = "v0.9";
const MESSAGE_KEYS: &[&str] = &[
    "createSurface",
    "updateComponents",
    "updateDataModel",
    "deleteSurface",
];

/// Validate an array of A2UI messages.
///
/// Returns the first batch of errors found (at most one per message).
/// An empty `Vec` means all messages are valid.
pub fn validate_a2ui_messages(messages: &[Value]) -> Vec<A2uiValidationError> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| validate_single(i, msg))
        .collect()
}

fn validate_single(index: usize, msg: &Value) -> Option<A2uiValidationError> {
    let err = |m: &str| {
        Some(A2uiValidationError {
            index,
            message: m.to_string(),
        })
    };

    let obj = match msg.as_object() {
        Some(o) => o,
        None => return err("expected a JSON object"),
    };

    // version check
    match obj.get("version").and_then(Value::as_str) {
        Some(v) if v == SUPPORTED_VERSION => {}
        Some(v) => {
            return err(&format!(
                "unsupported version \"{v}\", expected \"{SUPPORTED_VERSION}\""
            ))
        }
        None => return err("missing required field \"version\""),
    }

    // exactly one message-type key
    let type_keys: Vec<&&str> = MESSAGE_KEYS
        .iter()
        .filter(|k| obj.contains_key(**k))
        .collect();
    match type_keys.len() {
        0 => {
            return err(&format!(
                "missing message type; expected one of: {}",
                MESSAGE_KEYS.join(", ")
            ))
        }
        1 => {}
        _ => {
            let names: Vec<&str> = type_keys.into_iter().copied().collect();
            return err(&format!(
                "multiple message types in one object: {}",
                names.join(", ")
            ));
        }
    }

    let type_key = type_keys[0];
    let body = match obj.get(*type_key).and_then(Value::as_object) {
        Some(b) => b,
        None => return err(&format!("\"{type_key}\" must be a JSON object")),
    };

    // surfaceId is required on all message types
    match body.get("surfaceId").and_then(Value::as_str) {
        Some(s) if s.is_empty() => {
            return err(&format!("\"{type_key}.surfaceId\" must not be empty"))
        }
        Some(_) => {}
        None => return err(&format!("\"{type_key}.surfaceId\" is required")),
    }

    // type-specific checks
    match *type_key {
        "createSurface" => {
            if body.get("catalogId").and_then(Value::as_str).is_none() {
                return err("\"createSurface.catalogId\" is required");
            }
        }
        "updateComponents" => {
            match body.get("components").and_then(Value::as_array) {
                Some(arr) if arr.is_empty() => {
                    return err("\"updateComponents.components\" must not be empty");
                }
                Some(arr) => {
                    // each component must have an "id" and "component" field
                    for (ci, comp) in arr.iter().enumerate() {
                        let comp_obj = match comp.as_object() {
                            Some(o) => o,
                            None => {
                                return err(&format!(
                                    "\"updateComponents.components[{ci}]\" must be a JSON object"
                                ));
                            }
                        };
                        if comp_obj.get("id").and_then(Value::as_str).is_none() {
                            return err(&format!(
                                "\"updateComponents.components[{ci}].id\" is required"
                            ));
                        }
                        if comp_obj.get("component").and_then(Value::as_str).is_none() {
                            return err(&format!(
                                "\"updateComponents.components[{ci}].component\" is required"
                            ));
                        }
                    }
                }
                None => {
                    return err("\"updateComponents.components\" is required and must be an array");
                }
            }
        }
        "updateDataModel" | "deleteSurface" => {
            // surfaceId already checked above — no further required fields
        }
        _ => unreachable!(),
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_create_surface() {
        let msgs = vec![json!({
            "version": "v0.9",
            "createSurface": {
                "surfaceId": "s1",
                "catalogId": "https://a2ui.org/specification/v0_9/basic_catalog.json"
            }
        })];
        assert!(validate_a2ui_messages(&msgs).is_empty());
    }

    #[test]
    fn valid_update_components() {
        let msgs = vec![json!({
            "version": "v0.9",
            "updateComponents": {
                "surfaceId": "s1",
                "components": [
                    {"id": "root", "component": "Card", "child": "col"},
                    {"id": "col", "component": "Column", "children": ["title"]},
                    {"id": "title", "component": "Text", "text": "Hello"}
                ]
            }
        })];
        assert!(validate_a2ui_messages(&msgs).is_empty());
    }

    #[test]
    fn valid_update_data_model() {
        let msgs = vec![json!({
            "version": "v0.9",
            "updateDataModel": {
                "surfaceId": "s1",
                "path": "/user",
                "value": {"name": "Alice"}
            }
        })];
        assert!(validate_a2ui_messages(&msgs).is_empty());
    }

    #[test]
    fn valid_delete_surface() {
        let msgs = vec![json!({
            "version": "v0.9",
            "deleteSurface": {"surfaceId": "s1"}
        })];
        assert!(validate_a2ui_messages(&msgs).is_empty());
    }

    #[test]
    fn rejects_missing_version() {
        let msgs = vec![json!({
            "createSurface": {"surfaceId": "s1", "catalogId": "c"}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("version"));
    }

    #[test]
    fn rejects_wrong_version() {
        let msgs = vec![json!({
            "version": "v0.8",
            "createSurface": {"surfaceId": "s1", "catalogId": "c"}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unsupported version"));
    }

    #[test]
    fn rejects_no_message_type() {
        let msgs = vec![json!({"version": "v0.9"})];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("missing message type"));
    }

    #[test]
    fn rejects_multiple_message_types() {
        let msgs = vec![json!({
            "version": "v0.9",
            "createSurface": {"surfaceId": "s1", "catalogId": "c"},
            "deleteSurface": {"surfaceId": "s1"}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("multiple message types"));
    }

    #[test]
    fn rejects_missing_surface_id() {
        let msgs = vec![json!({
            "version": "v0.9",
            "deleteSurface": {}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("surfaceId"));
    }

    #[test]
    fn rejects_empty_surface_id() {
        let msgs = vec![json!({
            "version": "v0.9",
            "createSurface": {"surfaceId": "", "catalogId": "c"}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("must not be empty"));
    }

    #[test]
    fn rejects_missing_catalog_id() {
        let msgs = vec![json!({
            "version": "v0.9",
            "createSurface": {"surfaceId": "s1"}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("catalogId"));
    }

    #[test]
    fn rejects_empty_components() {
        let msgs = vec![json!({
            "version": "v0.9",
            "updateComponents": {"surfaceId": "s1", "components": []}
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("must not be empty"));
    }

    #[test]
    fn rejects_component_missing_id() {
        let msgs = vec![json!({
            "version": "v0.9",
            "updateComponents": {
                "surfaceId": "s1",
                "components": [{"component": "Text", "text": "hi"}]
            }
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("components[0].id"));
    }

    #[test]
    fn rejects_component_missing_component_type() {
        let msgs = vec![json!({
            "version": "v0.9",
            "updateComponents": {
                "surfaceId": "s1",
                "components": [{"id": "root"}]
            }
        })];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("components[0].component"));
    }

    #[test]
    fn reports_per_message_errors() {
        let msgs = vec![
            json!({"version": "v0.9", "deleteSurface": {"surfaceId": "ok"}}),
            json!({"version": "v0.9"}),
            json!({"version": "v0.9", "deleteSurface": {"surfaceId": "ok"}}),
            json!(42),
        ];
        let errs = validate_a2ui_messages(&msgs);
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].index, 1);
        assert_eq!(errs[1].index, 3);
    }
}
