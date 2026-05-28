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
/// - Rewrites `const` to an equivalent single-value `enum` because Gemini's
///   OpenAPI schema subset rejects `const`
/// - Simplifies `anyOf: [T, {type: null}]` patterns produced by `Option<T>`
/// - Ensures arrays have `items` and objects have `properties`
pub fn sanitize_for_llm(schema: &mut Value) {
    rewrite_const_as_enum(schema);
    simplify_any_of(schema);
    fix_missing_fields(schema);
}

/// Recursively rewrite JSON Schema `const` keywords to `enum: [value]`.
///
/// `const` and a single-value `enum` are equivalent for JSON Schema
/// validation, while the latter is accepted by Gemini's function declaration
/// schema. If a schema contains both keywords and they intersect (the const
/// value is one of the enum values) we narrow to `enum: [const_value]`. If
/// they are incompatible the original schema is unsatisfiable; replace the
/// subschema with the JSON Schema `false` boolean so the rewrite does not
/// silently widen the constraint.
fn rewrite_const_as_enum(value: &mut Value) {
    if let Some(arr) = value.as_array_mut() {
        for item in arr.iter_mut() {
            rewrite_const_as_enum(item);
        }
        return;
    }

    let conflict = if let Some(obj) = value.as_object_mut() {
        for v in obj.values_mut() {
            rewrite_const_as_enum(v);
        }

        let Some(const_value) = obj.remove("const") else {
            return;
        };
        let conflict = match obj.get("enum").and_then(|v| v.as_array()) {
            Some(arr) => !arr.iter().any(|v| v == &const_value),
            None => false,
        };
        if !conflict {
            obj.insert("enum".to_string(), Value::Array(vec![const_value]));
            return;
        }
        conflict
    } else {
        return;
    };

    debug_assert!(conflict);
    *value = Value::Bool(false);
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
    fn const_keyword_rewritten_to_single_enum() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "target": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "self" }
                            },
                            "required": ["relation"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "relation": { "const": "child" },
                                "name": { "type": "string" }
                            },
                            "required": ["relation", "name"]
                        }
                    ]
                }
            },
            "required": ["target"]
        });

        sanitize_for_llm(&mut schema);
        let output = serde_json::to_string(&schema).unwrap();
        assert!(
            !output.contains("\"const\""),
            "Gemini rejects JSON Schema `const` in function declarations"
        );
        assert_eq!(
            schema["properties"]["target"]["oneOf"][0]["properties"]["relation"]["enum"],
            json!(["self"])
        );
        assert_eq!(
            schema["properties"]["target"]["oneOf"][1]["properties"]["relation"]["enum"],
            json!(["child"])
        );
        assert!(jsonschema::validator_for(&schema).is_ok());
        assert!(validate_against_schema(&schema, &json!({"target": {"relation": "self"}})).is_ok());
        assert!(
            validate_against_schema(&schema, &json!({"target": {"relation": "other"}})).is_err()
        );
    }

    #[test]
    fn const_with_matching_enum_narrows_to_const() {
        let mut schema = json!({
            "type": "string",
            "const": "self",
            "enum": ["self", "child"]
        });
        sanitize_for_llm(&mut schema);
        assert_eq!(schema["enum"], json!(["self"]));
        assert!(schema.get("const").is_none());
        assert!(jsonschema::validator_for(&schema).is_ok());
    }

    #[test]
    fn const_with_contradictory_enum_becomes_unsatisfiable() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "relation": {
                    "const": "self",
                    "enum": ["child"]
                }
            },
            "required": ["relation"]
        });
        sanitize_for_llm(&mut schema);
        // The contradictory subschema collapses to the JSON Schema `false`
        // boolean so any value at this position is rejected.
        assert_eq!(schema["properties"]["relation"], Value::Bool(false));
        let validator = jsonschema::validator_for(&schema).expect("valid schema");
        assert!(!validator.is_valid(&json!({"relation": "self"})));
        assert!(!validator.is_valid(&json!({"relation": "child"})));
    }

    #[test]
    fn const_inside_one_of_handles_each_branch_independently() {
        let mut schema = json!({
            "oneOf": [
                { "const": "a", "enum": ["a", "b"] },
                { "const": "c", "enum": ["d"] }
            ]
        });
        sanitize_for_llm(&mut schema);
        assert_eq!(schema["oneOf"][0]["enum"], json!(["a"]));
        assert_eq!(schema["oneOf"][1], Value::Bool(false));
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
