//! JSON Render format preset.

use serde::Deserialize;
use serde_json::{Map, Value};

/// Activity type for JSON Render streaming events.
pub const ACTIVITY_TYPE: &str = "generative-ui.json-render";

#[derive(Debug, Deserialize)]
struct SpecStreamPatch {
    op: String,
    path: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    value: Option<Value>,
}

/// Returns a system prompt instructing the sub-agent to output JSON Render SpecStream JSONL.
pub fn system_prompt(catalog_json: &str) -> String {
    format!(
        "You are a UI generation agent. Output ONLY JSON Render SpecStream JSONL.\n\n\
         ## Output Format\n\
         - Emit one JSON object per line.\n\
         - Each line must be a JSON Patch-style operation for the flat JSON Render spec.\n\
         - Start streaming immediately. Do not wait to finish the whole interface before writing the first line.\n\
         - Do not wrap the lines in an array or markdown fences.\n\
         - Do not add commentary or prose.\n\n\
         ## Flat Spec Shape\n\
         {{\n  \"root\": \"elementId\",\n  \"elements\": {{\n    \
         \"elementId\": {{\n      \"type\": \"ComponentName\",\n      \
         \"props\": {{}},\n      \"children\": []\n    }}\n  }}\n}}\n\n\
         ## SpecStream Example\n\
         {{\"op\":\"add\",\"path\":\"/root\",\"value\":\"workspace\"}}\n\
         {{\"op\":\"add\",\"path\":\"/elements/workspace\",\"value\":{{\"type\":\"Card\",\"props\":{{\"title\":\"Procurement request\"}},\"children\":[\"header\",\"actions\"]}}}}\n\
         {{\"op\":\"add\",\"path\":\"/elements/header\",\"value\":{{\"type\":\"Text\",\"props\":{{\"text\":\"Request overview\"}},\"children\":[]}}}}\n\
         {{\"op\":\"add\",\"path\":\"/elements/actions\",\"value\":{{\"type\":\"Stack\",\"props\":{{\"direction\":\"horizontal\"}},\"children\":[\"approve\",\"hold\"]}}}}\n\
         {{\"op\":\"add\",\"path\":\"/elements/approve\",\"value\":{{\"type\":\"Button\",\"props\":{{\"label\":\"Approve\"}},\"children\":[]}}}}\n\
         {{\"op\":\"add\",\"path\":\"/elements/hold\",\"value\":{{\"type\":\"Button\",\"props\":{{\"label\":\"Request changes\"}},\"children\":[]}}}}\n\n\
         ## Available Components\n\
         {catalog_json}\n\n\
         ## Rules\n\
         - Preserve the user's business domain, requested fields, and realistic product copy.\n\
         - Keep the layout practical for SaaS or internal business tools.\n\
         - Every referenced child id must be defined in `elements`.\n\
         - Keep `visible`, `repeat`, `watch`, and `on` as top-level element fields, never inside `props`.\n\
         - Use `add` to create structure, `replace` for late corrections, and `remove` only when necessary.\n\
         - Output ONLY SpecStream JSONL."
    )
}

/// Compile a JSON Render SpecStream JSONL string or a complete spec object string.
pub fn compile_output(content: &str) -> Result<Value, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("json-render output was empty".into());
    }

    let mut spec = Value::Object(Map::new());
    let mut patch_count = 0usize;

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(patch) = serde_json::from_str::<SpecStreamPatch>(line) else {
            continue;
        };

        if patch.path.is_empty() {
            continue;
        }

        apply_spec_stream_patch(&mut spec, patch)?;
        patch_count += 1;
    }

    if patch_count == 0 {
        let parsed: Value = serde_json::from_str(trimmed)
            .map_err(|error| format!("invalid JSON output: {error}"))?;
        validate_spec(&parsed)?;
        return Ok(parsed);
    }

    validate_spec(&spec)?;
    Ok(spec)
}

