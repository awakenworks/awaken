use awaken_contract::contract::tool::{Tool, ToolCallContext};
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::state::MutationBatch;
use serde_json::json;

use awaken_runtime::plugins::Plugin;

use super::*;

const TEST_CATALOG: &str = "https://a2ui.org/specification/v0_8/standard_catalog_definition.json";

#[test]
fn valid_begin_rendering() {
    let msgs = vec![json!({
        "beginRendering": {
            "surfaceId": "s1",
            "root": "root",
        }
    })];
    assert!(validate_a2ui_messages(&msgs).is_empty());
}

#[test]
fn valid_surface_update() {
    let msgs = vec![json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "root", "component": {"Card": {"child": "content"}}},
                {"id": "content", "component": {"Text": {"text": {"literalString": "Hello"}}}}
            ]
        }
    })];
    assert!(validate_a2ui_messages(&msgs).is_empty());
}

#[test]
fn valid_data_model_update() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "path": "/request",
            "contents": [
                {"key": "requester", "valueString": "Alice"},
                {"key": "priority", "valueNumber": 2.0}
            ]
        }
    })];
    assert!(validate_a2ui_messages(&msgs).is_empty());
}

#[test]
fn valid_delete_surface() {
    let msgs = vec![json!({
        "deleteSurface": {"surfaceId": "s1"}
    })];
    assert!(validate_a2ui_messages(&msgs).is_empty());
}

#[test]
fn rejects_no_message_type() {
    let msgs = vec![json!({})];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("missing message type"));
}

#[test]
fn rejects_multiple_message_types() {
    let msgs = vec![json!({
        "beginRendering": {"surfaceId": "s1", "root": "root"},
        "deleteSurface": {"surfaceId": "s1"}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("multiple message types"));
}

#[test]
fn rejects_missing_surface_id() {
    let msgs = vec![json!({
        "deleteSurface": {}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("surfaceId"));
}

#[test]
fn rejects_empty_surface_id() {
    let msgs = vec![json!({
        "beginRendering": {"surfaceId": "", "root": "root"}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must not be empty"));
}

#[test]
fn rejects_missing_root() {
    let msgs = vec![json!({
        "beginRendering": {"surfaceId": "s1"}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("root"));
}

#[test]
fn rejects_empty_components() {
    let msgs = vec![json!({
        "surfaceUpdate": {"surfaceId": "s1", "components": []}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must not be empty"));
}

#[test]
fn rejects_component_missing_id() {
    let msgs = vec![json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [{"component": {"Text": {"text": {"literalString": "hi"}}}}]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("components[0].id"));
}

#[test]
fn rejects_component_missing_component_payload() {
    let msgs = vec![json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [{"id": "root"}]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("components[0].component"));
}

#[test]
fn rejects_empty_data_model_contents() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "contents": []
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("contents"));
}

#[test]
fn rejects_data_model_entry_missing_key() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "contents": [{"valueString": "hi"}]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("contents[0].key"));
}

#[test]
fn reports_per_message_errors() {
    let msgs = vec![
        json!({"deleteSurface": {"surfaceId": "ok"}}),
        json!({}),
        json!({"deleteSurface": {"surfaceId": "ok"}}),
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
fn tool_descriptor_exposes_strict_nested_schema() {
    let tool = A2uiRenderTool::new();
    let desc = tool.descriptor();
    let schema = &desc.parameters;

    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["surfaceUpdate"]["required"],
        json!(["surfaceId", "components"])
    );
    assert_eq!(
        schema["properties"]["surfaceUpdate"]["properties"]["components"]["items"]["required"],
        json!(["id", "component"])
    );
}

#[test]
fn tool_rejects_empty_args() {
    let tool = A2uiRenderTool::new();
    assert!(tool.validate_args(&json!({})).is_err());
}

#[test]
fn tool_accepts_flat_begin_rendering() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "beginRendering": {"surfaceId": "s1", "root": "root"}
    });
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_accepts_flat_surface_update() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [{"id": "root", "component": {"Card": {"child": "content"}}}]
        }
    });
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_accepts_flat_data_model_update() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "path": "/request",
            "contents": [{"key": "requester", "valueString": "Alice"}]
        }
    });
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_accepts_flat_delete_surface() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "deleteSurface": {"surfaceId": "s1"}
    });
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_rejects_structural_error() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "beginRendering": {"surfaceId": "s1"}
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("root"));
}

