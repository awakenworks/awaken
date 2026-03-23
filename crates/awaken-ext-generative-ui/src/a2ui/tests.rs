use awaken_contract::contract::tool::{Tool, ToolCallContext};
use serde_json::json;

use awaken_runtime::plugins::Plugin;

use super::*;

const TEST_CATALOG: &str = "https://a2ui.org/specification/v0_9/basic_catalog.json";

#[test]
fn valid_create_surface() {
    let msgs = vec![json!({
        "version": "v0.9",
        "createSurface": {
            "surfaceId": "s1",
            "catalogId": TEST_CATALOG,
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

#[test]
fn tool_descriptor_has_correct_id() {
    let tool = A2uiRenderTool::new();
    let desc = tool.descriptor();
    assert_eq!(desc.id, A2UI_TOOL_ID);
    assert_eq!(desc.name, A2UI_TOOL_NAME);
    assert!(desc.description.contains("A2UI"));
}

#[test]
fn tool_validates_missing_messages() {
    let tool = A2uiRenderTool::new();
    assert!(tool.validate_args(&json!({})).is_err());
}

#[test]
fn tool_validates_empty_messages() {
    let tool = A2uiRenderTool::new();
    let err = tool.validate_args(&json!({"messages": []})).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn tool_validates_valid_messages() {
    let tool = A2uiRenderTool::new();
    let args = json!({"messages": [
        {
            "version": "v0.9",
            "createSurface": {
                "surfaceId": "s1",
                "catalogId": TEST_CATALOG,
            }
        }
    ]});
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_validates_invalid_a2ui() {
    let tool = A2uiRenderTool::new();
    let args = json!({"messages": [{"version": "v0.9"}]});
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("missing message type"));
}

#[tokio::test]
async fn tool_execute_returns_validated_payload() {
    let tool = A2uiRenderTool::new();
    let ctx = ToolCallContext::test_default();
    let args = json!({"messages": [
        {
            "version": "v0.9",
            "createSurface": {"surfaceId": "s1", "catalogId": TEST_CATALOG}
        },
        {
            "version": "v0.9",
            "updateComponents": {
                "surfaceId": "s1",
                "components": [{"id": "root", "component": "Card"}]
            }
        }
    ]});

    let result = tool.execute(args, &ctx).await.unwrap();
    assert_eq!(result.tool_name, A2UI_TOOL_NAME);
    assert_eq!(result.data["rendered"], true);
    assert!(result.data["a2ui"].is_array());
    assert_eq!(result.data["a2ui"].as_array().unwrap().len(), 2);
}

#[test]
fn plugin_id() {
    let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
    assert_eq!(plugin.descriptor().name, A2UI_PLUGIN_ID);
}

#[test]
fn plugin_instructions_contain_catalog() {
    let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
    assert!(plugin.instructions().contains(TEST_CATALOG));
    assert!(!plugin.instructions().contains("{{CATALOG_ID}}"));
}

#[test]
fn plugin_with_examples() {
    let plugin = A2uiPlugin::with_catalog_and_examples(TEST_CATALOG, "example content");
    assert!(plugin.instructions().contains("---BEGIN A2UI EXAMPLES---"));
    assert!(plugin.instructions().contains("example content"));
    assert!(plugin.instructions().contains("---END A2UI EXAMPLES---"));
}

#[test]
fn plugin_custom_instructions() {
    let plugin = A2uiPlugin::with_custom_instructions("Use A2UI.".into());
    assert_eq!(plugin.instructions(), "Use A2UI.");
}

#[test]
fn plugin_instructions_mention_all_message_types() {
    let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
    let text = plugin.instructions();
    assert!(text.contains("render_a2ui"));
    assert!(text.contains("createSurface"));
    assert!(text.contains("updateComponents"));
    assert!(text.contains("updateDataModel"));
    assert!(text.contains("deleteSurface"));
}
