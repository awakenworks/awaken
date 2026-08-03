//! In-process hand tools (ADR-0007). `read`, `write`, `edit`, `move`, `delete`,
//! `glob`, `grep`, and `bash` run directly in the runtime process and render results as text.
//! Their ids match the descriptors in [`crate::builtin_tools`], so a run that
//! makes a descriptor model-visible can register the matching implementation.
//! Network fetch and the separately configured search plugin live in [`crate::web`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::erasure::erase_for;

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
        // An empty `old` is a degenerate anchor: `"".matches("")` is 1, so on an
        // empty file the "exactly one occurrence" arm would silently *insert*
        // `new`. Reject it up front so an edit always replaces a real, located
        // substring rather than mutating on a no-op anchor.
        if args.old.is_empty() {
            return Err(ToolError::InvalidArguments(format!(
                "edit {}: `old` must be a non-empty substring to locate",
                args.path
            )));
        }
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

/// Move or rename one file. Directory trees are intentionally unsupported so
/// callers cannot turn a narrowly-scoped file operation into a recursive move.
pub struct MoveTool;

#[derive(Deserialize)]
pub struct MoveArgs {
    pub source: String,
    pub destination: String,
}

#[async_trait]
impl Tool for MoveTool {
    type Args = MoveArgs;
    type Output = String;
    fn id(&self) -> &str {
        "move"
    }
    async fn call(&self, args: MoveArgs) -> Result<String, ToolError> {
        if !std::path::Path::new(&args.source).is_file() {
            return Err(ToolError::Execution(format!(
                "move {}: source is not a file",
                args.source
            )));
        }
        if let Some(parent) = std::path::Path::new(&args.destination).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                ToolError::Execution(format!("move {}: {error}", args.destination))
            })?;
        }
        std::fs::rename(&args.source, &args.destination).map_err(|error| {
            ToolError::Execution(format!(
                "move {} to {}: {error}",
                args.source, args.destination
            ))
        })?;
        Ok(format!("moved {} to {}", args.source, args.destination))
    }
}

/// Delete exactly one regular file. Directories are rejected; recursive deletion
/// remains outside the model-callable capability surface.
pub struct DeleteTool;

#[derive(Deserialize)]
pub struct DeleteArgs {
    pub path: String,
}

#[async_trait]
impl Tool for DeleteTool {
    type Args = DeleteArgs;
    type Output = String;
    fn id(&self) -> &str {
        "delete"
    }
    async fn call(&self, args: DeleteArgs) -> Result<String, ToolError> {
        if !std::path::Path::new(&args.path).is_file() {
            return Err(ToolError::Execution(format!(
                "delete {}: path is not a file",
                args.path
            )));
        }
        std::fs::remove_file(&args.path)
            .map_err(|error| ToolError::Execution(format!("delete {}: {error}", args.path)))?;
        Ok(format!("deleted {}", args.path))
    }
}

/// Run a command via the platform shell and return its output. A non-zero exit
/// is a model-visible error result carrying stdout/stderr, not a run abort.
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
        // The async child wait yields to authority heartbeats and, unlike a
        // `spawn_blocking(Command::output)` task, remains cancellation-safe. Each
        // shell leads a private process group: finishing or dropping this tool call
        // kills the entire group, so neither a successful `cmd &` nor WorkUnit
        // cancellation can orphan servers, approval prompts, compilers, or other
        // descendants under the Worker service. A background service that must live
        // for several checks belongs inside one bounded shell call with a trap.
        let mut command = platform_shell_command(&args.command);
        let shell = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        configure_process_group(&mut command);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|err| ToolError::Execution(format!("spawn {shell}: {err}")))?;
        let process_group = ProcessGroupGuard::new(child.id());
        let mut child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution(format!("capture {shell} stdout")))?;
        let mut child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution(format!("capture {shell} stderr")))?;
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            child_stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            child_stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let status = child
            .wait()
            .await
            .map_err(|err| ToolError::Execution(format!("wait for {shell}: {err}")))?;
        // `wait_with_output` waits for pipe EOF as well as the foreground shell.
        // An unredirected `server &` keeps both pipes open indefinitely even
        // after that shell has exited. Reap the private process group as soon as
        // the foreground status is known; only then can the readers observe EOF.
        drop(process_group);
        let stdout = stdout_reader
            .await
            .map_err(|err| ToolError::Execution(format!("join {shell} stdout reader: {err}")))?
            .map_err(|err| ToolError::Execution(format!("read {shell} stdout: {err}")))?;
        let stderr = stderr_reader
            .await
            .map_err(|err| ToolError::Execution(format!("join {shell} stderr reader: {err}")))?
            .map_err(|err| ToolError::Execution(format!("read {shell} stderr: {err}")))?;
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        if status.success() {
            Ok(stdout.into_owned())
        } else {
            let code = status
                .code()
                .map_or_else(|| "signal".to_string(), |c| c.to_string());
            Err(ToolError::Execution(format!(
                "command exited {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )))
        }
    }
}

#[cfg(all(test, not(windows)))]
mod bash_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn bash_wait_does_not_starve_runtime_control_tasks() {
        // A long local command must yield the sole async runtime thread. Worker
        // heartbeat and dispatch renewal use the same scheduling path in the
        // served runtime, so blocking here previously expired both authorities.
        let control_task_ran = Arc::new(AtomicBool::new(false));
        let observed = control_task_ran.clone();
        let control = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            observed.store(true, Ordering::SeqCst);
        });

        BashTool
            .call(BashArgs {
                command: "sleep 0.15".into(),
            })
            .await
            .expect("sleep command");

        assert!(control_task_ran.load(Ordering::SeqCst));
        control.await.expect("control task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_bash_call_kills_its_descendant_process_group() {
        // Cancellation previously dropped only the blocking-task join handle,
        // leaving the shell and descendants alive beneath the Worker service.
        // A descendant reports readiness, would write `leaked` after cancellation,
        // and is required to disappear with its process group instead.
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let ready = directory.path().join("ready");
        let leaked = directory.path().join("leaked");
        let command = format!(
            "sh -c 'printf ready > {}; sleep 0.4; printf leaked > {}' & wait",
            ready.display(),
            leaked.display()
        );
        let call = tokio::spawn(BashTool.call(BashArgs { command }));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !ready.is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant becomes ready");

        call.abort();
        let _ = call.await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !leaked.exists(),
            "a descendant survived cancellation and wrote {}",
            leaked.display()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_bash_call_kills_background_descendants() {
        // A successful shell used to disarm the process-group guard. `server &`
        // therefore escaped the tool boundary and accumulated in a reusable K8s
        // Session even though the Agent had already returned an approved verdict.
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let leaked = directory.path().join("leaked");
        let command = format!(
            "sh -c 'sleep 0.4; printf leaked > {}' & printf done",
            leaked.display()
        );

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            BashTool.call(BashArgs { command }),
        )
        .await
        .expect("an unredirected background child must not hold the tool pipe open")
        .expect("foreground shell succeeds");
        assert_eq!(output, "done");

        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !leaked.exists(),
            "a successful bash tool call must reap its background process group"
        );
    }
}

