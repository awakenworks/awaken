// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The normalized stdio target carries the published initial tool view and keeps transport identity separate from its content.
//! Canonical, protocol-neutral identity for an MCP endpoint.

use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cached_tools: Vec<CachedMcpTool>,
    /// The operator declares that tools/list depends only on the immutable launch inputs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_tool_catalog: bool,
}

/// Initial model-facing definitions for a stdio server before its first connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CachedMcpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: serde_json::Value,
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

impl McpTargetIdentity {
    /// Render the sole canonical HTTP audience shared by credential authoring
    /// and Session execution. Cosmetic spelling can never fork authorization.
    #[must_use]
    pub fn canonical_url(&self) -> String {
        let port = self
            .port
            .map_or_else(String::new, |port| format!(":{port}"));
        let query = self
            .query
            .as_ref()
            .map_or_else(String::new, |query| format!("?{query}"));
        format!(
            "{}://{}{}{}{}",
            self.scheme, self.host, port, self.path, query
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpTargetError {
    #[error("MCP target must be an absolute HTTP(S) URL without userinfo or fragment")]
    Invalid,
    #[error(
        "sandbox stdio MCP command must be a non-empty executable and arguments cannot contain NUL"
    )]
    InvalidSandboxStdio,
    #[error("cached MCP tools require a stdio target, unique nonempty names and object schemas")]
    InvalidCachedTools,
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
            cached_tools: Vec::new(),
            share_tool_catalog: false,
        }))
    }

    /// Pin the initial tool view in the target carried by Agent and Session revisions.
    pub fn with_cached_tools(
        mut self,
        mut tools: Vec<CachedMcpTool>,
    ) -> Result<Self, McpTargetError> {
        let Self::SandboxStdio(target) = &mut self else {
            return Err(McpTargetError::InvalidCachedTools);
        };
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        if tools
            .iter()
            .any(|tool| tool.name.trim().is_empty() || !tool.input_schema.is_object())
            || tools.windows(2).any(|pair| pair[0].name == pair[1].name)
        {
            return Err(McpTargetError::InvalidCachedTools);
        }
        target.cached_tools = tools;
        Ok(self)
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
    fn stdio_cached_tools_preserve_command_identity_and_freeze_tool_content() {
        let plain = McpTarget::sandbox_stdio("stdio-server", vec!["--local".into()]).unwrap();
        let wire = serde_json::to_value(&plain).unwrap();
        assert!(wire.get("cached_tools").is_none());
        assert_eq!(serde_json::from_value::<McpTarget>(wire).unwrap(), plain);
        let tool = |name: &str, input_schema| CachedMcpTool {
            name: name.into(),
            description: String::new(),
            input_schema,
        };
        let one = tool("a", serde_json::json!({"type": "object"}));
        let two = tool("b", serde_json::json!({"type": "object"}));
        let pinned = plain
            .clone()
            .with_cached_tools(vec![two.clone(), one.clone()])
            .unwrap();
        assert_eq!(pinned.fingerprint(), plain.fingerprint());
        assert_eq!(
            pinned,
            plain
                .clone()
                .with_cached_tools(vec![one.clone(), two])
                .unwrap()
        );
        assert_ne!(
            crate::stable_fingerprint(&plain),
            crate::stable_fingerprint(&pinned)
        );
        assert_eq!(
            serde_json::from_value::<McpTarget>(serde_json::to_value(&pinned).unwrap()).unwrap(),
            pinned,
        );
        assert!(
            plain
                .clone()
                .with_cached_tools(vec![one.clone(), one])
                .is_err()
        );
        assert!(
            plain
                .with_cached_tools(vec![tool("array", serde_json::json!([]))])
                .is_err()
        );
    }

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
    fn canonical_http_audience_collapses_cosmetic_url_spellings() {
        // Cause/effect: C1 scheme/host case, default port, and trailing slash
        // differ while the parsed identity is equal; C2 a material port/path
        // differs. E1 C1 renders one audience; E2 C2 stays distinct.
        let canonical = McpTarget::identity("https://mcp.example.test/mcp").unwrap();
        let cosmetic = McpTarget::identity("HTTPS://MCP.EXAMPLE.TEST:443/mcp/").unwrap();
        assert_eq!(canonical.canonical_url(), cosmetic.canonical_url(), "C1/E1");
        assert_ne!(
            canonical.canonical_url(),
            McpTarget::identity("https://mcp.example.test:8443/mcp")
                .unwrap()
                .canonical_url(),
            "C2/E2"
        );
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