#[test]
fn tool_rejects_legacy_multiple_choice_shape() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "approval",
            "components": [
                {
                    "id": "priority",
                    "component": {
                        "MultipleChoice": {
                            "choices": [
                                {
                                    "label": { "literalString": "High" },
                                    "value": "high"
                                }
                            ],
                            "label": { "literalString": "Priority" },
                            "value": { "path": "/request/priority" }
                        }
                    }
                }
            ]
        }
    });

    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("choices") || err.to_string().contains("MultipleChoice"));
}

#[test]
fn tool_rejects_button_context_object_shape() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "approval",
            "components": [
                {
                    "id": "submit_text",
                    "component": {
                        "Text": {
                            "text": { "literalString": "Submit" }
                        }
                    }
                },
                {
                    "id": "submit_button",
                    "component": {
                        "Button": {
                            "child": "submit_text",
                            "action": {
                                "name": "approval.submit",
                                "context": {
                                    "requester": { "path": "/request/requester" }
                                }
                            }
                        }
                    }
                }
            ]
        }
    });

    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("context") || err.to_string().contains("Button"));
}

#[test]
fn tool_accepts_official_multiple_choice_and_button_shapes() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "approval",
            "components": [
                {
                    "id": "root",
                    "component": {
                        "Card": {
                            "child": "content"
                        }
                    }
                },
                {
                    "id": "content",
                    "component": {
                        "Column": {
                            "children": {
                                "explicitList": ["priority_label", "priority", "submit_text", "submit_button"]
                            },
                            "distribution": "start",
                            "alignment": "stretch"
                        }
                    }
                },
                {
                    "id": "priority_label",
                    "component": {
                        "Text": {
                            "usageHint": "caption",
                            "text": { "literalString": "Priority" }
                        }
                    }
                },
                {
                    "id": "priority",
                    "component": {
                        "MultipleChoice": {
                            "selections": { "path": "/request/priority" },
                            "options": [
                                {
                                    "label": { "literalString": "Standard" },
                                    "value": "standard"
                                }
                            ],
                            "maxAllowedSelections": 1
                        }
                    }
                },
                {
                    "id": "submit_text",
                    "component": {
                        "Text": {
                            "text": { "literalString": "Submit" }
                        }
                    }
                },
                {
                    "id": "submit_button",
                    "component": {
                        "Button": {
                            "child": "submit_text",
                            "action": {
                                "name": "approval.submit",
                                "context": [
                                    {
                                        "key": "priority",
                                        "value": { "path": "/request/priority" }
                                    }
                                ]
                            }
                        }
                    }
                }
            ]
        }
    });

    assert!(tool.validate_args(&args).is_ok());
}

#[tokio::test]
async fn tool_execute_flat_args() {
    let tool = A2uiRenderTool::new();
    let ctx = ToolCallContext::test_default();
    let args = json!({
        "beginRendering": {"surfaceId": "s1", "root": "root"}
    });
    let output = tool.execute(args, &ctx).await.unwrap();
    assert_eq!(output.result.data["rendered"], true);
    assert_eq!(output.result.data["count"], 1);
}

#[test]
fn tool_accepts_legacy_messages_array() {
    let tool = A2uiRenderTool::new();
    let args = json!({"messages": [
        {"surfaceUpdate": {"surfaceId": "s1", "components": [{"id": "root", "component": {"Card": {"child": "content"}}}]}},
        {"beginRendering": {"surfaceId": "s1", "root": "root"}}
    ]});
    assert!(tool.validate_args(&args).is_ok());
}

#[test]
fn tool_rejects_empty_messages_array() {
    let tool = A2uiRenderTool::new();
    let err = tool.validate_args(&json!({"messages": []})).unwrap_err();
    assert!(err.to_string().contains("at least one"));
}

