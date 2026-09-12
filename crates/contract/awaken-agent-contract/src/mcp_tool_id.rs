//! The single owner of the `mcp__<server>__<tool>` tool-identity grammar.
//!
//! One MCP tool has exactly one spelling. Every producer builds it here and
//! every consumer decomposes it here, so a producer can never sanitize a name
//! that a consumer then compares in its raw authored form — the shape that let
//! a *disabled* toolset on server `github-mcp` silently apply no policy at all.
//!
//! # Why a sealed newtype
//!
//! The payload is private and there is no `Deserialize` into the raw form, so
//! the only ways to obtain an [`McpToolId`] are [`McpToolId::new`] (which
//! encodes) and [`McpToolId::parse`] (which validates an already-encoded id).
//! `format!("mcp__…")` and `strip_prefix("mcp__")` are forbidden everywhere
//! else by `scripts/ci/check_crate_boundaries.py`.
//!
//! # The component encoding is injective
//!
//! The historical encoding replaced every non-alphanumeric byte with `_`,
//! collapsed runs and trimmed the edges, so `a-b`, `a.b`, `a_b` and `-a-b-` all
//! became the single namespace `mcp__a_b__` — a policy authored for one silently
//! governed the others. This encoding has [`decode_component`] as a total left
//! inverse, so distinct server names always occupy distinct namespaces:
//!
//! * an ASCII alphanumeric byte is itself;
//! * `_` is itself, *unless* the next source character is also `_` (a literal
//!   `__` would otherwise forge the separator), in which case it is `-5f`;
//! * every other byte — `-` included — is `-` followed by its two lowercase hex
//!   digits, per UTF-8 byte.
//!
//! `-` therefore always introduces an escape, and an encoded component never
//! contains `__`, which is what makes the separator split unambiguous.
//! Names made only of ASCII alphanumerics and single underscores — `calc`,
//! `awaken_session`, `my_server` — are unchanged by the encoding.

use std::fmt;

/// The reserved namespace prefix every canonical MCP tool id carries.
pub const MCP_TOOL_ID_PREFIX: &str = "mcp__";
/// The separator between the encoded server and tool components.
pub const MCP_TOOL_ID_SEPARATOR: &str = "__";
/// The prefix of the dotted `mcp.<server>.<tool>` spelling some ACP agents report.
pub const MCP_DOTTED_TOOL_PREFIX: &str = "mcp.";

/// Why a raw name could not become part of a canonical MCP tool identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpToolIdError {
    /// A server or tool name was empty, so it has no identity to encode.
    EmptyComponent {
        /// Which of the two components was empty.
        component: McpToolIdComponent,
    },
}

/// Which half of `mcp__<server>__<tool>` a failure concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpToolIdComponent {
    /// The MCP server name (the policy namespace).
    Server,
    /// The tool name as the server advertises it.
    Tool,
}

impl McpToolIdComponent {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Tool => "tool",
        }
    }
}

impl fmt::Display for McpToolIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyComponent { component } => {
                write!(f, "MCP {} name must not be empty", component.as_str())
            }
        }
    }
}

impl std::error::Error for McpToolIdError {}

/// A canonical MCP tool identity: `mcp__<encoded server>__<encoded tool>`.
///
/// The payload is private on purpose. Construct with [`McpToolId::new`] from raw
/// names, or recover one with [`McpToolId::parse`]; there is no way to inject a
/// hand-spelled id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpToolId {
    id: String,
    /// Byte offset, inside `id`, at which the encoded server component ends.
    server_end: usize,
}

impl McpToolId {
    /// Build the identity of `tool_name` on `server_name` from their raw,
    /// as-configured spellings. This is the only constructor.
    pub fn new(server_name: &str, tool_name: &str) -> Result<Self, McpToolIdError> {
        let server = encode_component(server_name, McpToolIdComponent::Server)?;
        let tool = encode_component(tool_name, McpToolIdComponent::Tool)?;
        let server_end = MCP_TOOL_ID_PREFIX.len() + server.len();
        Ok(Self {
            id: format!("mcp__{server}__{tool}"),
            server_end,
        })
    }

