//! Unit tests for the four management tools. The `CapabilityReader` /
//! `DraftValidator` ports are exercised with in-crate test doubles; the real
//! adapters (over the shared catalog and `ConfigService`) are wired and tested in
//! the host.

use super::*;

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        tool_id: id.into(),
        arguments: args,
    }
}

struct FakeCaps;
impl CapabilityReader for FakeCaps {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            agents: vec!["assistant".into()],
            models: vec!["m-1".into(), "m-2".into()],
            providers: vec!["openai".into()],
            tools: vec!["read".into(), "glob".into()],
            plugins: vec![PluginInfo {
                id: "state_machine".into(),
                schema_keys: vec!["state_machine".into()],
            }],
            skills: vec!["greet".into()],
            mcp_servers: vec![],
        }
    }
}

/// Rejects any draft that names a tool id containing "ghost" (stands in for the real
/// compile-time `UnknownTool` fence).
struct FakeValidator;
impl DraftValidator for FakeValidator {
    fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        if draft.tool_ids.iter().any(|t| t.contains("ghost")) {
            Err("references unknown tool".into())
        } else {
            Ok(())
        }
    }
}

fn tools() -> Vec<Arc<dyn RawTool>> {
    admin_tools(Arc::new(FakeCaps), Arc::new(FakeValidator))
}

fn tool(id: &str) -> Arc<dyn RawTool> {
    tools()
        .into_iter()
        .find(|t| t.id() == id)
        .expect("tool exists")
}

#[test]
fn descriptors_are_the_four_admin_tools_and_carry_no_publish_tool() {
    let ids: Vec<String> = admin_tool_descriptors()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            SET_PLUGIN_TOOL,
            VALIDATE_TOOL
        ]
    );
    // There is deliberately no publish tool (D4): publication is a console action.
    assert!(!ids.iter().any(|id| id.contains("publish")));
    // Every descriptor is namespaced under the admin owner prefix.
    assert!(
        admin_tool_descriptors()
            .iter()
            .all(|d| d.content_hash.starts_with("admin:"))
    );
    // The executable set matches the advertised descriptor set one-to-one.
    let exec_ids: Vec<String> = tools().iter().map(|t| t.id().to_string()).collect();
    assert_eq!(exec_ids, ids);
}

#[tokio::test]
async fn capabilities_returns_the_redacted_org_shared_view() {
    let out = tool(CAPABILITIES_TOOL)
        .invoke(call(CAPABILITIES_TOOL, serde_json::json!({})))
        .await
        .unwrap();
    assert!(!out.is_error);
    let caps: PlatformCapabilities = serde_json::from_str(&out.content).unwrap();
    assert_eq!(caps.models, vec!["m-1", "m-2"]);
    assert_eq!(caps.plugins[0].id, "state_machine");
    // No secret-bearing field exists on the view — it is redacted by construction.
    assert!(!out.content.contains("api_key") && !out.content.contains("credential"));
}

#[tokio::test]
async fn create_draft_auto_binds_the_model_and_never_publishes() {
    let out = tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "support",
                "instructions": "be helpful",
                "tool_ids": ["read"],
                "max_steps": 5
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error);
    let draft: AgentConfig = serde_json::from_str(&out.content).unwrap();
    assert_eq!(draft.id, "support");
    assert_eq!(draft.max_steps, 5);
    assert_eq!(draft.tool_ids, vec!["read".to_string()]);
    // The draft is auto-bound (D5) — the operator never hand-picks a model.
    assert!(draft.model_binding.is_auto());
    // A draft is data only: the wire carries {"mode":"auto"} for the binding.
    assert!(out.content.contains("\"mode\":\"auto\""));
}

#[tokio::test]
async fn create_draft_rejects_bad_arguments_without_aborting() {
    let out = tool(CREATE_DRAFT_TOOL)
        .invoke(call(CREATE_DRAFT_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    // Missing `instructions` is a model-visible error, not a hard abort.
    assert!(out.is_error);
    assert!(out.content.contains("invalid arguments"));
}

#[tokio::test]
async fn set_plugin_config_attaches_and_validates() {
    let draft = serde_json::json!({
        "id": "support",
        "instructions": "be helpful",
        "max_steps": 8,
        "model_binding": { "mode": "auto" },
        "tool_ids": []
    });
    let out = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "machines": [] }
            }),
        ))
        .await
        .unwrap();
    assert!(
        !out.is_error,
        "attaching a valid section succeeds: {}",
        out.content
    );
    let updated: AgentConfig = serde_json::from_str(&out.content).unwrap();
    assert_eq!(updated.plugin_ids, vec!["state_machine".to_string()]);
    assert_eq!(
        updated.plugin_config.get("state_machine"),
        Some(&serde_json::json!({ "machines": [] }))
    );
}

#[tokio::test]
async fn set_plugin_config_size_bounds_the_section() {
    let draft = serde_json::json!({
        "id": "support", "instructions": "be helpful", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": []
    });
    let big = "x".repeat(MAX_PLUGIN_CONFIG_BYTES + 1);
    let out = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "blob": big }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("over the"));
}

#[tokio::test]
async fn validate_agent_reports_valid_and_invalid() {
    let ok_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["read"]
    });
    let ok = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": ok_draft }),
        ))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ok.content).unwrap()["valid"],
        true
    );

    let bad_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["ghost_tool"]
    });
    let bad = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": bad_draft }),
        ))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&bad.content).unwrap();
    assert_eq!(parsed["valid"], false);
    assert!(parsed["error"].as_str().unwrap().contains("unknown tool"));
}

#[test]
fn seed_config_is_an_ordinary_auto_bound_config_naming_the_four_tools() {
    let cfg = admin_assistant_config();
    assert_eq!(cfg.id, ADMIN_ASSISTANT_AGENT_ID);
    // Auto-bound (D5), names exactly the four admin tools (D3), no plugins, no
    // sandbox concept — an ordinary AgentConfig (D1/D4).
    assert!(cfg.model_binding.is_auto());
    assert_eq!(
        cfg.tool_ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            SET_PLUGIN_TOOL,
            VALIDATE_TOOL
        ]
    );
    assert!(cfg.instructions.contains("management assistant"));
}

#[test]
fn seeded_instructions_are_authorable_and_mention_no_publish() {
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("management assistant"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("never publish"));
}
