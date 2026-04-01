use std::fmt;

use serde_json::Value;

use super::MESSAGE_KEYS;

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

fn single_component_entry<'a>(
    index: usize,
    comp_obj: &'a serde_json::Map<String, Value>,
) -> Option<Result<(&'a str, &'a serde_json::Map<String, Value>), A2uiValidationError>> {
    let err = |message: String| Err(A2uiValidationError { index, message });

    let component = match comp_obj.get("component").and_then(Value::as_object) {
        Some(component) => component,
        None => return None,
    };

    if component.len() != 1 {
        return Some(err(
            "\"surfaceUpdate.components[*].component\" must contain exactly one component payload"
                .to_string(),
        ));
    }

    // len() == 1 was verified above, so iter().next() is guaranteed Some.
    let Some((name, payload)) = component.iter().next() else {
        return Some(err(
            "\"surfaceUpdate.components[*].component\" must contain exactly one component payload"
                .to_string(),
        ));
    };
    let payload_obj = match payload.as_object() {
        Some(payload_obj) => payload_obj,
        None => {
            return Some(err(format!(
                "\"surfaceUpdate.components[*].component.{name}\" must be a JSON object"
            )));
        }
    };

    Some(Ok((name.as_str(), payload_obj)))
}

fn validate_component_payload(
    index: usize,
    component_index: usize,
    comp_obj: &serde_json::Map<String, Value>,
) -> Option<A2uiValidationError> {
    let err = |message: String| Some(A2uiValidationError { index, message });

    let Some(component_entry) = single_component_entry(index, comp_obj) else {
        return None;
    };
    let (name, payload) = match component_entry {
        Ok(entry) => entry,
        Err(error) => return Some(error),
    };

    match name {
        "Text" => {
            if !matches!(payload.get("text"), Some(Value::Object(_))) {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.Text.text\" must be a JSON object"
                ));
            }
        }
        "Button" => {
            let Some(action) = payload.get("action").and_then(Value::as_object) else {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.Button.action\" must be a JSON object"
                ));
            };
            if action.get("name").and_then(Value::as_str).is_none() {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.Button.action.name\" is required"
                ));
            }
            if let Some(context) = action.get("context") {
                let Some(entries) = context.as_array() else {
                    return err(format!(
                        "\"surfaceUpdate.components[{component_index}].component.Button.action.context\" must be an array"
                    ));
                };
                for (entry_index, entry) in entries.iter().enumerate() {
                    let Some(entry_obj) = entry.as_object() else {
                        return err(format!(
                            "\"surfaceUpdate.components[{component_index}].component.Button.action.context[{entry_index}]\" must be a JSON object"
                        ));
                    };
                    if entry_obj.get("key").and_then(Value::as_str).is_none() {
                        return err(format!(
                            "\"surfaceUpdate.components[{component_index}].component.Button.action.context[{entry_index}].key\" is required"
                        ));
                    }
                    if !matches!(entry_obj.get("value"), Some(Value::Object(_))) {
                        return err(format!(
                            "\"surfaceUpdate.components[{component_index}].component.Button.action.context[{entry_index}].value\" must be a JSON object"
                        ));
                    }
                }
            }
        }
        "MultipleChoice" => {
            if payload.contains_key("choices")
                || payload.contains_key("label")
                || payload.contains_key("value")
            {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.MultipleChoice\" must use v0.8 fields `options` and `selections`"
                ));
            }
            if payload.get("options").and_then(Value::as_array).is_none() {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.MultipleChoice.options\" is required"
                ));
            }
            if !matches!(payload.get("selections"), Some(Value::Object(_))) {
                return err(format!(
                    "\"surfaceUpdate.components[{component_index}].component.MultipleChoice.selections\" must be a JSON object"
                ));
            }
        }
        _ => {}
    }

    None
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
        "beginRendering" => match body.get("root").and_then(Value::as_str) {
            Some("") => return err("\"beginRendering.root\" must not be empty"),
            Some(_) => {}
            None => return err("\"beginRendering.root\" is required"),
        },
        "surfaceUpdate" => match body.get("components").and_then(Value::as_array) {
            Some(arr) if arr.is_empty() => {
                return err("\"surfaceUpdate.components\" must not be empty");
            }
            Some(arr) => {
                for (ci, comp) in arr.iter().enumerate() {
                    let comp_obj = match comp.as_object() {
                        Some(o) => o,
                        None => {
                            return err(&format!(
                                "\"surfaceUpdate.components[{ci}]\" must be a JSON object"
                            ));
                        }
                    };
                    if comp_obj.get("id").and_then(Value::as_str).is_none() {
                        return err(&format!(
                            "\"surfaceUpdate.components[{ci}].id\" is required"
                        ));
                    }
                    if !matches!(comp_obj.get("component"), Some(Value::Object(_))) {
                        return err(&format!(
                            "\"surfaceUpdate.components[{ci}].component\" must be a JSON object"
                        ));
                    }
                    if let Some(error) = validate_component_payload(index, ci, comp_obj) {
                        return Some(error);
                    }
                }
            }
            None => {
                return err("\"surfaceUpdate.components\" is required and must be an array");
            }
        },
        "dataModelUpdate" => match body.get("contents").and_then(Value::as_array) {
            Some(arr) if arr.is_empty() => {
                return err("\"dataModelUpdate.contents\" must not be empty");
            }
            Some(arr) => {
                for (ci, entry) in arr.iter().enumerate() {
                    let entry_obj = match entry.as_object() {
                        Some(o) => o,
                        None => {
                            return err(&format!(
                                "\"dataModelUpdate.contents[{ci}]\" must be a JSON object"
                            ));
                        }
                    };
                    match entry_obj.get("key").and_then(Value::as_str) {
                        Some("") => {
                            return err(&format!(
                                "\"dataModelUpdate.contents[{ci}].key\" must not be empty"
                            ));
                        }
                        Some(_) => {}
                        None => {
                            return err(&format!(
                                "\"dataModelUpdate.contents[{ci}].key\" is required"
                            ));
                        }
                    }
                }
            }
            None => {
                return err("\"dataModelUpdate.contents\" is required and must be an array");
            }
        },
        "deleteSurface" => {
            // surfaceId already checked above
        }
        _ => unreachable!(),
    }

    None
}