    /// Recover the identity carried by an already-encoded id. `None` means the
    /// string is not a canonical MCP tool id and must not be treated as one.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let rest = id.strip_prefix("mcp__")?;
        let separator = rest.find(MCP_TOOL_ID_SEPARATOR)?;
        let (server, tool) = rest.split_at(separator);
        let tool = &tool[MCP_TOOL_ID_SEPARATOR.len()..];
        if server.is_empty() || tool.is_empty() || !is_encoded_component(server) {
            return None;
        }
        if !is_encoded_component(tool) {
            return None;
        }
        Some(Self {
            id: id.to_string(),
            server_end: MCP_TOOL_ID_PREFIX.len() + server.len(),
        })
    }

    /// Recover the identity behind the dotted `mcp.<server>.<tool>` spelling some
    /// ACP agents report instead of the canonical id.
    ///
    /// The tool name is the final dotted segment: MCP server names are operator
    /// aliases that routinely carry dots (`api.example.com`), while tool names
    /// are the server's own identifiers. Splitting on the *first* dot instead is
    /// what made `mcp.a.b.c` on server `a.b` decompose into a namespace that
    /// existed nowhere.
    #[must_use]
    pub fn from_dotted(dotted: &str) -> Option<Self> {
        let rest = dotted.strip_prefix(MCP_DOTTED_TOOL_PREFIX)?;
        let (server, tool) = rest.rsplit_once('.')?;
        Self::new(server, tool).ok()
    }

    /// The dotted spelling of `tool_name` on `server_name`, for the ACP peers
    /// that speak it. Raw names in, dotted name out; never reparsed as identity.
    #[must_use]
    pub fn dotted(server_name: &str, tool_name: &str) -> String {
        format!("mcp.{server_name}.{tool_name}")
    }

    /// The id prefix every tool on `server_name` shares — the namespace bound a
    /// plugin declares for its dynamic tools.
    pub fn namespace(server_name: &str) -> Result<String, McpToolIdError> {
        let server = encode_component(server_name, McpToolIdComponent::Server)?;
        Ok(format!("mcp__{server}__"))
    }

    /// Does `id` carry the reserved MCP namespace prefix? This answers "is this
    /// name claiming MCP identity", not "is it a well-formed id" ([`Self::parse`]).
    #[must_use]
    pub fn claims_namespace(id: &str) -> bool {
        id.starts_with(MCP_TOOL_ID_PREFIX)
    }

    /// The whole canonical id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// The encoded server component (the policy namespace segment).
    #[must_use]
    pub fn encoded_server(&self) -> &str {
        &self.id[MCP_TOOL_ID_PREFIX.len()..self.server_end]
    }

    /// The encoded tool component.
    #[must_use]
    pub fn encoded_tool(&self) -> &str {
        &self.id[self.server_end + MCP_TOOL_ID_SEPARATOR.len()..]
    }

    /// The raw server name this identity was built from.
    #[must_use]
    pub fn server_name(&self) -> String {
        decode_component(self.encoded_server())
    }

    /// The raw tool name this identity was built from.
    #[must_use]
    pub fn tool_name(&self) -> String {
        decode_component(self.encoded_tool())
    }

    /// Is this tool served by the server an operator configured as
    /// `configured_server_name`?
    ///
    /// The configured name is raw, so it is encoded before comparison. Comparing
    /// the *encoded* segment against the *raw* configured name is exactly the
    /// bug this type exists to make unwritable.
    #[must_use]
    pub fn is_served_by(&self, configured_server_name: &str) -> bool {
        encode_component(configured_server_name, McpToolIdComponent::Server)
            .is_ok_and(|encoded| encoded == self.encoded_server())
    }
}

impl fmt::Display for McpToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

impl From<McpToolId> for String {
    fn from(value: McpToolId) -> Self {
        value.id
    }
}

/// Encode one raw name into the identity charset, injectively. See the module
/// docs for the rule; [`decode_component`] is its total left inverse.
fn encode_component(raw: &str, component: McpToolIdComponent) -> Result<String, McpToolIdError> {
    if raw.is_empty() {
        return Err(McpToolIdError::EmptyComponent { component });
    }
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
        } else if byte == b'_' && bytes.get(index + 1) != Some(&b'_') {
            out.push('_');
        } else {
            out.push('-');
            out.push_str(&format!("{byte:02x}"));
        }
    }
    Ok(out)
}

