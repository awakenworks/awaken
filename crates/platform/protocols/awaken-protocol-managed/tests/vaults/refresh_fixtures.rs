//! MCP OAuth refresh validation fixtures for the Vault route tests.

use super::*;

pub(super) fn refused_initialize() -> McpProbeFailure {
    McpProbeFailure {
        method: "initialize".into(),
        http_response: Some(ProbeHttpResponse {
            status_code: 401,
            content_type: "application/json".into(),
            body: r#"{"error":"invalid_token","echo":"{bearer}"}"#.into(),
            body_truncated: false,
        }),
    }
}

/// The exact `BetaManagedAgentsMCPProbe` projection of [`refused_initialize`]
/// after the Vault owner scrubbed the access token the fake echoed.
pub(super) fn refused_initialize_wire() -> Value {
    json!({
        "method": "initialize",
        "http_response": {
            "body": r#"{"error":"invalid_token","echo":"[REDACTED]"}"#,
            "body_truncated": false,
            "content_type": "application/json",
            "status_code": 401
        }
    })
}

pub(super) fn oauth_refresh_config() -> Value {
    json!({
        "client_id": "client-1",
        "refresh_token": "rt-secret", // awaken-allow(secret):
        "token_endpoint": "https://auth.example.com/token",
        "token_endpoint_auth": { "type": "none" }
    })
}