fn validate_spec(spec: &Value) -> Result<(), String> {
    let obj = spec
        .as_object()
        .ok_or_else(|| "json-render output must be a JSON object".to_string())?;
    let root = obj
        .get("root")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "json-render spec must contain a non-empty string root".to_string())?;
    let elements = obj
        .get("elements")
        .and_then(Value::as_object)
        .ok_or_else(|| "json-render spec must contain an elements object".to_string())?;
    if !elements.contains_key(root) {
        return Err(format!(
            "json-render spec root \"{root}\" does not exist in elements"
        ));
    }
    Ok(())
}

fn apply_spec_stream_patch(target: &mut Value, patch: SpecStreamPatch) -> Result<(), String> {
    match patch.op.as_str() {
        "add" => add_by_path(target, &patch.path, patch.value.unwrap_or(Value::Null)),
        "replace" => set_by_path(target, &patch.path, patch.value.unwrap_or(Value::Null)),
        "remove" => remove_by_path(target, &patch.path),
        "move" => {
            let Some(from) = patch.from.as_deref() else {
                return Err("move patch is missing `from`".into());
            };
            let Some(value) = get_by_path(target, from).cloned() else {
                return Ok(());
            };
            remove_by_path(target, from);
            add_by_path(target, &patch.path, value);
        }
        "copy" => {
            let Some(from) = patch.from.as_deref() else {
                return Err("copy patch is missing `from`".into());
            };
            let Some(value) = get_by_path(target, from).cloned() else {
                return Ok(());
            };
            add_by_path(target, &patch.path, value);
        }
        "test" => {
            let Some(expected) = patch.value.as_ref() else {
                return Err("test patch is missing `value`".into());
            };
            let Some(actual) = get_by_path(target, &patch.path) else {
                return Err(format!("test patch failed at path {}", patch.path));
            };
            if actual != expected {
                return Err(format!("test patch failed at path {}", patch.path));
            }
        }
        other => {
            return Err(format!("unsupported spec stream operation: {other}"));
        }
    }

    Ok(())
}

