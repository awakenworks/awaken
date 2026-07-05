//! In-process hand tools (ADR-0007). `read`, `write`, `edit`, `glob`, `grep`,
//! and `bash` run directly in the runtime process and render results as text.
//! Their ids match the descriptors in [`crate::builtin_tools`], so a run that
//! makes a descriptor model-visible can register the matching implementation.
//! The network tools `web_fetch`/`web_search` live in [`crate::web`].

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

/// Write `content` to a file, creating or truncating it. Returns a confirmation.
pub struct WriteTool;

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[async_trait]
impl Tool for WriteTool {
    type Args = WriteArgs;
    type Output = String;
    fn id(&self) -> &str {
        "write"
    }
    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        // Create parent directories so a write to a nested path (e.g. `outputs/x.txt`)
        // succeeds without a prior mkdir — matching editor/`write`-tool expectations.
        if let Some(parent) = std::path::Path::new(&args.path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| ToolError::Execution(format!("write {}: {err}", args.path)))?;
        }
        std::fs::write(&args.path, &args.content)
            .map_err(|err| ToolError::Execution(format!("write {}: {err}", args.path)))?;
        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            args.path
        ))
    }
}

/// Replace one exact occurrence of `old` with `new` in a file. Fails closed when
/// `old` is absent or ambiguous, so an edit never silently changes the wrong
/// span.
pub struct EditTool;

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old: String,
    pub new: String,
}

#[async_trait]
impl Tool for EditTool {
    type Args = EditArgs;
    type Output = String;
    fn id(&self) -> &str {
        "edit"
    }
    async fn call(&self, args: EditArgs) -> Result<String, ToolError> {
        let content = std::fs::read_to_string(&args.path)
            .map_err(|err| ToolError::Execution(format!("read {}: {err}", args.path)))?;
        let matches = content.matches(&args.old).count();
        match matches {
            0 => Err(ToolError::Execution(format!(
                "edit {}: `old` text not found",
                args.path
            ))),
            1 => {
                let updated = content.replacen(&args.old, &args.new, 1);
                std::fs::write(&args.path, &updated)
                    .map_err(|err| ToolError::Execution(format!("write {}: {err}", args.path)))?;
                Ok(format!("edited {}", args.path))
            }
            n => Err(ToolError::Execution(format!(
                "edit {}: `old` text is ambiguous ({n} occurrences); add context to make it unique",
                args.path
            ))),
        }
    }
}

/// Run a shell command via `sh -c` and return its output. A non-zero exit is a
/// model-visible error result carrying stdout/stderr, not a run abort.
pub struct BashTool;

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
}

#[async_trait]
impl Tool for BashTool {
    type Args = BashArgs;
    type Output = String;
    fn id(&self) -> &str {
        "bash"
    }
    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .output()
            .map_err(|err| ToolError::Execution(format!("spawn sh: {err}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            Ok(stdout.into_owned())
        } else {
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            Err(ToolError::Execution(format!(
                "command exited {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )))
        }
    }
}

/// The local hand tools, erased for `Runtime::with_tool` registration. The
/// network tools `web_fetch` and `web_search` are added by `web_hand_tools`.
pub fn executable_hand_tools() -> Vec<Arc<dyn RawTool>> {
    vec![
        erase(ReadTool),
        erase(WriteTool),
        erase(EditTool),
        erase(GlobTool),
        erase(GrepTool),
        erase(BashTool),
    ]
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[tokio::test]
    async fn write_creates_missing_parent_directories() {
        // A write to a nested path (e.g. `outputs/x.txt`) must succeed without a prior
        // mkdir — the artifact-write path (ADR-0038) relies on this.
        let base = std::env::temp_dir().join(format!("awaken-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join("outputs/deep/result.txt");
        let out = WriteTool
            .call(WriteArgs {
                path: path.to_string_lossy().into_owned(),
                content: "artifact-bytes".into(),
            })
            .await
            .expect("write into a missing dir tree");
        assert!(out.contains("wrote"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "artifact-bytes");
        let _ = std::fs::remove_dir_all(&base);
    }
}
