//! Canonical, protocol-neutral identity for an HTTP(S) MCP endpoint.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTarget {
    pub url: String,
    pub fingerprint: String,
}

/// Canonical HTTP(S) target identity shared by authoring, Vault matching,
/// persistence migration, duplicate rejection, and generation diffing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpTargetIdentity {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpTargetError {
    #[error("MCP target must be an absolute HTTP(S) URL without userinfo or fragment")]
    Invalid,
}

impl McpTarget {
    /// Parse and fingerprint the sole canonical MCP HTTP(S) identity.
    pub fn parse_http(raw: impl Into<String>) -> Result<Self, McpTargetError> {
        let url = raw.into();
        let identity = Self::identity(&url)?;
        Ok(Self {
            fingerprint: crate::stable_fingerprint(&(
                &identity.scheme,
                &identity.host,
                identity.port,
                &identity.path,
                &identity.query,
            )),
            url,
        })
    }

    /// Parse the same canonical identity used by [`Self::parse_http`] without
    /// constructing desired Session state. Vault and Agent admission reuse it.
    pub fn identity(raw: &str) -> Result<McpTargetIdentity, McpTargetError> {
        if raw.contains('#') {
            return Err(McpTargetError::Invalid);
        }
        let parsed = raw
            .parse::<http::Uri>()
            .map_err(|_| McpTargetError::Invalid)?;
        let scheme = parsed
            .scheme_str()
            .map(str::to_ascii_lowercase)
            .ok_or(McpTargetError::Invalid)?;
        let authority = parsed.authority().ok_or(McpTargetError::Invalid)?;
        if !matches!(scheme.as_str(), "http" | "https") || authority.as_str().contains('@') {
            return Err(McpTargetError::Invalid);
        }
        let host = authority.host().to_ascii_lowercase();
        if host.is_empty() {
            return Err(McpTargetError::Invalid);
        }
        let port = match (scheme.as_str(), authority.port_u16()) {
            ("http", Some(80)) | ("https", Some(443)) => None,
            (_, port) => port,
        };
        let path_and_query = parsed.path_and_query().ok_or(McpTargetError::Invalid)?;
        Ok(McpTargetIdentity {
            scheme,
            host,
            port,
            path: path_and_query.path().trim_end_matches('/').to_string(),
            query: path_and_query.query().map(str::to_string),
        })
    }
}
