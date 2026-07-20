//! Runtime capability discovery is a deterministic projection of the executable
//! ports actually composed on the node; it is not mutable Agent publication state.

use std::sync::Arc;

use awaken_runtime::Runtime;
use awaken_runtime_contract::capability::RuntimeCapabilitySource;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

struct NamedTool(&'static str);

#[async_trait::async_trait]
impl RawTool for NamedTool {
    fn id(&self) -> &str {
        self.0
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "ok"))
    }
}

#[test]
fn capabilities_are_projected_from_registered_runtime_ports() {
    let runtime = Runtime::new()
        .with_tool(Arc::new(NamedTool("zeta")))
        .with_tool(Arc::new(NamedTool("alpha")));

    let capabilities = runtime.runtime_capabilities();
    let ids: Vec<_> = capabilities
        .tools
        .iter()
        .map(|tool| tool.id.as_str())
        .collect();
    assert_eq!(ids, vec!["alpha", "zeta"]);
    assert!(!capabilities.catalog_fingerprint.0.is_empty());
    assert!(!capabilities.runtime_version.is_empty());
}

#[test]
fn capability_fingerprint_is_deterministic_and_content_sensitive() {
    let first = Runtime::new()
        .with_tool(Arc::new(NamedTool("beta")))
        .with_tool(Arc::new(NamedTool("alpha")))
        .runtime_capabilities();
    let reordered = Runtime::new()
        .with_tool(Arc::new(NamedTool("alpha")))
        .with_tool(Arc::new(NamedTool("beta")))
        .runtime_capabilities();
    let changed = Runtime::new()
        .with_tool(Arc::new(NamedTool("alpha")))
        .runtime_capabilities();

    assert_eq!(first.catalog_fingerprint, reordered.catalog_fingerprint);
    assert_ne!(first.catalog_fingerprint, changed.catalog_fingerprint);
}
