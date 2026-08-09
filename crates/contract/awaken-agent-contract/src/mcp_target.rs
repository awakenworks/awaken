//! Canonical, protocol-neutral identity for an MCP endpoint.

use serde::{Deserialize, Serialize};

/// One normalized MCP endpoint selected by an Agent and realized by a Session.
///
/// HTTP targets retain their historical untagged wire shape so existing Session
/// rows continue to deserialize. `sandbox_stdio` is an Awaken extension: the
/// executable is resolved inside the Session Environment, never on the Runtime
/// Host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpTarget {
    SandboxStdio(SandboxStdioMcpTarget),
    Http(HttpMcpTarget),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMcpTarget {
    pub url: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxStdioMcpTarget {
    #[serde(rename = "type")]
    pub kind: SandboxStdioMcpTargetKind,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxStdioMcpTargetKind {
    #[serde(rename = "sandbox_stdio")]
    SandboxStdio,
}

/// Canonical identity of an HTTP target, shared by Vault matching and egress
/// admission. Sandbox stdio targets intentionally have no network identity.
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
    #[error(
        "sandbox stdio MCP command must be a non-empty executable and arguments cannot contain NUL"
    )]
    InvalidSandboxStdio,
}

impl McpTarget {
    /// Parse and fingerprint one canonical HTTP(S) identity.
    pub fn parse_http(raw: impl Into<String>) -> Result<Self, McpTargetError> {
        let url = raw.into();
        let identity = Self::identity(&url)?;
        Ok(Self::Http(HttpMcpTarget {
            fingerprint: crate::stable_fingerprint(&(
                &identity.scheme,
                &identity.host,
                identity.port,
                &identity.path,
                &identity.query,
            )),
            url,
        }))
    }

    /// Validate and fingerprint a command executed inside the Session sandbox.
    pub fn sandbox_stdio(
        command: impl Into<String>,
        args: Vec<String>,
    ) -> Result<Self, McpTargetError> {
        let command = command.into();
        if command.trim().is_empty()
            || command.contains('\0')
            || args.iter().any(|arg| arg.contains('\0'))
        {
            return Err(McpTargetError::InvalidSandboxStdio);
        }
        Ok(Self::SandboxStdio(SandboxStdioMcpTarget {
            fingerprint: crate::stable_fingerprint(&("sandbox_stdio", &command, &args)),
            kind: SandboxStdioMcpTargetKind::SandboxStdio,
            command,
            args,
        }))
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        match self {
            Self::Http(target) => &target.fingerprint,
            Self::SandboxStdio(target) => &target.fingerprint,
        }
    }

    #[must_use]
    pub fn http_url(&self) -> Option<&str> {
        match self {
            Self::Http(target) => Some(&target.url),
            Self::SandboxStdio(_) => None,
        }
    }

    #[must_use]
    pub fn sandbox_stdio_target(&self) -> Option<&SandboxStdioMcpTarget> {
        match self {
            Self::SandboxStdio(target) => Some(target),
            Self::Http(_) => None,
        }
    }

    #[must_use]
    pub fn display_target(&self) -> String {
        match self {
            Self::Http(target) => target.url.clone(),
            Self::SandboxStdio(target) => format!("sandbox-stdio:{}", target.command),
        }
    }

    /// Parse the canonical HTTP identity without constructing desired state.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_http_shape_remains_compatible() {
        // Decision rule H1: given a valid HTTP target, serialization preserves
        // the legacy untagged `url` shape and deserialization recovers the same
        // target; this is the compatibility effect for existing Session rows.
        let target = McpTarget::parse_http("https://mcp.example.test/mcp").unwrap();
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["url"], "https://mcp.example.test/mcp");
        assert!(json.get("type").is_none());
        assert_eq!(serde_json::from_value::<McpTarget>(json).unwrap(), target);
    }

    #[test]
    fn sandbox_stdio_is_explicit_and_secret_free() {
        // Decision rules S1/S2: a non-empty command and NUL-free arguments
        // produce an explicit, round-trippable sandbox target; an empty command
        // is rejected. The serialized effect contains execution identity only.
        let target = McpTarget::sandbox_stdio(
            "playwright-mcp",
            vec!["--headless".into(), "--browser=chromium".into()],
        )
        .unwrap();
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["type"], "sandbox_stdio");
        assert_eq!(json["command"], "playwright-mcp");
        assert_eq!(serde_json::from_value::<McpTarget>(json).unwrap(), target);
        assert!(McpTarget::sandbox_stdio(" ", Vec::new()).is_err());
    }
}
