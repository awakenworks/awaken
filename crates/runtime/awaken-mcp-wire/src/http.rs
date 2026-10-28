// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Shared MCP response projection preserves protocol data without creating authorization or Session state.
//! Direction-neutral MCP HTTP vocabulary and transparent relay response policy.

pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// Explicit destination opt-in, frozen in this connection's authored URL.
/// Default MCP services receive no Awaken execution identifiers. This is
/// correlation consent only; receivers must still authenticate and revalidate.
pub fn execution_context_enabled(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let mut selected = url
        .query_pairs()
        .filter(|(key, _)| key == "awaken_execution_context");
    matches!(selected.next(),Some((_,value)) if value=="run-agent-v1") && selected.next().is_none()
}

/// Preserve protocol and failure semantics without forwarding credentials,
/// cookies, message framing, or Connection-nominated hop headers. Adapters own
/// HTTP framework conversion; this wire helper preserves header value bytes.
pub fn relay_response_headers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Vec<(String, Vec<u8>)> {
    let headers: Vec<_> = headers.into_iter().collect();
    let hop_headers: Vec<_> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(|byte| *byte == b','))
        .map(|value| value.trim_ascii())
        .collect();
    const PRESERVED: &[&str] = &[
        "content-type",
        MCP_SESSION_ID_HEADER,
        MCP_PROTOCOL_VERSION_HEADER,
        "www-authenticate",
        "retry-after",
        "cache-control",
        "allow",
    ];
    headers
        .into_iter()
        .filter(|(name, _)| {
            PRESERVED
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
                && !hop_headers
                    .iter()
                    .any(|hop| name.as_bytes().eq_ignore_ascii_case(hop))
        })
        .map(|(name, value)| (name.to_owned(), value.to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_context_requires_an_exact_destination_opt_in() {
        for endpoint in [
            "https://other.invalid/mcp",
            "https://other.invalid/mcp?awaken_execution_context=no",
            "https://other.invalid/mcp?awaken_execution_context=run-agent-v1&awaken_execution_context=run-agent-v1",
            "https://u:p@other.invalid/mcp?awaken_execution_context=run-agent-v1",
            "https://other.invalid/mcp?awaken_execution_context=run-agent-v1#fragment",
        ] {
            assert!(!execution_context_enabled(endpoint));
        }
        assert!(execution_context_enabled(
            "http://127.0.0.1:3062/mcp?awaken_execution_context=run-agent-v1"
        ));
    }

    #[test]
    fn relay_header_decision_table_preserves_only_end_to_end_mcp_semantics() {
        // Causes: C1 permitted protocol/failure header (any casing), C2 named
        // by any Connection value, C3 credential/cookie/framing/unknown header.
        // Effects: E1 original value bytes retained; E2 removed. Rules:
        // R1 C1+!C2+!C3 -> E1; R2 C2 or C3 -> E2. Repeated permitted headers
        // retain their order and bytes; no transport Session state is created.
        let input = [
            ("Mcp-Session-Id", b"opaque-upstream-session".as_slice()),
            ("mcp-protocol-version", b"2025-06-18".as_slice()),
            ("Content-Type", b"text/event-stream".as_slice()),
            ("WWW-Authenticate", b"Bearer error=invalid_token".as_slice()),
            ("www-authenticate", b"opaque-\xff".as_slice()),
            ("retry-after", b"3".as_slice()),
            ("Cache-Control", b"no-cache".as_slice()),
            ("Allow", b"GET, POST, DELETE".as_slice()),
            ("Connection", b"keep-alive, Retry-After".as_slice()),
            ("connection", b"ALLOW".as_slice()),
            ("Authorization", b"Bearer forbidden".as_slice()),
            ("Set-Cookie", b"secret=forbidden".as_slice()),
            ("Content-Length", b"123".as_slice()),
            ("Transfer-Encoding", b"chunked".as_slice()),
            ("X-Unreviewed", b"forbidden".as_slice()),
        ];
        let actual = relay_response_headers(input);
        assert_eq!(
            actual,
            input[..5]
                .iter()
                .chain(input[6..7].iter())
                .map(|(name, value)| (name.to_string(), value.to_vec()))
                .collect::<Vec<_>>(),
            "R1/R2"
        );
        // R2 also holds if another Connection token has opaque bytes: a UTF-8
        // decode failure must not revive the ASCII hop header.
        assert!(
            relay_response_headers([
                (MCP_SESSION_ID_HEADER, b"session".as_slice()),
                ("connection", b"mcp-session-id,\xff".as_slice()),
            ])
            .is_empty()
        );
    }
}
