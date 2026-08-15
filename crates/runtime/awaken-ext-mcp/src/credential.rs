//! Opaque credentials for authenticating to an MCP server.
//!
//! Re-exports the shared host-side credential vocabulary from
//! [`awaken_credential`] — the same seam the A2A outbound client uses, so the
//! challenge shape and refresh semantics cannot drift between wire clients.
//!
//! A [`Credential`] carries an already-resolved secret value (a bearer token or
//! a header pair) — never a vault reference or lookup policy. The host resolves
//! its secrets and hands this crate the opaque value, keeping credential
//! mechanics (vaults, OAuth refresh) out of the runtime/extension boundary
//! (D6/D9): this crate only attaches the value to a request.
//!
//! Rotation stays host-owned through two hooks on the HTTP transport: the host
//! can push a fresh value at any time
//! ([`set_credential`](crate::http::HttpTransport::set_credential)), and it can
//! register a [`CredentialRefresher`] that the transport calls when a server
//! answers 401/403 — the OAuth/vault machinery runs on the host side of that
//! callback. This crate rotates the value, then retries only methods classified
//! read-only by [`auth_retry_safety`]; effectful and unknown methods are never
//! replayed merely because an upstream returned an auth challenge.

use serde_json::Value;

pub use awaken_credential::{AuthChallenge, Credential, CredentialRefresher};

/// Whether a request may be sent a second time after credential rotation.
/// Unknown methods fail closed because an extension method may have effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRetrySafety {
    ReadOnly,
    Never,
}

/// Classify an MCP JSON-RPC request for automatic auth retry. This is the one
/// policy shared by the direct Streamable HTTP client and the sandbox relay.
#[must_use]
pub fn auth_retry_safety(request: &Value) -> AuthRetrySafety {
    match request.get("method").and_then(Value::as_str) {
        Some(
            "initialize"
            | "ping"
            | "tools/list"
            | "resources/list"
            | "resources/templates/list"
            | "resources/read"
            | "prompts/list"
            | "prompts/get"
            | "completion/complete",
        ) => AuthRetrySafety::ReadOnly,
        _ => AuthRetrySafety::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_retry_classifier_is_an_explicit_fail_closed_decision_table() {
        // Cause/effect graph: C1=protocol handshake or known read-only method;
        // C2=tools/call; C3=unknown/missing/notification method. Effects E1=one auth retry,
        // E2=credential may rotate but request is never replayed. Decision
        // rules MR1=C1=>E1; MR2=C2|C3=>E2. FMECA: a false E1 can duplicate an
        // external mutation, so every method outside the allowlist is MR2.
        for method in [
            "initialize",
            "ping",
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "prompts/list",
            "prompts/get",
            "completion/complete",
        ] {
            assert_eq!(
                auth_retry_safety(&json!({"jsonrpc":"2.0", "id":1, "method":method})),
                AuthRetrySafety::ReadOnly,
                "MR1 {method}"
            );
        }
        for request in [
            json!({"jsonrpc":"2.0", "id":1, "method":"tools/call"}),
            json!({"jsonrpc":"2.0", "id":1, "method":"vendor/mutate"}),
            json!({"jsonrpc":"2.0", "id":1}),
        ] {
            assert_eq!(auth_retry_safety(&request), AuthRetrySafety::Never, "MR2");
        }
    }
}
