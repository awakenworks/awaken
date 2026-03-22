//! Tool ID sanitization: "server_name/tool_name" -> clean identifier.

use crate::error::McpError;

/// Sanitize a single component (server name or tool name) into a valid identifier part.
///
/// Replaces non-alphanumeric characters with underscores, collapses consecutive
/// underscores, and trims leading/trailing underscores.
pub(crate) fn sanitize_component(raw: &str) -> Result<String, McpError> {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.chars() {
        let keep = ch.is_ascii_alphanumeric();
        let next = if keep { ch } else { '_' };
        if next == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        out.push(next);
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        return Err(McpError::InvalidToolIdComponent(raw.to_string()));
    }
    Ok(out)
}

/// Build a tool ID from server name and tool name.
///
/// Format: `mcp__{sanitized_server}__{sanitized_tool}`
pub fn to_tool_id(server_name: &str, tool_name: &str) -> Result<String, McpError> {
    let s = sanitize_component(server_name)?;
    let t = sanitize_component(tool_name)?;
    Ok(format!("mcp__{s}__{t}"))
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
    fn special_characters_are_replaced() {
        assert_eq!(
            to_tool_id("my-server", "my.tool").unwrap(),
            "mcp__my_server__my_tool"
        );
    }

    #[test]
    fn consecutive_specials_are_collapsed() {
        assert_eq!(to_tool_id("a--b", "c..d").unwrap(), "mcp__a_b__c_d");
    }

    #[test]
    fn leading_trailing_underscores_are_trimmed() {
        assert_eq!(
            to_tool_id("-server-", "-tool-").unwrap(),
            "mcp__server__tool"
        );
    }

    #[test]
    fn empty_after_sanitize_is_error() {
        let err = to_tool_id("   ", "echo").unwrap_err();
        assert!(matches!(err, McpError::InvalidToolIdComponent(_)));
    }

    #[test]
    fn all_special_chars_is_error() {
        let err = to_tool_id("---", "echo").unwrap_err();
        assert!(matches!(err, McpError::InvalidToolIdComponent(_)));
    }

    #[test]
    fn unicode_is_replaced() {
        assert_eq!(
            to_tool_id("srv", "tool_\u{1F600}").unwrap(),
            "mcp__srv__tool"
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
    fn spaces_are_replaced_and_collapsed() {
        assert_eq!(
            to_tool_id("my server", "my  tool").unwrap(),
            "mcp__my_server__my_tool"
        );
    }

    #[test]
    fn conflict_case_a_dash_b_vs_a_underscore_b() {
        let id1 = to_tool_id("s", "a-b").unwrap();
        let id2 = to_tool_id("s", "a_b").unwrap();
        assert_eq!(id1, id2, "dash and underscore map to the same id");
    }
}
