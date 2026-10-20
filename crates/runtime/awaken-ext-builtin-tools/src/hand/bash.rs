// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Persistent shell execution and its adjacent cancellation tests remain one private Hand
// @formal reason: implementation, re-exported through the unchanged public catalog. Product-boundary
// @formal reason: classification, not a formal proof of process termination.
//! Persistent shell execution owned by the Hand toolset, with its lifecycle tests.
use super::*;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(windows)]
mod windows_process;
#[cfg(not(windows))]
use tokio::process::Child as BashChild;
#[cfg(windows)]
use windows_process::Child as BashChild;

const BASH_OUTPUT_LIMIT: usize = 100 * 1024;
const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// The longest one invocation may hold the persistent shell. A model-supplied
/// timeout above this is rejected rather than silently shortened, so the model
/// learns the bound instead of misreading a truncated run.
const BASH_MAX_TIMEOUT_MS: u64 = 600_000;

/// A persistent Bash scoped to one Hand/toolset instance.
pub struct BashTool {
    context: HandToolContext,
    session: tokio::sync::Mutex<Option<BashSession>>,
}

impl BashTool {
    pub fn new(context: &HandToolContext) -> Self {
        Self {
            context: context.clone(),
            session: tokio::sync::Mutex::new(None),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// Shell command to execute.
    #[serde(default)]
    pub command: String,
    /// Restart the runner-side Bash session before executing the command.
    #[serde(default)]
    pub restart: bool,
    /// Invocation timeout in milliseconds (at most 600000); zero selects the
    /// default timeout of 120000.
    #[serde(default)]
    #[schemars(range(max = 600_000))]
    pub timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    type Args = BashArgs;
    type Output = String;
    const ID: &'static str = "bash";
    const DESCRIPTION: &'static str = "Run shell commands for computations, programs, builds and other operations that require command execution. When dedicated file tools are available, use them to read, write, edit or search text instead of shell cat/head/tail, redirection or ad hoc scripts. In particular, load SKILL.md and known text inputs with the text-file reader before running commands that depend on their instructions. Shell execution remains subject to its own permissions; do not use it to bypass a denied file operation.";

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        let mut session = self.session.lock().await;
        if args.restart {
            discard_session(&mut session).await?;
            if args.command.is_empty() {
                return Ok("bash session restarted".into());
            }
        }
        if args.command.is_empty() {
            return Err(ToolError::InvalidArguments(
                "bash: command is required".into(),
            ));
        }
        if let Some(timeout) = args
            .timeout_ms
            .filter(|timeout| *timeout > BASH_MAX_TIMEOUT_MS)
        {
            return Err(ToolError::InvalidArguments(format!(
                "bash: timeout_ms {timeout} exceeds the maximum of {BASH_MAX_TIMEOUT_MS}"
            )));
        }
        if session
            .as_ref()
            .is_some_and(|existing| existing.invalidated.load(Ordering::SeqCst))
        {
            discard_session(&mut session).await?;
        }
        if session.is_none() {
            *session = Some(BashSession::spawn(&self.context)?);
        }
        let timeout_ms = args
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .unwrap_or(BASH_DEFAULT_TIMEOUT_MS);
        let result = session
            .as_mut()
            .expect("Bash session was initialized")
            .exec(&args.command, timeout_ms)
            .await;
        let (output, exit_code) = match result {
            Ok(result) => result,
            Err(error) => {
                discard_session(&mut session).await.map_err(|cleanup| {
                    ToolError::DefinitiveFailure(format!("{error}; {cleanup}"))
                })?;
                return Err(error);
            }
        };
        if exit_code == 0 {
            Ok(output)
        } else {
            Err(ToolError::DefinitiveFailure(if output.is_empty() {
                format!("exit {exit_code}")
            } else {
                format!("exit {exit_code}\n{output}")
            }))
        }
    }
}

async fn discard_session(session: &mut Option<BashSession>) -> Result<(), ToolError> {
    if let Some(mut previous) = session.take() {
        #[cfg(windows)]
        previous.child.shutdown().await.map_err(|error| {
            ToolError::DefinitiveFailure(format!("bash: stop process tree: {error}"))
        })?;
        #[cfg(not(windows))]
        let _ = &mut previous;
    }
    Ok(())
}

struct BashSession {
    child: BashChild,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    invalidated: Arc<AtomicBool>,
    process_group: ProcessGroupGuard,
}

impl BashSession {
    fn spawn(context: &HandToolContext) -> Result<Self, ToolError> {
        let mut command = match &context.bash_launcher {
            Some(Ok((program, args))) => {
                let mut command = tokio::process::Command::new(program);
                if let Some(prepare) = &context.bash_launch_preparation {
                    prepare(&mut command).map_err(|error| {
                        ToolError::DefinitiveFailure(format!("prepare bash launcher: {error}"))
                    })?;
                }
                command.args(args);
                command
            }
            Some(Err(cause)) => return Err(ToolError::DefinitiveFailure(cause.clone())),
            None => persistent_bash_command(),
        };
        command
            .current_dir(&context.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(env) = &context.bash_env {
            command.env_clear().envs(env);
        } else {
            for key in std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| key.to_string_lossy().starts_with("ANTHROPIC_"))
            {
                command.env_remove(key);
            }
        }
        command.env("PS1", "").env("PS2", "").env("TERM", "dumb");
        configure_process_group(&mut command);
        #[cfg(not(windows))]
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::DefinitiveFailure(format!("spawn /bin/bash: {error}")))?;
        #[cfg(windows)]
        let mut child = BashChild::spawn(command)
            .map_err(|error| ToolError::DefinitiveFailure(format!("spawn /bin/bash: {error}")))?;
        #[cfg(not(windows))]
        let process_group = ProcessGroupGuard::new(child.id());
        #[cfg(windows)]
        let process_group = ProcessGroupGuard::new(child.process_tree());
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::DefinitiveFailure("capture /bin/bash stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::DefinitiveFailure("capture /bin/bash stdout".into()))?;
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdin,
            stdout,
            stderr,
            invalidated: Arc::new(AtomicBool::new(false)),
            process_group,
        })
    }

    async fn exec(&mut self, command: &str, timeout_ms: u64) -> Result<(String, i32), ToolError> {
        let sentinel = format!("__ANT_CMD_{}_DONE__", uuid::Uuid::new_v4());
        let split = format!("{}''{}", &sentinel[..8], &sentinel[8..]);
        let wrapped = format!("{{ {command}\n}} </dev/null 2>&1; printf '\\n{split}%d\\n' $?\n");
        // Cover the first cancellable write as well as response reads. A
        // partially submitted command must not survive a cancelled invocation.
        let invalidated = self.invalidated.clone();
        let mut cancellation_guard = InvocationProcessGuard::new(
            self.process_group.cancellation_source(),
            invalidated.clone(),
        );
        let read = async {
            self.stdin
                .write_all(wrapped.as_bytes())
                .await
                .map_err(|error| ToolError::DefinitiveFailure(format!("bash: {error}")))?;
            self.stdin
                .flush()
                .await
                .map_err(|error| ToolError::DefinitiveFailure(format!("bash: {error}")))?;
            let mut collected = Vec::new();
            let mut truncated = false;
            let mut stdout_chunk = [0_u8; 8192];
            let mut stderr_chunk = [0_u8; 8192];
            let mut stdout_open = true;
            loop {
                if !stdout_open && self.stderr.is_none() {
                    // `exit` and inherited `set -e` can end Bash before its
                    // completion marker. Drain both pipes, preserve diagnostics,
                    // and use the actual child status; never replay the command.
                    invalidated.store(true, Ordering::SeqCst);
                    let status = self.child.wait().await.map_err(|error| {
                        ToolError::DefinitiveFailure(format!("bash: wait for shell: {error}"))
                    })?;
                    let output = render_bash_output(&collected, truncated);
                    return match status.code() {
                        Some(code) => Ok((output, code)),
                        None => Err(ToolError::DefinitiveFailure(format!(
                            "bash: bash session terminated by signal\n{output}"
                        ))),
                    };
                }
                let (read, bytes, from_stdout) = if let Some(stderr) = self.stderr.as_mut() {
                    tokio::select! {
                        result = self.stdout.read(&mut stdout_chunk), if stdout_open => {
                            result.map(|read| (read, &stdout_chunk[..read], true))
                        },
                        result = stderr.read(&mut stderr_chunk) => {
                            result.map(|read| (read, &stderr_chunk[..read], false))
                        },
                    }
                } else {
                    self.stdout
                        .read(&mut stdout_chunk)
                        .await
                        .map(|read| (read, &stdout_chunk[..read], true))
                }
                .map_err(|error| ToolError::DefinitiveFailure(format!("bash: {error}")))?;
                if read == 0 {
                    if from_stdout {
                        stdout_open = false;
                    } else {
                        self.stderr.take();
                    }
                    continue;
                }
                collected.extend_from_slice(bytes);
                if collected.len() > BASH_OUTPUT_LIMIT {
                    let drain = collected.len() - BASH_OUTPUT_LIMIT;
                    collected.drain(..drain);
                    truncated = true;
                }
                if let Some((index, exit_code)) = bash_completion(&collected, &sentinel) {
                    return Ok((
                        render_bash_output(&collected[..index], truncated),
                        exit_code,
                    ));
                }
            }
        };
        let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read).await;
        match result {
            Ok(result) => {
                cancellation_guard.disarm();
                result
            }
            Err(_) => {
                invalidated.store(true, Ordering::SeqCst);
                Err(ToolError::DefinitiveFailure(format!(
                    "bash: bash command timed out after {timeout_ms}ms"
                )))
            }
        }
    }
}