fn get_by_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() || path == "/" {
        return Some(value);
    }

    let mut current = value;
    for segment in parse_json_pointer(path) {
        match current {
            Value::Array(items) => {
                let index = segment.parse::<usize>().ok()?;
                current = items.get(index)?;
            }
            Value::Object(map) => {
                current = map.get(&segment)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn set_by_path(target: &mut Value, path: &str, value: Value) {
    if path.is_empty() || path == "/" {
        *target = value;
        return;
    }

    let segments = parse_json_pointer(path);
    if segments.is_empty() {
        *target = value;
        return;
    }

    let current = ensure_parent_container(target, &segments[..segments.len() - 1]);
    let last = &segments[segments.len() - 1];
    set_child_value(current, last, value);
}

fn add_by_path(target: &mut Value, path: &str, value: Value) {
    if path.is_empty() || path == "/" {
        *target = value;
        return;
    }

    let segments = parse_json_pointer(path);
    if segments.is_empty() {
        *target = value;
        return;
    }

    let current = ensure_parent_container(target, &segments[..segments.len() - 1]);
    let last = &segments[segments.len() - 1];

    match current {
        Value::Array(items) => {
            if last == "-" {
                items.push(value);
            } else {
                let index = last
                    .parse::<usize>()
                    .unwrap_or(items.len())
                    .min(items.len());
                items.insert(index, value);
            }
        }
        Value::Object(map) => {
            map.insert(last.clone(), value);
        }
        _ => {}
    }
}

fn remove_by_path(target: &mut Value, path: &str) {
    if path.is_empty() || path == "/" {
        return;
    }

    let segments = parse_json_pointer(path);
    if segments.is_empty() {
        return;
    }

    let Some(current) = get_parent_container_mut(target, &segments[..segments.len() - 1]) else {
        return;
    };
    let last = &segments[segments.len() - 1];

    match current {
        Value::Array(items) => {
            if let Ok(index) = last.parse::<usize>()
                && index < items.len()
            {
                items.remove(index);
            }
        }
        Value::Object(map) => {
            map.remove(last);
        }
        _ => {}
    }
}

fn set_child_value(current: &mut Value, last: &str, value: Value) {
    match current {
        Value::Array(items) => {
            if last == "-" {
                items.push(value);
                return;
            }

            let index = last.parse::<usize>().unwrap_or(items.len());
            if index >= items.len() {
                items.resize(index + 1, Value::Null);
            }
            items[index] = value;
        }
        Value::Object(map) => {
            map.insert(last.to_string(), value);
        }
        _ => {}
    }
}

fn ensure_parent_container<'a>(target: &'a mut Value, segments: &[String]) -> &'a mut Value {
    let mut current = target;
    for (index, segment) in segments.iter().enumerate() {
        let next_is_numeric = segments
            .get(index + 1)
            .map(|next| is_numeric_index(next) || next == "-")
            .unwrap_or(false);

        match current {
            Value::Array(items) => {
                let array_index = segment.parse::<usize>().unwrap_or(items.len());
                if array_index >= items.len() {
                    items.resize(array_index + 1, Value::Null);
                }
                if !items[array_index].is_array() && !items[array_index].is_object() {
                    items[array_index] = if next_is_numeric {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    };
                }
                current = &mut items[array_index];
            }
            Value::Object(map) => {
                let entry = map.entry(segment.clone()).or_insert_with(|| {
                    if next_is_numeric {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    }
                });
                if !entry.is_array() && !entry.is_object() {
                    *entry = if next_is_numeric {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    };
                }
                current = entry;
            }
            _ => {
                *current = Value::Object(Map::new());
                if let Value::Object(map) = current {
                    let entry = map.entry(segment.clone()).or_insert_with(|| {
                        if next_is_numeric {
                            Value::Array(Vec::new())
                        } else {
                            Value::Object(Map::new())
                        }
                    });
                    current = entry;
                }
            }
        }
    }
    current
}

fn get_parent_container_mut<'a>(
    target: &'a mut Value,
    segments: &[String],
) -> Option<&'a mut Value> {
    let mut current = target;

    for segment in segments {
        match current {
            Value::Array(items) => {
                let index = segment.parse::<usize>().ok()?;
                current = items.get_mut(index)?;
            }
            Value::Object(map) => {
                current = map.get_mut(segment)?;
            }
            _ => return None,
        }
    }

    Some(current)
}

