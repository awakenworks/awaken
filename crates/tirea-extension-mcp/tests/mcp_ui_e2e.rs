mod support;

use mcp::transport::McpServerConnectionConfig;
use mcp::McpToolDefinition;
use serde_json::json;
use std::sync::Arc;
use support::test_server::TestMcpTransport;
use tirea_extension_mcp::{McpToolRegistryManager, McpToolTransport};

fn cfg(name: &str) -> McpServerConnectionConfig {
    McpServerConnectionConfig::stdio(name, "unused", vec![])
}

fn tool_with_ui(name: &str, resource_uri: &str) -> McpToolDefinition {
    let mut def = McpToolDefinition::new(name)
        .with_title(name)
        .with_description(format!("Tool {name} with UI"));
    def.meta = Some(json!({"ui": {"resourceUri": resource_uri}}));
    def
}

fn tool_without_ui(name: &str) -> McpToolDefinition {
    McpToolDefinition::new(name)
        .with_title(name)
        .with_description(format!("Tool {name} without UI"))
}

async fn build_registry(
    transport: Arc<dyn McpToolTransport>,
) -> (McpToolRegistryManager, tirea_extension_mcp::McpToolRegistry) {
    let manager = McpToolRegistryManager::from_transports([(cfg("test_server"), transport)])
        .await
        .expect("build registry");
    let registry = manager.registry();
    (manager, registry)
}

#[tokio::test]
async fn tool_with_ui_resource_has_metadata_in_descriptor() {
    let transport = Arc::new(
        TestMcpTransport::new(vec![tool_with_ui("dashboard", "ui://dashboard/view")])
            .with_resource(
                "ui://dashboard/view",
                "<html><body>Dashboard</body></html>",
                "text/html",
            ),
    );

    let (_manager, registry) = build_registry(transport).await;
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.contains("dashboard"))
        .expect("find dashboard tool");
    let tool = registry.get(&tool_id).expect("get tool");
    let descriptor = tool.descriptor();

    assert_eq!(
        descriptor.metadata.get("mcp.ui.resourceUri"),
        Some(&json!("ui://dashboard/view")),
        "descriptor should contain mcp.ui.resourceUri metadata"
    );
}

#[tokio::test]
async fn tool_execution_fetches_ui_resource_content() {
    let transport = Arc::new(
        TestMcpTransport::new(vec![tool_with_ui("chart", "ui://chart/render")]).with_resource(
            "ui://chart/render",
            "<html><body><canvas id=\"chart\"></canvas></body></html>",
            "text/html",
        ),
    );

    let (_manager, registry) = build_registry(transport).await;
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.contains("chart"))
        .expect("find chart tool");
    let tool = registry.get(&tool_id).expect("get tool");

    let fix = tirea_contract::testing::TestFixture::new();
    let ctx = fix.ctx_with("ui-call", "test");
    let result = tool.execute(json!({"data": [1,2,3]}), &ctx).await.unwrap();

    assert!(result.is_success());
    assert_eq!(
        result.metadata.get("mcp.ui.resourceUri"),
        Some(&json!("ui://chart/render")),
    );
    assert_eq!(
        result.metadata.get("mcp.ui.content"),
        Some(&json!(
            "<html><body><canvas id=\"chart\"></canvas></body></html>"
        )),
    );
    assert_eq!(
        result.metadata.get("mcp.ui.mimeType"),
        Some(&json!("text/html")),
    );
}

#[tokio::test]
async fn tool_without_ui_has_no_ui_metadata() {
    let transport = Arc::new(TestMcpTransport::new(vec![tool_without_ui("echo")]));

    let (_manager, registry) = build_registry(transport).await;
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.contains("echo"))
        .expect("find echo tool");
    let tool = registry.get(&tool_id).expect("get tool");

    let descriptor = tool.descriptor();
    assert!(
        descriptor.metadata.get("mcp.ui.resourceUri").is_none(),
        "descriptor should not have mcp.ui.resourceUri"
    );

    let fix = tirea_contract::testing::TestFixture::new();
    let ctx = fix.ctx_with("no-ui-call", "test");
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(result.is_success());
    assert!(result.metadata.get("mcp.ui.content").is_none());
    assert!(result.metadata.get("mcp.ui.mimeType").is_none());
}

#[tokio::test]
async fn ui_resource_fetch_failure_does_not_break_tool_result() {
    // Tool declares a UI resource URI, but the transport doesn't have it.
    let transport = Arc::new(TestMcpTransport::new(vec![tool_with_ui(
        "broken_ui",
        "ui://broken/missing",
    )]));

    let (_manager, registry) = build_registry(transport).await;
    let tool_id = registry
        .ids()
        .into_iter()
        .find(|id| id.contains("broken_ui"))
        .expect("find broken_ui tool");
    let tool = registry.get(&tool_id).expect("get tool");

    let fix = tirea_contract::testing::TestFixture::new();
    let ctx = fix.ctx_with("broken-ui-call", "test");
    let result = tool.execute(json!({}), &ctx).await.unwrap();

    assert!(
        result.is_success(),
        "tool result should succeed even when UI resource fetch fails"
    );
    assert!(
        result.metadata.get("mcp.ui.content").is_none(),
        "no UI content when fetch fails"
    );
}

#[tokio::test]
async fn mixed_tools_with_and_without_ui() {
    let transport = Arc::new(
        TestMcpTransport::new(vec![
            tool_with_ui("with_ui", "ui://with_ui/view"),
            tool_without_ui("no_ui"),
        ])
        .with_resource("ui://with_ui/view", "<div>Hello</div>", "text/html"),
    );

    let (_manager, registry) = build_registry(transport).await;
    let ids = registry.ids();

    let ui_tool_id = ids.iter().find(|id| id.contains("with_ui")).unwrap();
    let no_ui_tool_id = ids.iter().find(|id| id.contains("no_ui")).unwrap();

    let ui_tool = registry.get(ui_tool_id).unwrap();
    let no_ui_tool = registry.get(no_ui_tool_id).unwrap();

    assert!(ui_tool
        .descriptor()
        .metadata
        .get("mcp.ui.resourceUri")
        .is_some());
    assert!(no_ui_tool
        .descriptor()
        .metadata
        .get("mcp.ui.resourceUri")
        .is_none());

    let fix = tirea_contract::testing::TestFixture::new();

    let ctx = fix.ctx_with("ui-exec", "test");
    let ui_result = ui_tool.execute(json!({}), &ctx).await.unwrap();
    assert_eq!(
        ui_result.metadata.get("mcp.ui.content"),
        Some(&json!("<div>Hello</div>"))
    );

    let ctx2 = fix.ctx_with("no-ui-exec", "test");
    let no_ui_result = no_ui_tool.execute(json!({}), &ctx2).await.unwrap();
    assert!(no_ui_result.metadata.get("mcp.ui.content").is_none());
}