#[tokio::test]
async fn tool_execute_legacy_messages_array() {
    let tool = A2uiRenderTool::new();
    let ctx = ToolCallContext::test_default();
    let args = json!({"messages": [
        {"surfaceUpdate": {"surfaceId": "s1", "components": [{"id": "root", "component": {"Card": {"child": "content"}}}]}},
        {"beginRendering": {"surfaceId": "s1", "root": "root"}}
    ]});
    let output = tool.execute(args, &ctx).await.unwrap();
    assert_eq!(output.result.tool_name, A2UI_TOOL_NAME);
    assert_eq!(output.result.data["rendered"], true);
    assert_eq!(output.result.data["count"], 2);
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
    assert!(text.contains("surfaceUpdate"));
    assert!(text.contains("dataModelUpdate"));
    assert!(text.contains("beginRendering"));
    assert!(text.contains("deleteSurface"));
}

// -- execute() edge-case tests -----------------------------------------------

#[tokio::test]
async fn tool_execute_empty_object_fails_validation() {
    let tool = A2uiRenderTool::new();
    let err = tool.validate_args(&json!({})).unwrap_err();
    // Empty object has no message keys, so it should fail
    assert!(err.to_string().contains("at least one"));
}

#[tokio::test]
async fn tool_execute_multiple_message_types_in_single_call() {
    let tool = A2uiRenderTool::new();
    let ctx = ToolCallContext::test_default();
    let args = json!({"messages": [
        {"beginRendering": {"surfaceId": "s1", "root": "root"}},
        {"surfaceUpdate": {
            "surfaceId": "s1",
            "components": [{"id": "root", "component": {"Card": {"child": "c"}}}]
        }},
        {"deleteSurface": {"surfaceId": "s2"}}
    ]});
    assert!(tool.validate_args(&args).is_ok());
    let output = tool.execute(args, &ctx).await.unwrap();
    assert_eq!(output.result.data["rendered"], true);
    assert_eq!(output.result.data["count"], 3);
}

#[tokio::test]
async fn tool_execute_returns_empty_command() {
    let tool = A2uiRenderTool::new();
    let ctx = ToolCallContext::test_default();
    let args = json!({
        "beginRendering": {"surfaceId": "s1", "root": "root"}
    });
    let output = tool.execute(args, &ctx).await.unwrap();
    assert!(
        output.command.is_empty(),
        "A2UI render tool should not produce side-effects"
    );
}

#[test]
fn tool_descriptor_id_is_render_a2ui() {
    let tool = A2uiRenderTool::new();
    let desc = tool.descriptor();
    assert_eq!(desc.id, "render_a2ui");
}

// -- normalize_args tests ---------------------------------------------------

#[test]
fn normalize_rewraps_flattened_surface_update() {
    use super::tool::normalize_args;

    // Model generates surfaceId+components at top level alongside surfaceUpdate key
    let flattened = json!({
        "surfaceUpdate": {},
        "surfaceId": "form1",
        "components": [{"id": "root", "component": {"Card": {"child": "c"}}}]
    });

    let normalized = normalize_args(&flattened);
    let su = normalized.get("surfaceUpdate").expect("surfaceUpdate key");
    assert_eq!(su.get("surfaceId").unwrap(), "form1");
    assert!(su.get("components").unwrap().is_array());
    // Top-level should only have surfaceUpdate now
    assert!(normalized.get("surfaceId").is_none());
}

#[test]
fn normalize_rewraps_flattened_begin_rendering() {
    use super::tool::normalize_args;

    let flattened = json!({
        "beginRendering": {},
        "surfaceId": "form1",
        "root": "root",
        "components": []
    });

    let normalized = normalize_args(&flattened);
    let br = normalized
        .get("beginRendering")
        .expect("beginRendering key");
    assert_eq!(br.get("surfaceId").unwrap(), "form1");
    assert_eq!(br.get("root").unwrap(), "root");
}