#[cfg(windows)]
fn platform_shell_command(command: &str) -> tokio::process::Command {
    let shell = windows_posix_shell();
    let mut process = tokio::process::Command::new(shell.as_deref().unwrap_or("cmd.exe"));
    if shell.is_some() {
        process.args(["-c", command]);
    } else {
        process.args(["/D", "/S", "/C", command]);
    }
    process
}

#[cfg(windows)]
fn windows_posix_shell() -> Option<String> {
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let shell = directory.join("sh.exe");
        if shell.is_file() {
            return Some(shell.to_string_lossy().into_owned());
        }
        let git = directory.join("git.exe");
        if git.is_file() && directory.file_name().is_some_and(|name| name == "cmd") {
            let shell = directory.parent()?.join("bin").join("sh.exe");
            if shell.is_file() {
                return Some(shell.to_string_lossy().into_owned());
            }
        }
    }
    std::env::var_os("ProgramFiles")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("Git").join("bin").join("sh.exe"))
        .filter(|shell| shell.is_file())
        .map(|shell| shell.to_string_lossy().into_owned())
}

#[cfg(not(windows))]
fn platform_shell_command(command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("sh");
    process.args(["-c", command]);
    process
}

fn configure_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// Synchronously terminates a Unix process group when a tool future finishes or
/// is dropped. Tokio's `kill_on_drop` covers the direct child on every platform;
/// this guard extends that guarantee to background descendants on Unix.
struct ProcessGroupGuard {
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            group: pid.map(|value| nix::unistd::Pid::from_raw(value as i32)),
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// The local hand tools, erased for `Runtime::with_tool` registration. The
/// network tool `web_fetch` is added by `web_hand_tools`; `web_search` is owned
/// exclusively by the separately configured plugin path.
pub fn executable_hand_tools() -> Vec<Arc<dyn RawTool>> {
    vec![
        erase_for(ReadTool, ToolExecutionTarget::Sandbox),
        erase_for(WriteTool, ToolExecutionTarget::Sandbox),
        erase_for(EditTool, ToolExecutionTarget::Sandbox),
        erase_for(MoveTool, ToolExecutionTarget::Sandbox),
        erase_for(DeleteTool, ToolExecutionTarget::Sandbox),
        erase_for(GlobTool, ToolExecutionTarget::Sandbox),
        erase_for(GrepTool, ToolExecutionTarget::Sandbox),
        erase_for(BashTool, ToolExecutionTarget::Sandbox),
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

    #[tokio::test]
    async fn move_and_delete_are_single_file_operations() {
        // Causes: M1 a regular source file and nested destination; M2 a regular
        // destination file; M3 a directory passed to delete.
        // Constraints: move/delete operate on one file and never recurse.
        // Effects: M1 preserves bytes at the new path, M2 removes that file, and
        // M3 fails without changing the directory tree.
        // Decision rules: M1 move success; M2 delete success; M3 directory reject.
        let base = std::env::temp_dir().join(format!("awaken-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let source = base.join("source.md");
        let destination = base.join("nested/destination.md");
        std::fs::write(&source, "durable").unwrap();
        MoveTool
            .call(MoveArgs {
                source: source.to_string_lossy().into_owned(),
                destination: destination.to_string_lossy().into_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "durable",
            "M1"
        );
        DeleteTool
            .call(DeleteArgs {
                path: destination.to_string_lossy().into_owned(),
            })
            .await
            .unwrap();
        assert!(!destination.exists(), "M2");
        assert!(
            DeleteTool
                .call(DeleteArgs {
                    path: base.to_string_lossy().into_owned(),
                })
                .await
                .is_err(),
            "M3"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
