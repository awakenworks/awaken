// @formal surface exclude=external_port_contract
// @formal owner crates/bin/awaken-process-startup/tests/mcp_sessions.rs
// @formal reason: Defines the three-state MCP probe result, the refresh-exchange outcome, their port
// @formal reason: signatures, and the pure scrub-then-bound projection of observed HTTP evidence; live
// @formal reason: handshake classification is owned by the runtime-host adapter and the exchange by the
// @formal reason: credential materializer.
//! The live MCP credential-probe port (ADR-0043), plus the OAuth
//! refresh-exchange port and the scrubbed HTTP evidence a credential
//! validation reports.

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::CredentialRefreshAccess;
use awaken_credential_contract::CredentialSourceId;

/// Upper bound on the body bytes a [`ProbeHttpResponse`] carries.
pub const PROBE_HTTP_BODY_LIMIT: usize = 4096;

/// The placeholder that replaces every scrubbed value.
const SCRUBBED: &str = "[REDACTED]";

/// Sensitive JSON fields shared by credential probe and configuration inspection.
const SENSITIVE_KEYS: [&str; 16] = [
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "token",
    "password",
    "secret",
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "authorization_token",
    "private_key",
    "credential",
    "credentials",
    "reference",
];

/// One HTTP response a credential-validation probe observed, safe to project:
/// sensitive values are scrubbed and the body is bounded to
/// [`PROBE_HTTP_BODY_LIMIT`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeHttpResponse {
    pub status_code: u16,
    /// The `Content-Type` response header; empty when the response had none.
    pub content_type: String,
    pub body: String,
    /// The body was cut, either while it was captured or at the projection bound.
    pub body_truncated: bool,
}

impl ProbeHttpResponse {
    /// Scrub then bound one captured response. A JSON body has the string value
    /// of every [`SENSITIVE_KEYS`] member replaced; every body then has each
    /// literal in `secrets` replaced. Scrubbing precedes truncation, so a
    /// secret can never survive as a cut-off prefix.
    #[must_use]
    pub fn scrubbed(
        status_code: u16,
        content_type: Option<&str>,
        body: &str,
        captured_truncated: bool,
        secrets: &[&str],
    ) -> Self {
        let mut text = body.to_owned();
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body)
            && scrub_sensitive_keys(&mut value)
        {
            text = value.to_string();
        }
        for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
            text = text.replace(secret, SCRUBBED);
        }
        let mut end = text.len().min(PROBE_HTTP_BODY_LIMIT);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let body_truncated = captured_truncated || end < text.len();
        text.truncate(end);
        Self {
            status_code,
            content_type: content_type.unwrap_or_default().to_owned(),
            body: text,
            body_truncated,
        }
    }
}

/// Replace every sensitive field, at any depth, including structured values. Returns
/// whether anything was replaced.
/// SDK: none — internal safe-read projection, not a Managed command.
pub fn scrub_sensitive_keys(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            let mut changed = false;
            for (key, member) in object.iter_mut() {
                if SENSITIVE_KEYS.iter().any(|sensitive| {
                    key.chars()
                        .filter(|c| *c != '_' && *c != '-')
                        .flat_map(char::to_lowercase)
                        .eq(sensitive
                            .chars()
                            .filter(|c| *c != '_' && *c != '-')
                            .flat_map(char::to_lowercase))
                }) {
                    *member = serde_json::Value::String(SCRUBBED.to_owned());
                    changed = true;
                } else {
                    changed |= scrub_sensitive_keys(member);
                }
            }
            changed
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| scrub_sensitive_keys(item) | changed),
        _ => false,
    }
}

/// The step at which a live MCP probe failed: the JSON-RPC method it was
/// performing and, when the server answered, its scrubbed HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProbeFailure {
    pub method: String,
    pub http_response: Option<ProbeHttpResponse>,
}

/// How an OAuth refresh-token exchange ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshExchangeStatus {
    /// The token endpoint issued an access token and it was resealed.
    Succeeded,
    /// Refused, rejected before any request, or not resealed.
    Failed,
    /// No HTTP response: unreachable endpoint, timeout, or a broken stream.
    ConnectError,
}

/// The outcome of one refresh-token exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshExchangeOutcome {
    pub status: RefreshExchangeStatus,
    /// The token endpoint's scrubbed response, whenever one arrived.
    pub http_response: Option<ProbeHttpResponse>,
}

/// A port that performs one exact OAuth refresh-token exchange for a credential
/// revision and reseals the issued access token, reporting how it went. The
/// implementation owns the Secret Store and the token-endpoint client; the
/// caller only names the pinned refresh configuration.
#[async_trait::async_trait]
pub trait CredentialRefreshExchange: Send + Sync {
    async fn exchange(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> RefreshExchangeOutcome;
}

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
            failure: None,
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
    /// The failing step, when the probe ran and did not complete. Its HTTP
    /// response is already scrubbed of the probe's own bearer.
    pub failure: Option<McpProbeFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_http_response_scrubs_before_it_bounds() {
        // Decision table. S1 JSON sensitive key -> value replaced, other members
        // kept. S2 literal secret in a non-JSON body -> replaced. S3 secret
        // straddling the bound -> never survives as a prefix. S4 captured
        // truncation is preserved. S5 absent content type -> empty string.
        let s1 = ProbeHttpResponse::scrubbed(
            200,
            Some("application/json"),
            r#"{"access_token":"at-1","nested":{"refresh_token":"rt-1"},"expires_in":60}"#,
            false,
            &[],
        );
        let body: serde_json::Value = serde_json::from_str(&s1.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"access_token": SCRUBBED, "nested": {"refresh_token": SCRUBBED}, "expires_in": 60}),
            "S1"
        );
        assert_eq!(
            (s1.status_code, s1.content_type.as_str(), s1.body_truncated),
            (200, "application/json", false)
        );

        let s2 = ProbeHttpResponse::scrubbed(
            401,
            Some("text/plain"),
            "bad bearer tok-9",
            false,
            &["tok-9", ""],
        );
        assert_eq!(s2.body, "bad bearer [REDACTED]", "S2");

        let secret = "s".repeat(32);
        let raw = format!("{}{secret}", "x".repeat(PROBE_HTTP_BODY_LIMIT - 4));
        let s3 = ProbeHttpResponse::scrubbed(500, None, &raw, false, &[&secret]);
        assert_eq!(s3.body.len(), PROBE_HTTP_BODY_LIMIT, "S3");
        assert!(!s3.body.contains("ssss"), "S3");
        assert!(s3.body_truncated, "S3");
        assert_eq!(s3.content_type, "", "S5");

        let s4 = ProbeHttpResponse::scrubbed(502, None, "partial", true, &[]);
        assert!(s4.body_truncated, "S4");
    }
}