#[test]
fn normalize_leaves_correct_args_unchanged() {
    use super::tool::normalize_args;

    let correct = json!({
        "surfaceUpdate": {
            "surfaceId": "form1",
            "components": [{"id": "r", "component": {"Text": {"text": {"literalString": "hi"}}}}]
        }
    });

    let normalized = normalize_args(&correct);
    assert_eq!(normalized, correct);
}

#[test]
fn normalize_returns_non_object_unchanged() {
    use super::tool::normalize_args;
    let arr = json!([1, 2, 3]);
    assert_eq!(normalize_args(&arr), arr);
}

#[test]
fn normalize_infers_surface_update_when_no_message_key() {
    use super::tool::normalize_args;

    // Model omits the message key entirely, just puts surfaceId+components
    let bare = json!({
        "surfaceId": "form1",
        "components": [{"id": "r", "component": {"Text": {"text": {"literalString": "hi"}}}}]
    });

    let normalized = normalize_args(&bare);
    assert!(normalized.get("surfaceUpdate").is_some());
    assert_eq!(normalized["surfaceUpdate"]["surfaceId"], "form1");
}

#[test]
fn normalize_rewraps_flattened_data_model_update() {
    use super::tool::normalize_args;

    let flattened = json!({
        "dataModelUpdate": {},
        "surfaceId": "form1",
        "contents": [{"key": "k", "valueString": "v"}],
        "path": "/data"
    });

    // The normalize function only triggers when both surfaceId and components are present at
    // top level. Without components, it returns the input unchanged. Verify it returns as-is.
    let normalized = normalize_args(&flattened);
    assert!(normalized.get("dataModelUpdate").is_some());
    assert!(normalized.get("surfaceId").is_some());
}

#[test]
fn normalize_rewraps_flattened_data_model_update_with_components() {
    use super::tool::normalize_args;

    // Models sometimes emit components alongside dataModelUpdate fields
    let flattened = json!({
        "dataModelUpdate": {},
        "surfaceId": "form1",
        "components": [],
        "contents": [{"key": "k", "valueString": "v"}],
        "path": "/data"
    });

    let normalized = normalize_args(&flattened);
    let dmu = normalized
        .get("dataModelUpdate")
        .expect("dataModelUpdate key");
    assert_eq!(dmu.get("surfaceId").unwrap(), "form1");
    assert_eq!(dmu.get("path").unwrap(), "/data");
    assert!(dmu.get("contents").unwrap().is_array());
}

#[test]
fn normalize_rewraps_flattened_delete_surface() {
    use super::tool::normalize_args;

    let flattened = json!({
        "deleteSurface": {},
        "surfaceId": "form1",
        "components": []
    });

    let normalized = normalize_args(&flattened);
    let ds = normalized.get("deleteSurface").expect("deleteSurface key");
    assert_eq!(ds.get("surfaceId").unwrap(), "form1");
}

// -- validation edge cases --

#[test]
fn rejects_non_object_message() {
    let msgs = vec![json!(42)];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("expected a JSON object"));
}

#[test]
fn rejects_message_type_not_object() {
    let msgs = vec![json!({"beginRendering": "not-an-object"})];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must be a JSON object"));
}

#[test]
fn rejects_surface_update_missing_components_array() {
    let msgs = vec![json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": "not-array"
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("components"));
}

#[test]
fn rejects_data_model_contents_not_array() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "contents": "not-array"
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("contents"));
}

#[test]
fn rejects_data_model_entry_not_object() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "contents": ["not-an-object"]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("contents[0]"));
}