fn parse_json_pointer(path: &str) -> Vec<String> {
    let raw = if let Some(stripped) = path.strip_prefix('/') {
        stripped.split('/')
    } else {
        path.split('/')
    };

    raw.map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn is_numeric_index(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{compile_output, system_prompt};

    #[test]
    fn prompt_mentions_spec_stream_jsonl() {
        let prompt = system_prompt("{\"Card\":\"Container\"}");
        assert!(prompt.contains("SpecStream JSONL"));
        assert!(prompt.contains("\"op\":\"add\""));
    }

    #[test]
    fn compiles_spec_stream_output() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"workspace"}
{"op":"add","path":"/elements/workspace","value":{"type":"Card","props":{"title":"Procurement request"},"children":["summary"]}}
{"op":"add","path":"/elements/summary","value":{"type":"Text","props":{"text":"Awaiting approval"},"children":[]}}"#,
        )
        .expect("spec stream should compile");

        assert_eq!(compiled["root"], "workspace");
        assert_eq!(compiled["elements"]["workspace"]["type"], "Card");
        assert_eq!(
            compiled["elements"]["summary"]["props"]["text"],
            "Awaiting approval"
        );
    }

    #[test]
    fn compiles_spec_stream_with_nested_paths() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"workspace"}
{"op":"add","path":"/elements/workspace","value":{"type":"Card","props":{},"children":[]}}
{"op":"add","path":"/state/filters/status","value":"pending"}"#,
        )
        .expect("nested add should create parents");

        assert_eq!(compiled["state"]["filters"]["status"], "pending");
    }

    #[test]
    fn accepts_complete_json_spec() {
        let compiled = compile_output(
            r#"{"root":"workspace","elements":{"workspace":{"type":"Card","props":{"title":"Quarterly review"},"children":[]}}}"#,
        )
        .expect("complete JSON spec should compile");

        assert_eq!(
            compiled["elements"]["workspace"]["props"]["title"],
            "Quarterly review"
        );
    }

    #[test]
    fn rejects_output_without_elements() {
        let error = compile_output(r#"{"op":"add","path":"/root","value":"workspace"}"#)
            .expect_err("missing root element should fail");
        assert!(error.contains("elements object"));
    }

    #[test]
    fn supports_replace_and_remove() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"workspace"}
{"op":"add","path":"/elements/workspace","value":{"type":"Card","props":{"title":"Draft"},"children":[]}}
{"op":"replace","path":"/elements/workspace/props/title","value":"Approved"}
{"op":"add","path":"/elements/workspace/children/-","value":"footer"}
{"op":"add","path":"/elements/footer","value":{"type":"Text","props":{"text":"Next review in 30 days"},"children":[]}}
{"op":"remove","path":"/elements/workspace/children/0"}"#,
        )
        .expect("patch operations should compile");

        assert_eq!(
            compiled["elements"]["workspace"]["props"]["title"],
            "Approved"
        );
        assert_eq!(compiled["elements"]["workspace"]["children"], json!([]));
    }

    // -- compile_output error paths --

    #[test]
    fn rejects_empty_input() {
        let err = compile_output("").expect_err("empty input should fail");
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_whitespace_only_input() {
        let err = compile_output("   \n\n  ").expect_err("whitespace-only should fail");
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_invalid_json_fallback() {
        // No valid patches, so it tries to parse the whole thing as JSON
        let err = compile_output("not valid json at all").expect_err("invalid JSON should fail");
        assert!(err.contains("invalid JSON output"));
    }

    #[test]
    fn rejects_complete_json_missing_root() {
        let err = compile_output(r#"{"elements":{"w":{"type":"Card"}}}"#)
            .expect_err("missing root should fail");
        assert!(err.contains("non-empty string root"));
    }

    #[test]
    fn rejects_complete_json_with_empty_root() {
        let err = compile_output(r#"{"root":"","elements":{"w":{"type":"Card"}}}"#)
            .expect_err("empty root should fail");
        assert!(err.contains("non-empty string root"));
    }

    #[test]
    fn rejects_complete_json_root_not_in_elements() {
        let err = compile_output(r#"{"root":"missing","elements":{"w":{"type":"Card"}}}"#)
            .expect_err("root not in elements should fail");
        assert!(err.contains("does not exist in elements"));
    }

    #[test]
    fn rejects_non_object_spec() {
        let err = compile_output(r#"[1,2,3]"#).expect_err("array should fail");
        assert!(err.contains("JSON object"));
    }

    #[test]
    fn rejects_complete_json_missing_elements() {
        let err = compile_output(r#"{"root":"w"}"#).expect_err("missing elements should fail");
        assert!(err.contains("elements object"));
    }

    // -- move operation --

    #[test]
    fn supports_move_operation() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"add","path":"/temp","value":"hello"}
{"op":"move","path":"/state","from":"/temp"}"#,
        )
        .expect("move should work");
        assert_eq!(compiled["state"], "hello");
        assert!(compiled.get("temp").is_none());
    }

    #[test]
    fn move_without_from_fails() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"move","path":"/state"}"#,
        )
        .expect_err("move without from should fail");
        assert!(err.contains("missing `from`"));
    }

    #[test]
    fn move_from_nonexistent_path_is_noop() {
        // move from a path that doesn't exist should silently succeed
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"move","path":"/dest","from":"/nonexistent"}"#,
        )
        .expect("move from nonexistent should succeed");
        assert!(compiled.get("dest").is_none());
    }

    // -- copy operation --

    #[test]
    fn supports_copy_operation() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"add","path":"/source","value":"data"}
{"op":"copy","path":"/dest","from":"/source"}"#,
        )
        .expect("copy should work");
        assert_eq!(compiled["source"], "data");
        assert_eq!(compiled["dest"], "data");
    }

    #[test]
    fn copy_without_from_fails() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"copy","path":"/dest"}"#,
        )
        .expect_err("copy without from should fail");
        assert!(err.contains("missing `from`"));
    }

    #[test]
    fn copy_from_nonexistent_path_is_noop() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"copy","path":"/dest","from":"/nonexistent"}"#,
        )
        .expect("copy from nonexistent should succeed");
        assert!(compiled.get("dest").is_none());
    }

    // -- test operation --

    #[test]
    fn test_op_passes_when_values_match() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"test","path":"/root","value":"w"}"#,
        )
        .expect("test op should pass when values match");
        assert_eq!(compiled["root"], "w");
    }

    #[test]
    fn test_op_fails_when_values_differ() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"test","path":"/root","value":"wrong"}"#,
        )
        .expect_err("test op should fail when values differ");
        assert!(err.contains("test patch failed"));
    }

    #[test]
    fn test_op_fails_when_path_missing() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"test","path":"/nonexistent","value":"x"}"#,
        )
        .expect_err("test op should fail for missing path");
        assert!(err.contains("test patch failed"));
    }

    #[test]
    fn test_op_without_value_fails() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"test","path":"/root"}"#,
        )
        .expect_err("test op without value should fail");
        assert!(err.contains("missing `value`"));
    }

    // -- unsupported operation --

    #[test]
    fn rejects_unsupported_operation() {
        let err = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"bogus","path":"/foo","value":1}"#,
        )
        .expect_err("unsupported op should fail");
        assert!(err.contains("unsupported spec stream operation"));
    }

    // -- skips blank/invalid lines --

    #[test]
    fn skips_blank_lines_in_stream() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}