/// Recover the raw name from an encoded component. Total: any leftover `-` that
/// is not followed by two hex digits decodes to itself, so a malformed component
/// never panics — it simply fails [`is_encoded_component`] before reaching here.
#[must_use]
pub fn decode_component(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'-'
            && let Some(byte) = hex_pair(bytes.get(index + 1), bytes.get(index + 2))
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(high: Option<&u8>, low: Option<&u8>) -> Option<u8> {
    let high = hex_digit(*high?)?;
    let low = hex_digit(*low?)?;
    Some(high * 16 + low)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Is this exactly the output shape [`encode_component`] produces? Round-tripping
/// is the test: anything an encoder never emits is rejected rather than silently
/// reinterpreted under a namespace it does not own.
fn is_encoded_component(candidate: &str) -> bool {
    encode_component(&decode_component(candidate), McpToolIdComponent::Server)
        .is_ok_and(|re_encoded| re_encoded == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_unchanged_by_the_encoding() {
        for name in ["calc", "awaken_session", "my_server", "Srv1", "123"] {
            assert_eq!(
                encode_component(name, McpToolIdComponent::Server).unwrap(),
                name,
                "{name} must keep its historical spelling"
            );
        }
        assert_eq!(
            McpToolId::new("calc", "add").unwrap().as_str(),
            "mcp__calc__add"
        );
    }

    #[test]
    fn distinct_server_names_never_share_a_namespace() {
        let spellings = ["a-b", "a.b", "a_b", "a b", "a__b", "-a-b-", "a-2db"];
        let mut namespaces = std::collections::BTreeSet::new();
        for name in spellings {
            assert!(
                namespaces.insert(McpToolId::namespace(name).unwrap()),
                "{name} collided with an earlier namespace"
            );
        }
        assert_eq!(namespaces.len(), spellings.len());
    }

    #[test]
    fn encoding_round_trips_through_decode() {
        for name in [
            "a-b",
            "a.b",
            "a_b",
            "a__b",
            "a___b",
            "-a-",
            "a-2db",
            "github-mcp",
            "api.example.com",
            "tool_\u{1F600}",
            "_",
            "__",
            "-",
        ] {
            let encoded = encode_component(name, McpToolIdComponent::Server).unwrap();
            assert_eq!(decode_component(&encoded), name, "round trip of {name:?}");
            assert!(
                !encoded.contains("__"),
                "{name:?} encoded to {encoded:?} which forges the separator"
            );
        }
    }

    #[test]
    fn a_double_underscore_server_cannot_forge_the_separator() {
        let forged = McpToolId::new("a__b", "c").unwrap();
        let honest = McpToolId::new("a", "b__c").unwrap();
        assert_ne!(forged.as_str(), honest.as_str());
        assert_eq!(forged.server_name(), "a__b");
        assert_eq!(forged.tool_name(), "c");
        assert_eq!(honest.server_name(), "a");
        assert_eq!(honest.tool_name(), "b__c");
    }

    #[test]
    fn parse_recovers_exactly_what_new_built() {
        let built = McpToolId::new("github-mcp", "search").unwrap();
        let parsed = McpToolId::parse(built.as_str()).expect("canonical id parses");
        assert_eq!(parsed, built);
        assert_eq!(parsed.server_name(), "github-mcp");
        assert_eq!(parsed.tool_name(), "search");
    }

    #[test]
    fn parse_rejects_non_canonical_spellings() {
        for candidate in [
            "search",
            "mcp__search",
            "mcp____search",
            "mcp__srv__",
            "mcp__github-mcp__search",
            "mcp__a__b.c",
        ] {
            assert!(
                McpToolId::parse(candidate).is_none(),
                "{candidate:?} must not parse as a canonical MCP tool id"
            );
        }
    }

    #[test]
    fn is_served_by_encodes_the_configured_name_before_comparing() {
        let id = McpToolId::new("github-mcp", "search").unwrap();
        assert!(id.is_served_by("github-mcp"));
        assert!(!id.is_served_by("github_mcp"));
        assert!(!id.is_served_by("github.mcp"));
        assert!(!id.is_served_by("githubmcp"));
    }

    #[test]
    fn dotted_tool_name_is_the_final_segment() {
        let id = McpToolId::from_dotted("mcp.a.b.c").expect("dotted name parses");
        assert_eq!(id.server_name(), "a.b");
        assert_eq!(id.tool_name(), "c");
        assert!(id.is_served_by("a.b"));
        assert_eq!(
            McpToolId::from_dotted("mcp.awaken_session.write")
                .unwrap()
                .as_str(),
            "mcp__awaken_session__write"
        );
        assert!(McpToolId::from_dotted("mcp.onlyserver").is_none());
        assert!(McpToolId::from_dotted("search").is_none());
    }

    #[test]
    fn empty_components_are_rejected() {
        assert_eq!(
            McpToolId::new("", "search").unwrap_err(),
            McpToolIdError::EmptyComponent {
                component: McpToolIdComponent::Server
            }
        );
        assert_eq!(
            McpToolId::new("srv", "").unwrap_err(),
            McpToolIdError::EmptyComponent {
                component: McpToolIdComponent::Tool
            }
        );
    }

    #[test]
    fn whitespace_only_names_keep_an_identity_instead_of_vanishing() {
        // The historical encoding trimmed these to nothing and errored; the
        // injective encoding has no lossy step, so they simply encode.
        let id = McpToolId::new("   ", "echo").unwrap();
        assert_eq!(id.server_name(), "   ");
        assert!(!id.is_served_by("  "));
    }

    #[test]
    fn namespace_is_the_shared_prefix_of_its_tools() {
        let namespace = McpToolId::namespace("github-mcp").unwrap();
        assert_eq!(namespace, "mcp__github-2dmcp__");
        assert!(
            McpToolId::new("github-mcp", "search")
                .unwrap()
                .as_str()
                .starts_with(&namespace)
        );
    }
}
