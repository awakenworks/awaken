//! Schema generation, sanitization, and validation utilities for LLM tool calling.

use serde_json::Value;

use super::tool::ToolError;

/// Generate a JSON Schema for `T` that is suitable for LLM tool calling.
///
/// Uses schemars 1.x with inlined subschemas and no meta-schema, then strips
/// meta fields and simplifies `Option`-generated `anyOf` patterns.
pub fn generate_tool_schema<T: schemars::JsonSchema>() -> Value {
    let settings = schemars::generate::SchemaSettings::default().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    // serde_json::to_value on a schemars RootSchema is infallible because
    // RootSchema implements Serialize with no fallible custom logic and all
    // field types are JSON-native. An Err here would indicate a serde bug.
    let mut value = serde_json::to_value(schema).expect("schema serialization cannot fail");

    // Remove top-level meta keys that LLMs don't need.
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
        obj.remove("definitions");
    }

    sanitize_for_llm(&mut value);
    value
}

/// Sanitize a JSON Schema value in-place to be LLM-friendly.
///
/// - Simplifies `anyOf: [T, {type: null}]` patterns produced by `Option<T>`
/// - Ensures arrays have `items` and objects have `properties`
pub fn sanitize_for_llm(schema: &mut Value) {
    simplify_any_of(schema);
    fix_missing_fields(schema);
}

/// Validate `args` against a JSON Schema, returning an error with joined messages on failure.
pub fn validate_against_schema(schema: &Value, args: &Value) -> Result<(), ToolError> {
    // Null schema means no validation needed.
    if schema.is_null() {
        return Ok(());
    }

    // Empty-ish schema with no properties and no required fields: skip validation.
    if let Some(obj) = schema.as_object() {
        let has_properties = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .is_some_and(|p| !p.is_empty());
        let has_required = obj
            .get("required")
            .and_then(|v| v.as_array())
            .is_some_and(|r| !r.is_empty());
        if !has_properties && !has_required {
            return Ok(());
        }
    }

    let validator = jsonschema::validator_for(schema)
        .map_err(|e| ToolError::Internal(format!("invalid tool schema: {e}")))?;

    let errors: Vec<String> = validator.iter_errors(args).map(|e| e.to_string()).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ToolError::InvalidArguments(errors.join("; ")))
    }
}

/// Recursively simplify Option-generated patterns to be LLM-friendly.
///
/// Handles two patterns:
/// - `anyOf: [T, {type: null}]` — merges T's fields into parent
/// - `type: ["integer", "null"]` — simplifies to `type: "integer"`
fn simplify_any_of(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        // First, recurse into all nested values.
        for v in obj.values_mut() {
            simplify_any_of(v);
        }

        // Simplify type arrays: ["integer", "null"] → "integer"
        if let Some(type_val) = obj.get_mut("type")
            && let Some(arr) = type_val.as_array().cloned()
        {
            let non_null: Vec<&Value> = arr.iter().filter(|v| v.as_str() != Some("null")).collect();
            if non_null.len() == 1 && non_null.len() < arr.len() {
                *type_val = non_null[0].clone();
            }
        }

        // Then check if this object has an anyOf that matches the Option pattern.
        if let Some(any_of) = obj.remove("anyOf") {
            if let Some(arr) = any_of.as_array()
                && arr.len() == 2
            {
                let (null_idx, non_null_idx) = if is_null_schema(&arr[0]) {
                    (0, 1)
                } else if is_null_schema(&arr[1]) {
                    (1, 0)
                } else {
                    // Not an Option pattern, put it back.
                    obj.insert("anyOf".to_string(), any_of);
                    return;
                };
                let _ = null_idx;

                // Merge the non-null variant's fields into the parent.
                if let Some(non_null_obj) = arr[non_null_idx].as_object() {
                    for (k, v) in non_null_obj {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                return;
            }
            // Not a 2-element array, put it back.
            obj.insert("anyOf".to_string(), any_of);
        }
    } else if let Some(arr) = value.as_array_mut() {
        for item in arr.iter_mut() {
            simplify_any_of(item);
        }
    }
}

/// Check if a schema value represents `{type: "null"}`.
fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|s| s == "null")
}

