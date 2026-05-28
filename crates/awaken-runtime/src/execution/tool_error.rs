use awaken_runtime_contract::contract::tool::{ToolError, ToolResult};

pub(crate) fn tool_error_result(tool_name: &str, error: ToolError) -> ToolResult {
    let timed_out = matches!(error, ToolError::Timeout(_));
    let result = ToolResult::error(tool_name, error.to_string());
    if timed_out {
        result.with_metadata("timed_out", true)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_tool_error_sets_timed_out_metadata() {
        let result = tool_error_result("slow", ToolError::Timeout("deadline".into()));
        assert_eq!(result.metadata["timed_out"], true);
    }
}
