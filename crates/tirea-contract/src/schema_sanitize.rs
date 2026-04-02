//! Shared JSON Schema sanitization for LLM function calling compatibility.
//!
//! LLM providers (OpenAI, Gemini, etc.) reject standard JSON Schema features
//! like `$ref`, `$defs`, `anyOf`, and arrays without `items`. This module
//! provides a single sanitization pipeline that all tool schema producers
//! should use instead of duplicating the logic.

use serde_json::{json, Map, Value};

/// Sanitize a JSON Schema for LLM function calling compatibility.
///
/// Performs these transformations (in order):
/// 1. Inlines `$ref` / `$defs` / `definitions` with depth-bounded recursion
/// 2. Removes `$schema`, `$defs`, `definitions` from root
/// 3. Simplifies `anyOf` (merges object variants, picks first for primitives)
/// 4. Adds `items: {}` to array types missing `items`
/// 5. Strips any remaining `$ref` nodes (safety net for recursive types)
pub fn sanitize_tool_schema(schema: &mut Value) {
    // Step 1: Inline $ref references (depth-bounded for recursive types)
    let definitions = schema
        .get("definitions")
        .or_else(|| schema.get("$defs"))
        .and_then(Value::as_object)
        .cloned();
    if let Some(definitions) = definitions.as_ref() {
        inline_refs_bounded(schema, definitions, 6);
    }

    // Step 2: Remove schema metadata
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$schema");
        obj.remove("definitions");
        obj.remove("$defs");
    }

    // Step 3: Simplify anyOf (Gemini rejects it, OpenAI handles it poorly)
    simplify_any_of(schema);

    // Step 4: Add missing array items (OpenAI requires items for array types)
    fix_missing_array_items(schema);

    // Step 5: Strip any remaining $ref (from unresolvable or recursive types)
    strip_unresolved_refs(schema);

    // Step 6: Add missing properties to object types (OpenAI requires it)
    fix_missing_object_properties(schema);
}

/// Add `properties: {}` to object types that are missing it.
/// OpenAI function calling requires `properties` for all object schemas.
fn fix_missing_object_properties(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("object")
                && !map.contains_key("properties")
            {
                map.insert("properties".into(), json!({}));
            }
            for v in map.values_mut() {
                fix_missing_object_properties(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                fix_missing_object_properties(v);
            }
        }
        _ => {}
    }
}

/// Inline `$ref` references with a depth limit to prevent infinite recursion
/// on self-referencing types.
///
/// Supports both schemars 0.8 (`#/definitions/X`) and 1.x (`#/$defs/X`) formats.
/// When the depth limit is reached, remaining `$ref` nodes are left for
/// [`strip_unresolved_refs`] to handle.
fn inline_refs_bounded(node: &mut Value, definitions: &Map<String, Value>, depth: usize) {
    if depth == 0 {
        return;
    }
    match node {
        Value::Object(map) => {
            let ref_name = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| {
                    r.strip_prefix("#/definitions/")
                        .or_else(|| r.strip_prefix("#/$defs/"))
                })
                .map(String::from);

            if let Some(ref_name) = ref_name {
                if let Some(def) = definitions.get(&ref_name) {
                    let mut resolved = def.clone();
                    inline_refs_bounded(&mut resolved, definitions, depth - 1);
                    *node = resolved;
                    return;
                }
            }

            for v in map.values_mut() {
                inline_refs_bounded(v, definitions, depth);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                inline_refs_bounded(v, definitions, depth);
            }
        }
        _ => {}
    }
}