{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}

"#,
        )
        .expect("blank lines should be skipped");
        assert_eq!(compiled["root"], "w");
    }

    #[test]
    fn skips_unparseable_lines() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
this is not json
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}"#,
        )
        .expect("unparseable lines should be skipped");
        assert_eq!(compiled["root"], "w");
    }

    #[test]
    fn skips_patches_with_empty_path() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"","value":"ignored"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}"#,
        )
        .expect("empty-path patches should be skipped");
        assert_eq!(compiled["root"], "w");
    }

    // -- JSON Pointer tilde escaping --

    #[test]
    fn handles_tilde_escape_in_paths() {
        use super::parse_json_pointer;
        let segments = parse_json_pointer("/a~1b/c~0d");
        assert_eq!(segments, vec!["a/b".to_string(), "c~d".to_string()]);
    }

    // -- get_by_path --

    #[test]
    fn get_by_path_root() {
        use super::get_by_path;
        let val = json!({"a": 1});
        assert_eq!(get_by_path(&val, ""), Some(&json!({"a": 1})));
        assert_eq!(get_by_path(&val, "/"), Some(&json!({"a": 1})));
    }

    #[test]
    fn get_by_path_array_index() {
        use super::get_by_path;
        let val = json!({"items": [10, 20, 30]});
        assert_eq!(get_by_path(&val, "/items/1"), Some(&json!(20)));
    }

    #[test]
    fn get_by_path_invalid_array_index() {
        use super::get_by_path;
        let val = json!({"items": [10, 20]});
        assert_eq!(get_by_path(&val, "/items/abc"), None);
        assert_eq!(get_by_path(&val, "/items/99"), None);
    }

    #[test]
    fn get_by_path_through_non_container() {
        use super::get_by_path;
        let val = json!({"x": 42});
        assert_eq!(get_by_path(&val, "/x/nested"), None);
    }

    // -- set_by_path --

    #[test]
    fn set_by_path_replaces_root() {
        use super::set_by_path;
        let mut val = json!({"old": true});
        set_by_path(&mut val, "", json!("replaced"));
        assert_eq!(val, json!("replaced"));
    }

    #[test]
    fn set_by_path_slash_replaces_root() {
        use super::set_by_path;
        let mut val = json!({"old": true});
        set_by_path(&mut val, "/", json!("replaced"));
        assert_eq!(val, json!("replaced"));
    }

    #[test]
    fn set_by_path_into_array() {
        use super::set_by_path;
        let mut val = json!({"arr": [1, 2, 3]});
        set_by_path(&mut val, "/arr/1", json!(99));
        assert_eq!(val["arr"], json!([1, 99, 3]));
    }

    #[test]
    fn set_by_path_extends_array() {
        use super::set_by_path;
        let mut val = json!({"arr": [1]});
        set_by_path(&mut val, "/arr/3", json!(99));
        assert_eq!(val["arr"], json!([1, Value::Null, Value::Null, 99]));
    }

    #[test]
    fn set_by_path_array_dash() {
        use super::set_by_path;
        let mut val = json!({"arr": [1, 2]});
        set_by_path(&mut val, "/arr/-", json!(3));
        assert_eq!(val["arr"], json!([1, 2, 3]));
    }

    // -- add_by_path --

    #[test]
    fn add_by_path_root_replacement() {
        use super::add_by_path;
        let mut val = json!({"old": true});
        add_by_path(&mut val, "", json!("new"));
        assert_eq!(val, json!("new"));
    }

    #[test]
    fn add_by_path_array_insert_at_index() {
        use super::add_by_path;
        let mut val = json!({"arr": [1, 2, 3]});
        add_by_path(&mut val, "/arr/1", json!(99));
        assert_eq!(val["arr"], json!([1, 99, 2, 3]));
    }

    #[test]
    fn add_by_path_array_dash_appends() {
        use super::add_by_path;
        let mut val = json!({"arr": [1, 2]});
        add_by_path(&mut val, "/arr/-", json!(3));
        assert_eq!(val["arr"], json!([1, 2, 3]));
    }

    #[test]
    fn add_by_path_on_non_container_last_segment() {
        use super::add_by_path;
        let mut val = json!({"x": 42});
        // x is a scalar; adding /x/- triggers ensure_parent_container to overwrite
        // the scalar with an object, then add_by_path inserts into it
        add_by_path(&mut val, "/x/child", json!("value"));
        assert_eq!(val["x"]["child"], json!("value"));
    }

    // -- remove_by_path --

    #[test]
    fn remove_by_path_root_is_noop() {
        use super::remove_by_path;
        let mut val = json!({"a": 1});
        remove_by_path(&mut val, "");
        assert_eq!(val, json!({"a": 1}));
    }

    #[test]
    fn remove_by_path_from_array() {
        use super::remove_by_path;
        let mut val = json!({"arr": [10, 20, 30]});
        remove_by_path(&mut val, "/arr/1");
        assert_eq!(val["arr"], json!([10, 30]));
    }

    #[test]
    fn remove_by_path_out_of_bounds_is_noop() {
        use super::remove_by_path;
        let mut val = json!({"arr": [10]});
        remove_by_path(&mut val, "/arr/5");
        assert_eq!(val["arr"], json!([10]));
    }

    #[test]
    fn remove_by_path_from_object() {
        use super::remove_by_path;
        let mut val = json!({"a": 1, "b": 2});
        remove_by_path(&mut val, "/a");
        assert_eq!(val, json!({"b": 2}));
    }

    #[test]
    fn remove_by_path_nonexistent_parent_is_noop() {
        use super::remove_by_path;
        let mut val = json!({"a": 1});
        remove_by_path(&mut val, "/missing/child");
        assert_eq!(val, json!({"a": 1}));
    }

    #[test]
    fn remove_by_path_non_numeric_array_index_is_noop() {
        use super::remove_by_path;
        let mut val = json!({"arr": [10, 20]});
        remove_by_path(&mut val, "/arr/abc");
        assert_eq!(val["arr"], json!([10, 20]));
    }

    // -- ensure_parent_container edge cases --

    #[test]
    fn ensure_parent_container_creates_nested_containers() {
        use super::add_by_path;
        // "-" is treated as a string key in objects (not an array append)
        // when the parent container is an object
        let mut val = json!({});
        add_by_path(&mut val, "/data/items/-", json!("first"));
        // "items" is an object because "-" is not purely numeric
        // "-" becomes a key in the "items" object
        assert_eq!(val["data"]["items"]["-"], json!("first"));
    }

    #[test]
    fn ensure_parent_container_overwrites_scalar() {
        use super::add_by_path;
        let mut val = json!({"x": "scalar"});
        // x is a string but we're trying to navigate into it
        add_by_path(&mut val, "/x/nested", json!("deep"));
        assert_eq!(val["x"]["nested"], json!("deep"));
    }

    // -- is_numeric_index --

    #[test]
    fn is_numeric_index_checks() {
        use super::is_numeric_index;
        assert!(is_numeric_index("0"));
        assert!(is_numeric_index("123"));
        assert!(!is_numeric_index(""));
        assert!(!is_numeric_index("abc"));
        assert!(!is_numeric_index("-"));
        assert!(!is_numeric_index("12a"));
    }

    // -- ACTIVITY_TYPE constant --

    #[test]
    fn activity_type_constant() {
        assert_eq!(super::ACTIVITY_TYPE, "generative-ui.json-render");
    }

    // -- system_prompt includes catalog --

    #[test]
    fn system_prompt_includes_catalog() {
        let prompt = system_prompt(r#"[{"Card":{}},{"Text":{}}]"#);
        assert!(prompt.contains(r#"[{"Card":{}},{"Text":{}}]"#));
        assert!(prompt.contains("Available Components"));
    }

    // -- full round-trip with move and copy --

    #[test]
    fn full_round_trip_with_copy_and_move() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"main"}
{"op":"add","path":"/elements/main","value":{"type":"Card","props":{"title":"Original"},"children":[]}}
{"op":"copy","path":"/elements/backup","from":"/elements/main"}
{"op":"move","path":"/elements/primary","from":"/elements/main"}
{"op":"replace","path":"/root","value":"primary"}"#,
        )
        .expect("round trip should work");

        // main was moved to primary
        assert!(compiled["elements"].get("main").is_none());
        assert_eq!(compiled["elements"]["primary"]["type"], "Card");
        // backup was copied before the move
        assert_eq!(compiled["elements"]["backup"]["type"], "Card");
        assert_eq!(compiled["root"], "primary");
    }

    // -- add with no value defaults to null --

    #[test]
    fn add_without_value_uses_null() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"add","path":"/nullfield"}"#,
        )
        .expect("add without value should use null");
        assert_eq!(compiled["nullfield"], json!(null));
    }

    // -- replace without value uses null --

    #[test]
    fn replace_without_value_uses_null() {
        let compiled = compile_output(
            r#"{"op":"add","path":"/root","value":"w"}
{"op":"add","path":"/elements/w","value":{"type":"Card","props":{},"children":[]}}
{"op":"add","path":"/field","value":"something"}
{"op":"replace","path":"/field"}"#,
        )
        .expect("replace without value should use null");
        assert_eq!(compiled["field"], json!(null));
    }
}
