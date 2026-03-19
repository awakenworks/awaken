use crate::contracts::runtime::{
    tool_call::{Tool, ToolDescriptor, ToolError, ToolResult},
    ToolCallContext,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::fs;

const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_MAX_BYTES_LABEL: &str = "10 MiB";

#[derive(Debug, Clone)]
pub struct ReadFileTool {
    sandbox_root: Option<PathBuf>,
    max_bytes: u64,
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self {
            sandbox_root: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sandbox_root(mut self, sandbox_root: impl Into<PathBuf>) -> Self {
        self.sandbox_root = Some(sandbox_root.into());
        self
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    async fn resolve_sandbox_root(&self) -> Result<PathBuf, ToolError> {
        let root = match &self.sandbox_root {
            Some(root) => root.clone(),
            None => std::env::current_dir().map_err(|err| {
                ToolError::Internal(format!("failed to determine sandbox root: {err}"))
            })?,
        };

        fs::canonicalize(&root).await.map_err(|err| {
            ToolError::Internal(format!(
                "failed to resolve sandbox root '{}': {err}",
                root.display()
            ))
        })
    }

    async fn resolve_path(&self, root: &Path, raw_path: &str) -> Result<PathBuf, ToolError> {
        let requested = PathBuf::from(raw_path);
        let candidate = if requested.is_absolute() {
            requested
        } else {
            root.join(requested)
        };

        let canonical = fs::canonicalize(&candidate).await.map_err(|err| {
            ToolError::ExecutionFailed(format!("failed to resolve '{raw_path}': {err}"))
        })?;

        if !canonical.starts_with(root) {
            return Err(ToolError::Denied(format!(
                "path escapes sandbox root: {raw_path}"
            )));
        }

        Ok(canonical)
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("read_file", "Read File", "Read file contents")
            .with_category("filesystem")
            .with_metadata("sandboxed", true)
            .with_metadata("max_bytes", self.max_bytes)
            .with_parameters(json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to file to read relative to the workspace sandbox"
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
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("Missing 'path'".into()))?;

        let sandbox_root = self.resolve_sandbox_root().await?;
        let resolved_path = self.resolve_path(&sandbox_root, path).await?;

        let metadata = fs::metadata(&resolved_path).await.map_err(|err| {
            ToolError::ExecutionFailed(format!(
                "failed to inspect '{}': {err}",
                resolved_path.display()
            ))
        })?;

        if !metadata.is_file() {
            return Err(ToolError::ExecutionFailed(format!(
                "path is not a regular file: {}",
                resolved_path.display()
            )));
        }

        if metadata.len() > self.max_bytes {
            let limit_label = if self.max_bytes == DEFAULT_MAX_BYTES {
                DEFAULT_MAX_BYTES_LABEL.to_string()
            } else {
                format!("{} bytes", self.max_bytes)
            };
            return Err(ToolError::ExecutionFailed(format!(
                "file exceeds size limit ({} bytes > {}, limit {}): {}",
                metadata.len(),
                self.max_bytes,
                limit_label,
                resolved_path.display()
            )));
        }

        let contents = fs::read_to_string(&resolved_path).await.map_err(|err| {
            ToolError::ExecutionFailed(format!(
                "failed to read '{}': {err}",
                resolved_path.display()
            ))
        })?;
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

        let tool = ReadFileTool::new().with_sandbox_root(temp_dir.path());
        let fixture = TestFixture::new();
        let result = tool
            .execute(json!({ "path": "note.txt" }), &fixture.ctx())
            .await
            .expect("read succeeds");

        assert_eq!(result.tool_name, "read_file");
        assert_eq!(result.data, json!("hello from read_file"));
    }

    #[tokio::test]
    async fn denies_paths_outside_the_sandbox() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let sandbox = temp_dir.path().join("sandbox");
        let outside = temp_dir.path().join("outside.txt");
        tokio::fs::create_dir_all(&sandbox)
            .await
            .expect("create sandbox dir");
        tokio::fs::write(&outside, "secret")
            .await
            .expect("write outside file");

        let tool = ReadFileTool::new().with_sandbox_root(&sandbox);
        let fixture = TestFixture::new();
        let error = tool
            .execute(json!({ "path": "../outside.txt" }), &fixture.ctx())
            .await
            .expect_err("path traversal should fail");

        match error {
            ToolError::Denied(message) => {
                assert!(message.contains("escapes sandbox"));
            }
            other => panic!("expected denied error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_files_that_exceed_the_size_limit() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = temp_dir.path().join("large.txt");
        tokio::fs::write(&path, "0123456789")
            .await
            .expect("write temp file");

        let tool = ReadFileTool::new()
            .with_sandbox_root(temp_dir.path())
            .with_max_bytes(4);
        let fixture = TestFixture::new();
        let error = tool
            .execute(json!({ "path": "large.txt" }), &fixture.ctx())
            .await
            .expect_err("oversized file should fail");

        match error {
            ToolError::ExecutionFailed(message) => {
                assert!(message.contains("size limit"));
            }
            other => panic!("expected execution failed error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_execution_failed_when_file_is_missing() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let tool = ReadFileTool::new().with_sandbox_root(temp_dir.path());
        let fixture = TestFixture::new();
        let error = tool
            .execute(json!({ "path": "missing.txt" }), &fixture.ctx())
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