fn bash_completion(collected: &[u8], sentinel: &str) -> Option<(usize, i32)> {
    let index = find_bytes(collected, sentinel.as_bytes())?;
    let tail = &collected[index + sentinel.len()..];
    // A pipe read may split anywhere, including inside the decimal status.
    // Read the entire framed line before accepting a terminal outcome.
    let end = tail.iter().position(|byte| *byte == b'\n')?;
    let status = std::str::from_utf8(&tail[..end]).ok()?.parse().ok()?;
    Some((index, status))
}

fn render_bash_output(bytes: &[u8], truncated: bool) -> String {
    let mut output = strip_ansi(&String::from_utf8_lossy(bytes));
    while output.ends_with('\n') {
        output.pop();
    }
    if truncated {
        output.insert_str(0, "[output truncated]\n");
    }
    output
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            if bytes[index] != 0 {
                output.push(bytes[index]);
            }
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

struct InvocationProcessGuard {
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
    #[cfg(windows)]
    tree: Option<windows_process::ProcessTree>,
    invalidated: Arc<AtomicBool>,
}

#[cfg(unix)]
impl InvocationProcessGuard {
    fn new(group: Option<nix::unistd::Pid>, invalidated: Arc<AtomicBool>) -> Self {
        Self { group, invalidated }
    }

    fn disarm(&mut self) {
        self.group.take();
    }
}

#[cfg(windows)]
impl InvocationProcessGuard {
    fn new(tree: windows_process::ProcessTree, invalidated: Arc<AtomicBool>) -> Self {
        Self {
            tree: Some(tree),
            invalidated,
        }
    }

    fn disarm(&mut self) {
        self.tree.take();
    }
}

#[cfg(unix)]
impl Drop for InvocationProcessGuard {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            self.invalidated.store(true, Ordering::SeqCst);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

#[cfg(windows)]
impl Drop for InvocationProcessGuard {
    fn drop(&mut self) {
        if let Some(tree) = self.tree.take() {
            self.invalidated.store(true, Ordering::SeqCst);
            let _ = tree.start_kill();
        }
    }
}

#[cfg(windows)]
fn persistent_bash_command() -> tokio::process::Command {
    let shell = windows_posix_shell();
    let mut process = tokio::process::Command::new(shell.as_deref().unwrap_or("cmd.exe"));
    if shell.is_some() {
        process.args(["--noprofile", "--norc"]);
    } else {
        process.args(["/D", "/Q"]);
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
fn persistent_bash_command() -> tokio::process::Command {
    let mut process = tokio::process::Command::new("/bin/bash");
    process.args(["--noprofile", "--norc"]);
    process
}

fn configure_process_group(command: &mut tokio::process::Command) {
    #[cfg(not(unix))]
    let _ = command;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// Owns descendant termination independently of the invocation future. Unix
/// uses a dedicated process group; Windows retains the actual owned Job Object.
struct ProcessGroupGuard {
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
    #[cfg(windows)]
    tree: windows_process::ProcessTree,
}

impl ProcessGroupGuard {
    #[cfg(not(windows))]
    fn new(pid: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = pid;
        Self {
            #[cfg(unix)]
            group: pid.map(|value| nix::unistd::Pid::from_raw(value as i32)),
        }
    }

    #[cfg(windows)]
    fn new(tree: windows_process::ProcessTree) -> Self {
        Self { tree }
    }

    #[cfg(unix)]
    fn cancellation_source(&self) -> Option<nix::unistd::Pid> {
        self.group
    }

    #[cfg(windows)]
    fn cancellation_source(&self) -> windows_process::ProcessTree {
        self.tree.clone()
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
        #[cfg(windows)]
        let _ = self.tree.start_kill();
    }
}

#[cfg(test)]
mod bash_tests;
