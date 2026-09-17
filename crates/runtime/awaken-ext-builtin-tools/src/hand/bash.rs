//! Persistent shell execution owned by the Hand toolset, with its lifecycle tests.
use super::*;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BASH_OUTPUT_LIMIT: usize = 100 * 1024;
const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;

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
    /// Invocation timeout in milliseconds; zero selects the default timeout.
    #[serde(default)]
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
            session.take();
            if args.command.is_empty() {
                return Ok("bash session restarted".into());
            }
        }
        if args.command.is_empty() {
            return Err(ToolError::InvalidArguments(
                "bash: command is required".into(),
            ));
        }
        if session
            .as_ref()
            .is_some_and(|existing| existing.invalidated.load(Ordering::SeqCst))
        {
            session.take();
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
                session.take();
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

struct BashSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    invalidated: Arc<AtomicBool>,
    process_group: ProcessGroupGuard,
}

impl BashSession {
    fn spawn(context: &HandToolContext) -> Result<Self, ToolError> {
        let mut command = if let Some((program, args)) = &context.bash_launcher {
            let mut command = tokio::process::Command::new(program);
            command.args(args);
            command
        } else {
            persistent_bash_command()
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
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::DefinitiveFailure(format!("spawn /bin/bash: {error}")))?;
        let process_group = ProcessGroupGuard::new(child.id());
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
        let mut cancellation_guard =
            InvocationProcessGuard::new(self.process_group.pid(), invalidated.clone());
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
    fn new(_group: Option<()>, invalidated: Arc<AtomicBool>) -> Self {
        Self { invalidated }
    }

    fn disarm(&mut self) {}
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
        self.invalidated.store(true, Ordering::SeqCst);
    }
}

#[cfg(all(test, not(windows)))]
mod bash_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[test]
    fn bash_completion_requires_the_whole_status_line() {
        // F1 graph: marker/status/newline can each span reads. Every proper
        // prefix is nonterminal; only the full frame yields the exact status.
        // F2 malformed/overflowing status is never converted to a fake -1.
        for code in [0, 1, 127, 255] {
            let frame = format!("output\n__marker__{code}\n");
            for end in 0..frame.len() {
                assert_eq!(
                    bash_completion(&frame.as_bytes()[..end], "__marker__"),
                    None
                );
            }
            assert_eq!(
                bash_completion(frame.as_bytes(), "__marker__"),
                Some((7, code))
            );
        }
        for invalid in ["", "abc", "99999999999999999999999999"] {
            assert_eq!(
                bash_completion(format!("__marker__{invalid}\n").as_bytes(), "__marker__"),
                None
            );
        }
    }

    #[tokio::test]
    async fn closed_stderr_does_not_discard_live_stdout_or_shell_state() {
        // E1 stderr closes before any reply, stdout stays live -> complete
        // output/status, not shell termination. E2 the same shell still owns
        // exported state on the next invocation; no hidden restart occurs.
        let context = HandToolContext::default().with_bash_launcher(
            "/bin/bash",
            [
                "-c".into(),
                "exec 2>&-; exec /bin/bash --noprofile --norc".into(),
            ],
        );
        let tool = BashTool::new(&context);
        for command in [
            "export PIPE_STATE=kept; printf first",
            "printf '%s' \"$PIPE_STATE\"",
        ] {
            let output = tool
                .call(BashArgs {
                    command: command.into(),
                    restart: false,
                    timeout_ms: Some(1000),
                })
                .await
                .unwrap();
            assert_eq!(
                output,
                if command.starts_with("export") {
                    "first"
                } else {
                    "kept"
                }
            );
        }
    }

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

        BashTool::new(&HandToolContext::default())
            .call(BashArgs {
                command: "sleep 0.15".into(),
                restart: false,
                timeout_ms: None,
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
        let tool = BashTool::new(&HandToolContext::new(directory.path()));
        let call = tokio::spawn(async move {
            tool.call(BashArgs {
                command,
                restart: false,
                timeout_ms: None,
            })
            .await
        });
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
    async fn a_blocked_command_write_obeys_cancellation_and_timeout() {
        // W1 launcher is live but has not read stdin; a command larger than the
        // pipe capacity blocks in write_all. W2 cancellation/timeout -> existing
        // process-group guard stops the launcher and descendants before their
        // delayed effect. W3 the same tool accepts a distinct next invocation;
        // the cancelled command is never replayed. Read-phase cancellation and
        // background-job preservation are covered by adjacent existing tests.
        for timeout in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let context = HandToolContext::new(directory.path()).with_bash_launcher(
                "/bin/bash", ["-c".into(), "if [ ! -e ready ]; then printf ready > ready; sleep 0.5; printf leaked > leaked; fi; exec /bin/bash --noprofile --norc".into()],
            );
            let tool = Arc::new(BashTool::new(&context));
            let pending = {
                let tool = tool.clone();
                tokio::spawn(async move {
                    tool.call(BashArgs {
                        command: format!("# {}\ntouch replayed", "x".repeat(2 * 1024 * 1024)),
                        restart: false,
                        timeout_ms: Some(if timeout { 100 } else { 2000 }),
                    })
                    .await
                })
            };
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !directory.path().join("ready").exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if timeout {
                let error = pending.await.unwrap().unwrap_err();
                assert!(error.to_string().contains("timed out after 100ms"));
            } else {
                pending.abort();
                assert!(pending.await.unwrap_err().is_cancelled());
            }
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            assert!(!directory.path().join("leaked").exists());
            assert!(!directory.path().join("replayed").exists());
            let next = tool
                .call(BashArgs {
                    command: "printf next".into(),
                    restart: false,
                    timeout_ms: Some(1000),
                })
                .await
                .unwrap();
            assert_eq!(next, "next");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_bash_call_keeps_background_jobs_until_restart() {
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let leaked = directory.path().join("leaked");
        let tool = BashTool::new(&HandToolContext::new(directory.path()));
        let command = format!(
            "sh -c 'sleep 0.4; printf leaked > {}' & printf done",
            leaked.display()
        );

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tool.call(BashArgs {
                command,
                restart: false,
                timeout_ms: None,
            }),
        )
        .await
        .expect("an unredirected background child must not hold the tool pipe open")
        .expect("foreground shell succeeds");
        assert_eq!(output, "done");

        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            leaked.exists(),
            "persistent Bash must preserve background jobs across calls"
        );
        tool.call(BashArgs {
            command: String::new(),
            restart: true,
            timeout_ms: None,
        })
        .await
        .expect("restart closes the old process group");
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

    #[cfg(unix)]
    fn pid(&self) -> Option<nix::unistd::Pid> {
        self.group
    }

    #[cfg(windows)]
    fn pid(&self) -> Option<()> {
        None
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
