//! The original live MCP probe fixture and its recorded observations.

use awaken_agent_contract::RedactedString;
use awaken_session_configuration_contract::{
    McpInspection, McpProbe, McpProbeFailure, McpProbeStatus,
};
use std::sync::{Arc, Mutex};

/// A fake live probe: answers a canned status and records every `(url, bearer)`
/// it is handed — the bearer arrives as the already-materialized secret (the
/// port signature takes a `RedactedString`, so a vault ref cannot cross it).
pub(super) struct FakeProbe {
    status: McpProbeStatus,
    /// The failing step reported with `status`. Its body echoes the bearer
    /// verbatim so tests prove the Vault owner scrubs without trusting the port.
    failure: Option<McpProbeFailure>,
    /// The verdict of every probe after the first (the post-refresh re-probe).
    after_first: Option<McpProbeStatus>,
    pub(super) seen: Mutex<Vec<(String, String)>>,
}

impl FakeProbe {
    pub(super) fn new(status: McpProbeStatus) -> Arc<Self> {
        Self::scripted(status, None, None)
    }

    pub(super) fn scripted(
        status: McpProbeStatus,
        failure: Option<McpProbeFailure>,
        after_first: Option<McpProbeStatus>,
    ) -> Arc<Self> {
        Arc::new(Self {
            status,
            failure,
            after_first,
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl McpProbe for FakeProbe {
    async fn probe(&self, mcp_server_url: &str, bearer: &RedactedString) -> McpProbeStatus {
        self.inspect(mcp_server_url, Some(bearer)).await.status
    }

    async fn inspect(&self, url: &str, bearer: Option<&RedactedString>) -> McpInspection {
        let bearer = bearer.map_or_else(String::new, |b| b.expose_secret().to_string());
        let mut seen = self.seen.lock().unwrap();
        seen.push((url.to_string(), bearer.clone()));
        if seen.len() > 1
            && let Some(status) = self.after_first
        {
            return McpInspection {
                status,
                tools: Vec::new(),
                failure: None,
            };
        }
        McpInspection {
            status: self.status,
            tools: Vec::new(),
            failure: self.failure.clone().map(|mut failure| {
                if let Some(response) = failure.http_response.as_mut() {
                    response.body = response.body.replace("{bearer}", &bearer);
                }
                failure
            }),
        }
    }
}
