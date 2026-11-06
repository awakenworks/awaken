// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Control's live credential check: one bounded MCP handshake with the caller's bearer,
// @formal reason: and a failure report with that bearer scrubbed from the captured response. It
// @formal reason: selects, stores and widens no credential; classification, not a formal-proof claim.
//! The live MCP credential probe Control's credential validation drives
//! (the `mcp_oauth_validate` route and the Vault's credential check). It is
//! Control's adapter of the [`McpProbe`] port over the MCP client, so Control
//! names no Worker-side MCP attachment lifecycle.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::RedactedString;
use awaken_ext_mcp::{Credential, HttpTransportBuilder, McpRequestTrace, McpToolTransport};
use awaken_session_configuration_contract::{McpProbe, McpProbeStatus, ProbeHttpResponse};

/// The live MCP credential probe (ADR-0043), implementing the
/// [`awaken_session_configuration_contract::McpProbe`] port over `awaken-ext-mcp`: connect +
/// MCP `initialize` handshake + `tools/list` with the materialized bearer.
/// Completion → `Valid`; a final auth challenge (401/403) → `Invalid` with the
/// HTTP status; anything else (unreachable, protocol or server error, timeout)
/// → `Unknown`. Every non-completion also reports the failing JSON-RPC method
/// and, when the server answered, its response with the bearer scrubbed. This
/// is the only place the MCP client is named for validation — the protocol
/// adapter crates stay wire-client-free.
pub struct ExtMcpProbe;

/// The whole probe, including a slow failed-response body, is bounded.
const MCP_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[async_trait::async_trait]
impl McpProbe for ExtMcpProbe {
    async fn probe(&self, url: &str, bearer: &RedactedString) -> McpProbeStatus {
        self.inspect(url, Some(bearer)).await.status
    }
    async fn inspect(
        &self,
        url: &str,
        bearer: Option<&RedactedString>,
    ) -> awaken_session_configuration_contract::McpInspection {
        use awaken_session_configuration_contract::{McpInspection, McpProbeFailure, McpProbeTool};
        let trace = Arc::new(McpRequestTrace::new());
        let mut builder =
            HttpTransportBuilder::new(url.to_string()).request_trace(Arc::clone(&trace));
        if let Some(bearer) = bearer {
            builder = builder.credential(Credential::Bearer(bearer.expose_secret().to_string()));
        }
        let result = tokio::time::timeout(MCP_PROBE_TIMEOUT, async {
            let transport = builder.connect().await?;
            transport.list_tools().await
        })
        .await;
        if let Ok(Ok(tools)) = result {
            return McpInspection {
                status: McpProbeStatus::Valid,
                tools: tools
                    .into_iter()
                    .map(|tool| McpProbeTool {
                        name: tool.name,
                        description: tool.description,
                        input_schema: tool.input_schema,
                    })
                    .collect(),
                failure: None,
            };
        }
        let captured = trace.last_failure();
        let status = match &captured {
            Some(failure) if failure.status == 401 || failure.status == 403 => {
                McpProbeStatus::Invalid {
                    http_status: failure.status,
                }
            }
            _ => McpProbeStatus::Unknown,
        };
        let secrets: Vec<&str> = bearer
            .map(RedactedString::expose_secret)
            .into_iter()
            .collect();
        McpInspection {
            status,
            tools: Vec::new(),
            failure: Some(McpProbeFailure {
                // No request attempted means the handshake never started.
                method: trace
                    .last_method()
                    .unwrap_or_else(|| "initialize".to_owned()),
                http_response: captured.map(|failure| {
                    ProbeHttpResponse::scrubbed(
                        failure.status,
                        failure.content_type.as_deref(),
                        &failure.body,
                        failure.body_truncated,
                        &secrets,
                    )
                }),
            }),
        }
    }
}
