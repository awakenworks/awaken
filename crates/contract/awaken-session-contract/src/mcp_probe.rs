//! The live MCP credential-probe port (ADR-0043).

use awaken_agent_contract::RedactedString;

/// The status a live MCP probe reports for an `mcp_oauth` credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProbeStatus {
    /// Connect + MCP `initialize` handshake succeeded with the bearer.
    Valid,
    /// The server refused the bearer with an auth challenge (401/403).
    Invalid { http_status: u16 },
    /// No verdict: unreachable, protocol error, or otherwise inconclusive.
    Unknown,
}

/// A port that live-probes an MCP server with an already-materialized bearer.
/// The implementation (server-local, backed by `awaken-ext-mcp`) is the only
/// place the MCP client is named — the wire adapter stays wire-client-free. The
/// signature takes the resolved secret, never a vault ref: materialization
/// happens on the implementor's side of the port.
#[async_trait::async_trait]
pub trait McpProbe: Send + Sync {
    async fn probe(&self, mcp_server_url: &str, bearer: &RedactedString) -> McpProbeStatus;
    /// Anonymous and bearer inspection use the same transport owner. Older
    /// probe implementations retain honest unknown evidence when unsupported.
    async fn inspect(&self, url: &str, bearer: Option<&RedactedString>) -> McpInspection {
        McpInspection {
            status: match bearer {
                Some(value) => self.probe(url, value).await,
                None => McpProbeStatus::Unknown,
            },
            tools: Vec::new(),
        }
    }
}

/// Ephemeral discovery evidence, never another persisted tool catalog.
#[derive(Clone, Debug, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpProbeTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}
#[derive(Clone, Debug)]
pub struct McpInspection {
    pub status: McpProbeStatus,
    pub tools: Vec<McpProbeTool>,
}
