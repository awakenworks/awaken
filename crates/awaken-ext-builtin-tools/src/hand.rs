//! In-process hand tools (ADR-0007). The read-only, safe set — `read`, `glob`,
//! `grep` — runs directly in the runtime process and renders results as text.
//! Their ids match the descriptors in [`crate::builtin_tools`], so a run that
//! makes a descriptor model-visible can register the matching implementation.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use serde::Deserialize;

use crate::erasure::erase;

/// Read a UTF-8 file and return its contents.
pub struct ReadTool;

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
}

#[async_trait]
impl Tool for ReadTool {
    type Args = ReadArgs;
    type Output = String;
    fn id(&self) -> &str {
        "read"
    }
    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        std::fs::read_to_string(&args.path)
            .map_err(|err| ToolError::Execution(format!("read {}: {err}", args.path)))
    }
}

/// List the paths matching a glob pattern, newline-joined.
pub struct GlobTool;

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
}

#[async_trait]
impl Tool for GlobTool {
    type Args = GlobArgs;
    type Output = String;
    fn id(&self) -> &str {
        "glob"
    }
    async fn call(&self, args: GlobArgs) -> Result<String, ToolError> {
        let entries = glob::glob(&args.pattern)
            .map_err(|err| ToolError::InvalidArguments(format!("glob {}: {err}", args.pattern)))?;
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry.map_err(|err| ToolError::Execution(format!("glob walk: {err}")))?;
            paths.push(path.display().to_string());
        }
        Ok(paths.join("\n"))
    }
}

/// Search a file's lines for a regex, returning `path:line:text` for each match.
pub struct GrepTool;

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: String,
}

#[async_trait]
impl Tool for GrepTool {
    type Args = GrepArgs;
    type Output = String;
    fn id(&self) -> &str {
        "grep"
    }
    async fn call(&self, args: GrepArgs) -> Result<String, ToolError> {
        let re = regex::Regex::new(&args.pattern)
            .map_err(|err| ToolError::InvalidArguments(format!("grep pattern: {err}")))?;
        let content = std::fs::read_to_string(&args.path)
            .map_err(|err| ToolError::Execution(format!("read {}: {err}", args.path)))?;
        let mut hits = Vec::new();
        for (index, line) in content.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!("{}:{}:{}", args.path, index + 1, line));
            }
        }
        Ok(hits.join("\n"))
    }
}

/// The read-only, safe hand tools, erased for `Runtime::with_tool` registration.
pub fn executable_hand_tools() -> Vec<Arc<dyn RawTool>> {
    vec![erase(ReadTool), erase(GlobTool), erase(GrepTool)]
}