/// Recursively ensure arrays have `items` and objects have `properties`.
fn fix_missing_fields(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        // Recurse into nested values first.
        for v in obj.values_mut() {
            fix_missing_fields(v);
        }

        if let Some(ty) = obj.get("type").and_then(|t| t.as_str()).map(String::from) {
            if ty == "array" && !obj.contains_key("items") {
                obj.insert("items".to_string(), Value::Object(Default::default()));
            }
            if ty == "object" && !obj.contains_key("properties") {
                obj.insert("properties".to_string(), Value::Object(Default::default()));
            }
        }
    } else if let Some(arr) = value.as_array_mut() {
        for item in arr.iter_mut() {
            fix_missing_fields(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    // --- Test structs ---

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct SimpleArgs {
        query: String,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct OptionalArgs {
        query: String,
        limit: Option<u32>,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct ListArgs {
        items: Vec<String>,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct Inner {
        x: i32,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct Outer {
        inner: Inner,
    }

    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    enum Format {
        Json,
        Yaml,
        Xml,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct FormatArgs {
        format: Format,
    }

    #[allow(dead_code)]
    #[derive(Deserialize, JsonSchema)]
    struct ComplexArgs {
        name: String,
        tags: Vec<String>,
        count: Option<u32>,
        inner: Inner,
        format: Option<Format>,
    }

    // --- Schema generation tests ---

    #[test]
    fn generate_simple_struct() {
        let schema = generate_tool_schema::<SimpleArgs>();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["query"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }

    #[test]
    fn option_field_simplified() {
        let schema = generate_tool_schema::<OptionalArgs>();
        // limit should not have anyOf
        assert!(schema["properties"]["limit"].get("anyOf").is_none());
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
        assert!(!required.contains(&json!("limit")));
    }

    #[test]
    fn vec_field_has_items() {
        let schema = generate_tool_schema::<ListArgs>();
        assert_eq!(schema["properties"]["items"]["type"], "array");
        assert!(schema["properties"]["items"].get("items").is_some());
    }

    #[test]
    fn nested_struct_inlined() {
        let schema = generate_tool_schema::<Outer>();
        let output = serde_json::to_string(&schema).unwrap();
        assert!(!output.contains("$ref"));
        assert_eq!(
            schema["properties"]["inner"]["properties"]["x"]["type"],
            "integer"
        );
    }

    #[test]
    fn enum_field_generates_enum_values() {
        let schema = generate_tool_schema::<FormatArgs>();
        let output = serde_json::to_string(&schema).unwrap();
        assert!(output.contains("Json"));
        assert!(output.contains("Yaml"));
    }

    #[test]
    fn no_meta_fields_in_output() {
        let schema = generate_tool_schema::<ComplexArgs>();
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$defs").is_none());
        assert!(schema.get("definitions").is_none());
    }

    #[test]
    fn sanitize_is_idempotent() {
        let mut schema = generate_tool_schema::<ComplexArgs>();
        let before = schema.clone();
        sanitize_for_llm(&mut schema);
        assert_eq!(schema, before);
    }

    // --- Complex combined ---

    #[test]
    fn complex_struct_is_llm_friendly() {
        let schema = generate_tool_schema::<ComplexArgs>();
        let output = serde_json::to_string(&schema).unwrap();
        assert!(!output.contains("$ref"));
        assert!(!output.contains("$defs"));
        assert!(!output.contains("$schema"));
        assert!(!output.contains("anyOf"));
        // Arrays have items
        assert!(schema["properties"]["tags"].get("items").is_some());
        // Objects have properties
        assert!(schema["properties"]["inner"].get("properties").is_some());
    }

    // --- Provider compatibility ---

    #[test]
    fn gemini_compatible_no_any_of() {
        let schema = generate_tool_schema::<ComplexArgs>();
        let output = serde_json::to_string(&schema).unwrap();
        assert!(!output.contains("anyOf"));
        assert!(!output.contains("oneOf"));
        assert!(!output.contains("allOf"));
    }

    #[test]
    fn openai_anthropic_valid_json_schema() {
        let schema = generate_tool_schema::<ComplexArgs>();
        assert!(jsonschema::validator_for(&schema).is_ok());
    }

    // --- Validation tests ---

    #[test]
    fn validate_accepts_valid_input() {
        let schema = generate_tool_schema::<SimpleArgs>();
        let args = json!({"query": "hello"});
        assert!(validate_against_schema(&schema, &args).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_type() {
        let schema = generate_tool_schema::<SimpleArgs>();
        let args = json!({"query": 42});
        assert!(validate_against_schema(&schema, &args).is_err());
    }

    #[test]
    fn validate_rejects_missing_required() {
        let schema = generate_tool_schema::<SimpleArgs>();
        let args = json!({});
        assert!(validate_against_schema(&schema, &args).is_err());
    }

    #[test]
    fn validate_skips_empty_schema() {
        let schema = json!({"type": "object"});
        let args = json!({"anything": true});
        assert!(validate_against_schema(&schema, &args).is_ok());
    }

    #[test]
    fn validate_skips_null_schema() {
        let schema = Value::Null;
        let args = json!({"anything": true});
        assert!(validate_against_schema(&schema, &args).is_ok());
    }

    #[test]
    fn validate_optional_field_accepts_absence() {
        let schema = generate_tool_schema::<OptionalArgs>();
        let args = json!({"query": "hello"});
        assert!(validate_against_schema(&schema, &args).is_ok());
    }

    #[test]
    fn validate_optional_field_accepts_value() {
        let schema = generate_tool_schema::<OptionalArgs>();
        let args = json!({"query": "hello", "limit": 10});
        assert!(validate_against_schema(&schema, &args).is_ok());
    }

    #[test]
    fn validate_optional_field_rejects_wrong_type() {
        let schema = generate_tool_schema::<OptionalArgs>();
        let args = json!({"query": "hello", "limit": "not a number"});
        assert!(validate_against_schema(&schema, &args).is_err());
    }
}
