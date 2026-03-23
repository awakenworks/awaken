use std::fmt;

use serde_json::Value;

use super::{MESSAGE_KEYS, SUPPORTED_VERSION};

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

/// Validate an array of A2UI messages.
///
/// Returns errors found (at most one per message).
/// Empty `Vec` means all messages are valid.
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
            ));
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
            ));
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

    // surfaceId required on all message types
    match body.get("surfaceId").and_then(Value::as_str) {
        Some("") => return err(&format!("\"{type_key}.surfaceId\" must not be empty")),
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
        "updateComponents" => match body.get("components").and_then(Value::as_array) {
            Some(arr) if arr.is_empty() => {
                return err("\"updateComponents.components\" must not be empty");
            }
            Some(arr) => {
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
        },
        "updateDataModel" | "deleteSurface" => {
            // surfaceId already checked above
        }
        _ => unreachable!(),
    }

    None
}
