//! The Managed anti-corruption boundary's one MCP target normalizer.

/// Canonical HTTP(S) identity used by Vault matching, duplicate rejection and
/// Session attachment compilation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NormalizedMcpUrl {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
    pub(crate) path: String,
    pub(crate) query: Option<String>,
}

pub(crate) fn normalize_mcp_server_url(raw: &str) -> Option<NormalizedMcpUrl> {
    if raw.contains('#') {
        return None;
    }
    let parsed = raw.parse::<axum::http::Uri>().ok()?;
    let scheme = parsed.scheme_str()?.to_ascii_lowercase();
    let authority = parsed.authority()?;
    if !matches!(scheme.as_str(), "http" | "https") || authority.as_str().contains('@') {
        return None;
    }
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = match (scheme.as_str(), authority.port_u16()) {
        ("http", Some(80)) | ("https", Some(443)) => None,
        (_, port) => port,
    };
    let path_and_query = parsed.path_and_query()?;
    Some(NormalizedMcpUrl {
        scheme,
        host,
        port,
        path: path_and_query.path().trim_end_matches('/').to_string(),
        query: path_and_query.query().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_normalization_decision_table() {
        // Cause graph: valid absolute HTTP(S) + no userinfo/fragment -> canonical
        // key; scheme/host case, default port and trailing slash are erased;
        // target-changing path/query/port remain; invalid causes reject.
        //
        // | Rule | HTTP(S) | userinfo/fragment | only cosmetic delta | Effect |
        // |------|---------|-------------------|---------------------|--------|
        // | U1   | T       | F                 | T                   | equal  |
        // | U2   | T       | F                 | F                   | differ |
        // | U3   | F       | -                 | -                   | reject |
        // | U4   | T       | T                 | -                   | reject |
        let canonical = normalize_mcp_server_url("https://mcp.example.test/sse").unwrap();
        assert_eq!(
            canonical,
            normalize_mcp_server_url("HTTPS://MCP.EXAMPLE.TEST:443/sse/").unwrap(),
            "U1"
        );
        assert_ne!(
            canonical,
            normalize_mcp_server_url("https://mcp.example.test:8443/sse").unwrap(),
            "U2"
        );
        assert!(normalize_mcp_server_url("file:///tmp/mcp").is_none(), "U3");
        for invalid in [
            "https://user:secret@mcp.example.test/sse",
            "https://mcp.example.test/sse#fragment",
        ] {
            assert!(normalize_mcp_server_url(invalid).is_none(), "U4: {invalid}");
        }
    }
}
