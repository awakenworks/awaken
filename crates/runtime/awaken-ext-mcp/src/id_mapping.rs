//! Tool ID projection: `(server_name, tool_name)` -> the canonical MCP tool id.
//!
//! The grammar itself is owned by [`awaken_agent_contract::McpToolId`]; this
//! module only adapts its error into [`McpError`] so the MCP extension keeps a
//! single error type. Nothing here re-spells `mcp__<server>__<tool>`.

use crate::error::McpError;
use awaken_agent_contract::McpToolId;

/// Build a tool ID from server name and tool name.
///
/// Format: `mcp__{encoded_server}__{encoded_tool}`. The encoding is injective,
/// so two differently punctuated server names can never share a namespace.
pub fn to_tool_id(server_name: &str, tool_name: &str) -> Result<String, McpError> {
    McpToolId::new(server_name, tool_name)
        .map(String::from)
        .map_err(|error| McpError::InvalidToolIdComponent(error.to_string()))
}

/// The id *prefix* every tool from `server_name` shares — the namespace bound a
/// plugin declares for its dynamic tools (`mcp__{encoded_server}__`).
pub fn tool_namespace(server_name: &str) -> Result<String, McpError> {
    McpToolId::namespace(server_name)
        .map_err(|error| McpError::InvalidToolIdComponent(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_tool_id() {
        assert_eq!(
            to_tool_id("my_server", "my_tool").unwrap(),
            "mcp__my_server__my_tool"
        );
    }

    #[test]
    fn special_characters_are_escaped_rather_than_collapsed() {
        assert_eq!(
            to_tool_id("my-server", "my.tool").unwrap(),
            "mcp__my-2dserver__my-2etool"
        );
    }

    #[test]
    fn distinct_punctuation_keeps_distinct_ids() {
        let ids = ["a-b", "a.b", "a_b", "a--b"]
            .map(|name| to_tool_id("s", name).unwrap())
            .to_vec();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?} must all differ");
    }

    #[test]
    fn leading_trailing_punctuation_is_preserved() {
        assert_eq!(
            to_tool_id("-server-", "-tool-").unwrap(),
            "mcp__-2dserver-2d__-2dtool-2d"
        );
    }

    #[test]
    fn empty_component_is_error() {
        let err = to_tool_id("", "echo").unwrap_err();
        assert!(matches!(err, McpError::InvalidToolIdComponent(_)));
    }

    #[test]
    fn whitespace_only_name_keeps_an_identity() {
        assert_eq!(to_tool_id("   ", "echo").unwrap(), "mcp__-20-20-20__echo");
    }

    #[test]
    fn unicode_is_escaped_per_utf8_byte() {
        assert_eq!(
            to_tool_id("srv", "tool_\u{1F600}").unwrap(),
            "mcp__srv__tool_-f0-9f-98-80"
        );
    }

    #[test]
    fn numeric_names_work() {
        assert_eq!(to_tool_id("123", "456").unwrap(), "mcp__123__456");
    }

    #[test]
    fn mixed_case_preserved() {
        assert_eq!(
            to_tool_id("MyServer", "MyTool").unwrap(),
            "mcp__MyServer__MyTool"
        );
    }

    #[test]
    fn single_char_names() {
        assert_eq!(to_tool_id("a", "b").unwrap(), "mcp__a__b");
    }

    #[test]
    fn spaces_are_escaped_not_collapsed() {
        assert_eq!(
            to_tool_id("my server", "my  tool").unwrap(),
            "mcp__my-20server__my-20-20tool"
        );
    }

    #[test]
    fn a_dash_and_an_underscore_no_longer_share_an_id() {
        let id1 = to_tool_id("s", "a-b").unwrap();
        let id2 = to_tool_id("s", "a_b").unwrap();
        assert_ne!(id1, id2, "dash and underscore must stay distinct");
    }

    #[test]
    fn a_double_underscore_cannot_forge_the_separator() {
        let id = to_tool_id("a__b", "c").unwrap();
        assert_eq!(id, "mcp__a-5f_b__c");
        assert_eq!(
            awaken_agent_contract::McpToolId::parse(&id)
                .expect("canonical")
                .server_name(),
            "a__b"
        );
    }

    #[test]
    fn namespace_is_the_shared_prefix() {
        let namespace = tool_namespace("my-server").unwrap();
        assert_eq!(namespace, "mcp__my-2dserver__");
        assert!(
            to_tool_id("my-server", "t")
                .unwrap()
                .starts_with(&namespace)
        );
        assert!(matches!(
            tool_namespace("").unwrap_err(),
            McpError::InvalidToolIdComponent(_)
        ));
    }
}