/// Simplify `anyOf` constructs that LLM providers cannot handle.
///
/// Handles three cases:
/// 1. **Option<T>** — `anyOf: [T, {type: null}]` → T (strip null variant)
/// 2. **Untagged enum of objects** — merges all properties into one object
///    with no required fields, so the LLM can see all possible fields
/// 3. **Mixed-type union** — picks the first non-null variant (best effort)
fn simplify_any_of(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if let Some(any_of) = map.remove("anyOf") {
                if let Some(variants) = any_of.as_array() {
                    let non_null: Vec<&Value> = variants
                        .iter()
                        .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
                        .collect();

                    let replacement = if non_null.len() == 1 {
                        // Case 1: Option<T> — single non-null variant
                        Some(non_null[0].clone())
                    } else if non_null.len() > 1
                        && non_null.iter().all(|v| {
                            v.get("properties").is_some()
                                || v.get("type").and_then(Value::as_str) == Some("object")
                        })
                    {
                        // Case 2: All variants are objects — merge properties
                        Some(merge_object_variants(&non_null))
                    } else if !non_null.is_empty() {
                        // Case 3: Mixed types — pick first non-null
                        Some(non_null[0].clone())
                    } else {
                        None
                    };

                    if let Some(mut replacement) = replacement {
                        simplify_any_of(&mut replacement);
                        if let Some(obj) = replacement.as_object() {
                            for (k, v) in obj {
                                map.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
            for value in map.values_mut() {
                simplify_any_of(value);
            }
        }
        Value::Array(values) => {
            for v in values {
                simplify_any_of(v);
            }
        }
        _ => {}
    }
}

/// Merge multiple object-type `anyOf` variants into a single object schema.
///
/// All properties from all variants are included. `required` is dropped
/// since each variant may require different fields. Descriptions from
/// variants are concatenated to help the LLM understand the alternatives.
fn merge_object_variants(variants: &[&Value]) -> Value {
    let mut merged_properties = Map::new();
    let mut variant_descriptions: Vec<String> = Vec::new();

    for (i, variant) in variants.iter().enumerate() {
        if let Some(props) = variant.get("properties").and_then(Value::as_object) {
            for (k, v) in props {
                // First variant's definition wins for each property
                merged_properties
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
        }
        // Collect variant descriptions or titles for context
        if let Some(desc) = variant
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| variant.get("title").and_then(Value::as_str))
        {
            variant_descriptions.push(desc.to_string());
        } else {
            // Generate a label from required fields if no description
            if let Some(required) = variant.get("required").and_then(Value::as_array) {
                let fields: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
                if !fields.is_empty() {
                    variant_descriptions.push(format!(
                        "variant {} (requires: {})",
                        i + 1,
                        fields.join(", ")
                    ));
                }
            }
        }
    }

    let mut result = json!({
        "type": "object",
        "properties": Value::Object(merged_properties),
    });

    if !variant_descriptions.is_empty() {
        let desc = format!("One of: {}", variant_descriptions.join(" | "));
        result
            .as_object_mut()
            .unwrap()
            .insert("description".into(), Value::String(desc));
    }

    result
}

/// Add `items: {}` to array types that are missing it.
/// OpenAI function calling requires `items` for all array schemas.
fn fix_missing_array_items(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("array")
                && !map.contains_key("items")
            {
                map.insert("items".into(), json!({}));
            }
            for v in map.values_mut() {
                fix_missing_array_items(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                fix_missing_array_items(v);
            }
        }
        _ => {}
    }
}

/// Strip any remaining `$ref` nodes, replacing them with `{"type": "object"}`.
/// This ensures the schema is fully self-contained after depth-bounded inlining.
fn strip_unresolved_refs(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.contains_key("$ref") {
                map.clear();
                map.insert("type".into(), json!("object"));
                return;
            }
            for v in map.values_mut() {
                strip_unresolved_refs(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_unresolved_refs(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inlines_defs_and_removes_schema() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "Foo": { "type": "object", "properties": { "x": { "type": "string" } } }
            },
            "type": "object",
            "properties": {
                "foo": { "$ref": "#/$defs/Foo" }
            }
        });

        sanitize_tool_schema(&mut schema);

        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        assert!(!pretty.contains("$ref"), "should have inlined $ref");
        assert!(!pretty.contains("$defs"), "should have removed $defs");
        assert!(!pretty.contains("$schema"), "should have removed $schema");
        assert!(
            pretty.contains("\"x\""),
            "should have inlined Foo's properties"
        );
    }

    #[test]
    fn inlines_definitions_format() {
        let mut schema = json!({
            "definitions": {
                "Bar": { "type": "string" }
            },
            "type": "object",
            "properties": {
                "bar": { "$ref": "#/definitions/Bar" }
            }
        });

        sanitize_tool_schema(&mut schema);

        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        assert!(!pretty.contains("$ref"));
        assert!(!pretty.contains("definitions"));
    }

    #[test]
    fn simplifies_option_any_of() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "name": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                }
            }
        });

        sanitize_tool_schema(&mut schema);

        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        assert!(!pretty.contains("anyOf"), "should have removed anyOf");
        assert!(pretty.contains("\"type\": \"string\""));
    }

    #[test]
    fn merges_untagged_enum_object_variants() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "handler": {
                    "anyOf": [
                        {
                            "type": "object",
                            "required": ["url"],
                            "properties": {
                                "url": { "type": "string" },
                                "method": { "type": "string" }
                            }
                        },
                        {
                            "type": "object",
                            "required": ["to"],
                            "properties": {
                                "to": { "type": "string" },
                                "subject": { "type": "string" }
                            }
                        }
                    ]
                }
            }
        });

        sanitize_tool_schema(&mut schema);

        let handler = &schema["properties"]["handler"];
        assert!(
            !handler.to_string().contains("anyOf"),
            "anyOf should be removed"
        );
        assert_eq!(handler["type"], "object");
        // All properties from both variants should be present
        let props = handler["properties"].as_object().unwrap();
        assert!(props.contains_key("url"), "should have url from variant 1");
        assert!(
            props.contains_key("method"),
            "should have method from variant 1"
        );
        assert!(props.contains_key("to"), "should have to from variant 2");
        assert!(
            props.contains_key("subject"),
            "should have subject from variant 2"
        );
        // required should be absent (variants have different requirements)
        assert!(
            handler.get("required").is_none(),
            "required should be dropped"
        );
    }

    #[test]
    fn fixes_array_without_items() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "tags": { "type": "array" }
            }
        });

        sanitize_tool_schema(&mut schema);

        assert!(schema["properties"]["tags"].get("items").is_some());
    }

    #[test]
    fn strips_unresolved_refs() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "nested": { "$ref": "#/$defs/Unknown" }
            }
        });

        sanitize_tool_schema(&mut schema);

        let nested = &schema["properties"]["nested"];
        assert_eq!(nested["type"], "object");
        assert!(nested.get("$ref").is_none());
    }

    #[test]
    fn handles_recursive_types_with_depth_limit() {
        let mut schema = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "value": { "type": "string" },
                        "children": {
                            "type": "array",
                            "items": { "$ref": "#/$defs/Node" }
                        }
                    }
                }
            },
            "type": "object",
            "properties": {
                "root": { "$ref": "#/$defs/Node" }
            }
        });

        sanitize_tool_schema(&mut schema);

        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        // Should not have $ref or $defs
        assert!(
            !pretty.contains("$ref"),
            "all refs should be resolved or stripped"
        );
        assert!(!pretty.contains("$defs"), "$defs should be removed");
        // Should have inlined at least the first level
        assert!(pretty.contains("\"value\""));
    }

    #[test]
    fn mixed_type_anyof_picks_first() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "field_type": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                }
            }
        });

        sanitize_tool_schema(&mut schema);

        // Should pick first non-null variant (string)
        assert_eq!(schema["properties"]["field_type"]["type"], "string");
        assert!(!schema["properties"]["field_type"]
            .to_string()
            .contains("anyOf"));
    }

    #[test]
    fn sanitizes_schemars_empty_struct() {
        #[derive(schemars::JsonSchema)]
        struct Empty {}

        let schema = serde_json::to_value(schemars::schema_for!(Empty)).unwrap();
        let mut sanitized = schema.clone();
        sanitize_tool_schema(&mut sanitized);
        assert!(sanitized.get("properties").is_some());
    }

    #[test]
    fn fixes_object_without_properties() {
        let mut schema = json!({
            "type": "object"
        });

        sanitize_tool_schema(&mut schema);

        assert!(
            schema.get("properties").is_some(),
            "object should have properties"
        );
        assert_eq!(schema["properties"], json!({}));
    }

    #[test]
    fn fixes_nested_object_without_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "nested": { "type": "object" }
            }
        });

        sanitize_tool_schema(&mut schema);

        assert!(schema["properties"]["nested"].get("properties").is_some());
    }

    #[test]
    fn no_anyof_no_ref_no_defs_after_sanitize() {
        // This schema has everything: $defs, $ref, anyOf, arrays without items
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "Item": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "tags": { "type": "array" }
                    }
                }
            },
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/Item" },
                "optional": {
                    "anyOf": [
                        { "$ref": "#/$defs/Item" },
                        { "type": "null" }
                    ]
                }
            }
        });

        sanitize_tool_schema(&mut schema);

        let pretty = serde_json::to_string_pretty(&schema).unwrap();
        assert!(!pretty.contains("$ref"));
        assert!(!pretty.contains("$defs"));
        assert!(!pretty.contains("$schema"));
        assert!(!pretty.contains("anyOf"));

        // Array should have items
        fn check_arrays(v: &Value, path: &str) {
            if let Some(map) = v.as_object() {
                if map.get("type").and_then(|v| v.as_str()) == Some("array") {
                    assert!(map.contains_key("items"), "array at {path} missing items");
                }
                for (k, val) in map {
                    check_arrays(val, &format!("{path}.{k}"));
                }
            }
            if let Some(arr) = v.as_array() {
                for (i, val) in arr.iter().enumerate() {
                    check_arrays(val, &format!("{path}[{i}]"));
                }
            }
        }
        check_arrays(&schema, "root");
    }
}
