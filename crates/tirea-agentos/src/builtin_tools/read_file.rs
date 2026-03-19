use crate::contracts::runtime::{
    tool_call::{Tool, ToolDescriptor, ToolError, ToolResult},
    ToolCallContext,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::fs;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("read_file", "Read File", "Read file contents").with_parameters(json!({
            "type": "object",
            "properties": {
                "path": { "type": "string",
                "description": "Path to file to read"
                }
            },
            "required": ["path"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'path'".into()))?;

        let contents = fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to read'{path}':{e}")))?;
        Ok(ToolResult::success("read_file", contents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::testing::TestFixture;
    use serde_json::json;
    use tempfile::TempDir;
    #[tokio::test]
    async fn reads_file_contents_successfully() {
        let temp_dir = TempDir::new().expect("create temp dir");

        let path = temp_dir.path().join("note.txt");
        tokio::fs::write(&path, "hello from read_file")
            .await
            .expect("write temp file");
        let tool = ReadFileTool;
        let fixture = TestFixture::new();
        let result = tool
            .execute(json!({ "path": path }), &fixture.ctx())
            .await
            .expect("read succeeds");
        assert_eq!(result.tool_name, "read_file");
        assert_eq!(result.data, json!("hello from read_file"));
    }
    #[tokio::test]
    async fn returns_execution_failed_when_file_is_missing() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let missing_path = temp_dir.path().join("missing.txt");
        let tool = ReadFileTool;
        let fixture = TestFixture::new();
        let error = tool
            .execute(json!({ "path": missing_path }), &fixture.ctx())
            .await
            .expect_err("missing file should fail");
        match error {
            ToolError::ExecutionFailed(message) => {
                assert!(message.contains("missing.txt"));
            }
            other => panic!("expected execution failed error, got {other:?}"),
        }
    }
}