#[test]
fn rejects_data_model_entry_empty_key() {
    let msgs = vec![json!({
        "dataModelUpdate": {
            "surfaceId": "s1",
            "contents": [{"key": "", "valueString": "x"}]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must not be empty"));
}

#[test]
fn rejects_surface_update_component_not_object() {
    let msgs = vec![json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": ["not-an-object"]
        }
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must be a JSON object"));
}

#[test]
fn rejects_begin_rendering_empty_root() {
    let msgs = vec![json!({
        "beginRendering": {"surfaceId": "s1", "root": ""}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("must not be empty"));
}

#[test]
fn rejects_surface_id_not_string() {
    let msgs = vec![json!({
        "deleteSurface": {"surfaceId": 123}
    })];
    let errs = validate_a2ui_messages(&msgs);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("surfaceId"));
}

// -- component payload validation --

#[test]
fn rejects_text_component_missing_text_object() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "t", "component": {"Text": {"text": "plain-string"}}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("Text.text"));
}

#[test]
fn rejects_text_component_no_text_field() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "t", "component": {"Text": {"label": "x"}}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("Text.text"));
}

#[test]
fn rejects_button_without_action() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "b", "component": {"Button": {"child": "label"}}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("action"));
}

#[test]
fn rejects_button_action_without_name() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "b", "component": {"Button": {"child": "label", "action": {"context": []}}}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("action.name"));
}

#[test]
fn rejects_button_context_entry_not_object() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "b", "component": {"Button": {
                    "child": "label",
                    "action": {"name": "do", "context": ["not-object"]}
                }}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("context[0]"));
}

#[test]
fn rejects_button_context_entry_missing_key() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "b", "component": {"Button": {
                    "child": "label",
                    "action": {"name": "do", "context": [{"value": {"path": "/x"}}]}
                }}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("context[0].key"));
}

#[test]
fn rejects_button_context_entry_value_not_object() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "b", "component": {"Button": {
                    "child": "label",
                    "action": {"name": "do", "context": [{"key": "k", "value": "string"}]}
                }}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("context[0].value"));
}

#[test]
fn rejects_multiple_choice_missing_options() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "mc", "component": {"MultipleChoice": {
                    "selections": {"path": "/x"}
                }}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("options"));
}

#[test]
fn rejects_multiple_choice_missing_selections() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "mc", "component": {"MultipleChoice": {
                    "options": [{"label": {"literalString": "A"}, "value": "a"}]
                }}}
            ]
        }
    });
    let err = tool.validate_args(&args).unwrap_err();
    assert!(err.to_string().contains("selections"));
}

#[test]
fn rejects_component_with_multiple_payloads() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "x", "component": {
                    "Text": {"text": {"literalString": "hi"}},
                    "Card": {"child": "c"}
                }}
            ]
        }
    });
    // Schema validation or custom validation should reject this
    assert!(tool.validate_args(&args).is_err());
}

#[test]
fn rejects_component_payload_not_object() {
    let tool = A2uiRenderTool::new();
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "x", "component": {"Text": "not-an-object"}}
            ]
        }
    });
    // Schema validation catches non-object component payloads
    assert!(tool.validate_args(&args).is_err());
}

#[test]
fn accepts_unknown_component_type() {
    let tool = A2uiRenderTool::new();
    // Unknown component types should pass (only Text, Button, MultipleChoice are validated deeply)
    let args = json!({
        "surfaceUpdate": {
            "surfaceId": "s1",
            "components": [
                {"id": "custom", "component": {"MyCustomWidget": {"foo": "bar"}}}
            ]
        }
    });
    assert!(tool.validate_args(&args).is_ok());
}

// -- validation error display --

#[test]
fn validation_error_display_format() {
    let err = A2uiValidationError {
        index: 2,
        message: "something wrong".into(),
    };
    assert_eq!(format!("{err}"), "message[2]: something wrong");
}

// -- plugin on_activate is no-op --

#[test]
fn plugin_on_activate_succeeds() {
    let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
    let spec = AgentSpec::default();
    let mut patch = MutationBatch::new();
    plugin.on_activate(&spec, &mut patch).unwrap();
}

// -- plugin instructions without examples has no example markers --

#[test]
fn plugin_without_examples_has_no_example_markers() {
    let plugin = A2uiPlugin::with_catalog_id(TEST_CATALOG);
    assert!(!plugin.instructions().contains("---BEGIN A2UI EXAMPLES---"));
    assert!(!plugin.instructions().contains("---END A2UI EXAMPLES---"));
}
