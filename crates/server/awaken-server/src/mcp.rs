//! MCP composition for the shared host (ADR-0043 Phase 3).
//!
//! Owns the wire-client half of per-thread MCP wiring: given the servers a
//! session staged (already credential-materialized), connect each through
//! `awaken-ext-mcp` and hand back the executable tools + model-visible
//! descriptors the host registers on that thread's runtime. Split out of
//! `host.rs` so the host keeps session orchestration and this module owns the
//! outbound wire composition.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;

use crate::host::HostError;

/// An MCP server prepared for one thread (ADR-0043 Phase 3): the connection
/// target plus an already-materialized bearer token (`None` = connect
/// unauthenticated and let the server decide). Registered before the thread's
/// first turn via [`SharedHost::register_thread_mcp`](crate::SharedHost::register_thread_mcp)
/// and consumed by the host's session build, which connects through
/// `awaken-ext-mcp` and registers the discovered tools.
/// Only the resolved `RedactedString` crosses in (D6/D9) — never a vault ref.
#[derive(Clone)]
pub struct PreparedMcpServer {
    pub name: String,
    pub url: String,
    pub bearer: Option<awaken_agent_contract::RedactedString>,
}

/// The discovered surface of one thread's staged MCP servers: the executable
/// tools to register on the runtime, their model-visible descriptors for the
/// advertised config, and the namespaced tool ids the gate pre-authorizes.
pub struct McpWiring {
    pub tools: Vec<Arc<dyn RawTool>>,
    pub descriptors: Vec<ToolDescriptor>,
    pub tool_ids: Vec<String>,
}

/// Connect every staged server and collect the discovered tools. Fail closed:
/// a configured server that cannot connect (network, 401, bad wire) fails the
/// build — never a silent skip — naming the server so the error is actionable.
pub async fn connect_staged(staged: &[PreparedMcpServer]) -> Result<McpWiring, HostError> {
    let mut wiring = McpWiring {
        tools: Vec::new(),
        descriptors: Vec::new(),
        tool_ids: Vec::new(),
    };
    for server in staged {
        let credential = match &server.bearer {
            Some(token) => awaken_ext_mcp::Credential::Bearer(token.expose_secret().to_string()),
            None => awaken_ext_mcp::Credential::None,
        };
        let transport = awaken_ext_mcp::HttpTransport::connect(&server.url, credential)
            .await
            .map_err(|e| {
                HostError::internal(format!(
                    "mcp server `{}` at {}: {e}",
                    server.name, server.url
                ))
            })?;
        let connection = awaken_ext_mcp::connect_tools(&server.name, Arc::new(transport))
            .await
            .map_err(|e| HostError::internal(format!("mcp server `{}`: {e}", server.name)))?;
        wiring
            .tool_ids
            .extend(connection.descriptors.iter().map(|d| d.id.clone()));
        wiring.descriptors.extend(connection.descriptors);
        wiring.tools.extend(connection.tools);
    }
    Ok(wiring)
}
