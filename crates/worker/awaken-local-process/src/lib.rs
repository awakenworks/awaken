// @formal entrypoint sandbox.local_process_spawn_policy_and_waiter_lock_ownership_are_preserved
//! Process waiting policy plus the local child-process lifecycle adapter.
//! Local/Namespace and container exec handles share cancellation-safe waiting;
//! trusted subprocess and Local/Namespace launches share the OS child adapter.
//!
//! Protocol supervisors own *when* to terminate. This adapter owns the
//! OS-specific termination boundary: Unix targets the private process group;
//! Windows owns a kill-on-close Job; other platforms target the direct child.

use async_trait::async_trait;
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
#[cfg(not(windows))]
use tokio::process::Child;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// Await a process through its provider-owned, nonblocking status port.
/// Poll remains the sole exit observer: dropping a waiter neither consumes a
/// completion handle nor holds a lifecycle lock needed by another poll/signal.
/// Status errors remain unknown; this adapter never synthesizes physical exit.
pub async fn wait_for_process_exit(
    process: &dyn ProcessHandle,
) -> Result<ExitStatus, SandboxError> {
    loop {
        if let Some(status) = process.poll().await? {
            return Ok(status);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[cfg(target_os = "linux")]
pub mod scoped;

#[cfg(target_os = "linux")]
pub mod seccomp;

/// How many BPF programs the workload syscall profile compiles to (an `EPERM`
/// table and a `clone3` → `ENOSYS` table), i.e. how many `--add-seccomp-fd`
/// options a bubblewrap launch carries. Declared on every platform so pure
/// launcher renderers stay target-independent; only Linux compiles programs.
pub const SECCOMP_PROGRAM_COUNT: usize = 2;

/// The descriptors a bubblewrap launch receives the programs on. fd 3 is
/// bubblewrap's private info report, so the programs start at fd 4.
pub const BUBBLEWRAP_SECCOMP_FDS: [i32; SECCOMP_PROGRAM_COUNT] = [4, 5];

/// The bubblewrap options installing the programs mapped at
/// [`BUBBLEWRAP_SECCOMP_FDS`], in installation order.
#[must_use]
pub fn bubblewrap_seccomp_args() -> Vec<String> {
    BUBBLEWRAP_SECCOMP_FDS
        .iter()
        .flat_map(|fd| ["--add-seccomp-fd".to_owned(), fd.to_string()])
        .collect()
}

pub mod podman;

mod execution_identity;
#[cfg(windows)]
mod windows_job;
pub use execution_identity::{ProcessExecutionIdentity, ProcessNamespaceIdentity};

/// One process-lifetime profile shared by Namespace and host-view rendering.
/// Filesystem and network policy remain the sandbox provider's responsibility.
pub const BUBBLEWRAP_PROCESS_BOUNDARY: [&str; 5] = [
    "--unshare-user",
    "--disable-userns",
    "--unshare-pid",
    "--die-with-parent",
    "--new-session",
];

/// Resolve a trusted launcher (`bwrap`, `sandbox-exec`) from the operator's
/// `PATH` — the Worker's own environment, never the environment a workload or
/// Session configuration supplies to the child. Only absolute directories are
/// searched, the match must be an executable regular file (`.exe` only on
/// Windows), and the result is canonicalized so a later launch execs exactly
/// the file that was found.
#[must_use]
pub fn resolve_operator_executable(
    name: &str,
    operator_path: &std::ffi::OsStr,
) -> Option<std::path::PathBuf> {
    let candidate = std::path::Path::new(name);
    if candidate.is_absolute() {
        return is_executable_file(candidate)
            .then(|| std::fs::canonicalize(candidate).ok())
            .flatten()
            .filter(|canonical| is_executable_file(canonical));
    }
    let mut components = candidate.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return None;
    }
    #[cfg(windows)]
    let name = {
        let extension = candidate.extension().and_then(std::ffi::OsStr::to_str);
        if extension.is_some_and(|extension| !extension.eq_ignore_ascii_case("exe")) {
            return None;
        }
        if extension.is_none() {
            let mut executable = candidate.to_path_buf();
            executable.set_extension("exe");
            executable
        } else {
            candidate.to_path_buf()
        }
    };
    #[cfg(not(windows))]
    let name = candidate.to_path_buf();
    std::env::split_paths(operator_path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(&name))
        .filter(|path| is_executable_file(path))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .find(|canonical| is_executable_file(canonical))
}

/// [`resolve_operator_executable`] against this process's own `PATH`.
#[expect(
    clippy::disallowed_methods,
    reason = "the local process provider looks up operator executables and the rootless systemd bus in the host environment"
)]
pub fn resolve_operator_launcher(name: &str) -> Result<std::path::PathBuf, SandboxError> {
    resolve_operator_executable(name, &std::env::var_os("PATH").unwrap_or_default()).ok_or_else(
        || {
            SandboxError::unsupported(format!(
                "sandbox launcher `{name}` is not an executable on the Worker's absolute PATH"
            ))
        },
    )
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        metadata.is_file()
            && path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.is_file()
    }
}

/// Configure a command so the child becomes leader of a private process group.
fn configure_process_group(command: &mut Command) {
    #[cfg(not(unix))]
    let _ = command;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// A locally owned process with serialized child access, never a lock held
/// while waiting for the OS to exit. Unix signals target its private group.
pub struct LocalProcess {
    id: String,
    #[cfg(unix)]
    process_group: Option<nix::unistd::Pid>,
    #[cfg(unix)]
    graceful_group: Option<nix::unistd::Pid>,
    #[cfg(not(windows))]
    child: Mutex<Child>,
    #[cfg(windows)]
    child: Mutex<windows_job::OwnedChild>,
}

impl LocalProcess {
    /// Launch through the sole local ProcessHandle construction boundary.
    /// Callers configure argv, cwd, environment and pipes, but cannot bypass or
    /// subsequently overwrite the private process-group and drop policy.
    ///
    /// Windows assigns the suspended child to a private kill-on-close Job before
    /// any workload executes. Unix Drop requests direct-child termination; it
    /// is not an owner-crash guarantee, since SIGKILL does not run Drop.
    pub fn spawn(command: &mut Command) -> std::io::Result<Self> {
        configure_process_group(command);
        command.kill_on_drop(true);
        #[cfg(windows)]
        let child = windows_job::spawn(command)?;
        #[cfg(not(windows))]
        let child = command.spawn()?;
        let pid = child.id();
        Ok(Self {
            id: pid.map(|value| value.to_string()).unwrap_or_default(),
            #[cfg(unix)]
            process_group: pid.map(|value| nix::unistd::Pid::from_raw(value as i32)),
            #[cfg(unix)]
            graceful_group: None,
            child: Mutex::new(child),
        })
    }

    /// Detach a configured pipe before sharing the lifecycle handle. There is
    /// deliberately no raw Child accessor or constructor: ownership stays here.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        #[cfg(windows)]
        {
            self.child.get_mut().stdin().take()
        }
        #[cfg(not(windows))]
        {
            self.child.get_mut().stdin.take()
        }
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        #[cfg(windows)]
        {
            self.child.get_mut().stdout().take()
        }
        #[cfg(not(windows))]
        {
            self.child.get_mut().stdout.take()
        }
    }
    /// Bind the private session group reported by an owned isolating launcher.
    /// The caller must obtain this identity from that launcher's private control
    /// descriptor, never from task input or a scan of unrelated host processes.
    /// TERM/INT then reach the inner group without killing its monitor; KILL
    /// retains the original outer-group teardown. Wait for setsid before exposing
    /// the handle, since launchers can report the child before isolation finishes.
    #[cfg(unix)]
    pub async fn bind_private_signal_group(&mut self, group: u32) -> Result<(), SandboxError> {
        use nix::unistd::{Pid, getpgid};
        use std::time::Duration;
        let group = i32::try_from(group)
            .ok()
            .filter(|group| *group > 1)
            .ok_or_else(|| {
                SandboxError::backend("bind_private_signal_group", "invalid private signal group")
            })?;
        let group = Pid::from_raw(group);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.poll().await?.is_some() {
                    return Ok(());
                }
                match getpgid(Some(group)) {
                    Ok(actual) if actual == group => {
                        self.graceful_group = Some(group);
                        return Ok(());
                    }
                    Ok(_) | Err(nix::errno::Errno::ESRCH) => {}
                    Err(error) => {
                        return Err(SandboxError::backend("bind_private_signal_group", error));
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .map_err(|_| SandboxError::timeout("private signal group setup timed out"))?
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        #[cfg(windows)]
        {
            self.child.get_mut().stderr().take()
        }
        #[cfg(not(windows))]
        {
            self.child.get_mut().stderr.take()
        }
    }
}

fn exit(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[cfg(unix)]
fn group_signal_result(result: Result<(), nix::errno::Errno>) -> Result<(), SandboxError> {
    match result {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        // EPERM (including an ambiguous macOS response after natural exit) is
        // not proof that the group is absent. A reaped leader says nothing
        // about surviving descendants. Preserve every error except exact ESRCH.
        Err(error) => Err(SandboxError::backend(
            "group_signal_result",
            format!("process-group signal failed: {error}"),
        )),
    }
}

#[async_trait]
impl ProcessHandle for LocalProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        wait_for_process_exit(self).await
    }

    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
        let mut child = self.child.lock().await;
        let status = child
            .try_wait()
            .map_err(|error| SandboxError::backend("poll", error))?;
        Ok(status.map(exit))
    }

    async fn signal(&self, signal: Signal) -> Result<(), SandboxError> {
        #[cfg(unix)]
        {
            let target = if matches!(signal, Signal::Kill) {
                self.process_group
            } else {
                self.graceful_group.or(self.process_group)
            };
            let Some(group) = target else {
                return Ok(());
            };
            let signal = match signal {
                Signal::Term => nix::sys::signal::Signal::SIGTERM,
                Signal::Kill => nix::sys::signal::Signal::SIGKILL,
                Signal::Int => nix::sys::signal::Signal::SIGINT,
            };
            return group_signal_result(nix::sys::signal::killpg(group, signal));
        }
        #[cfg(windows)]
        {
            let _ = signal;
            // Windows has no POSIX TERM here; preserve forceful cancellation,
            // extending its scope to the exact owned Job rather than one child.
            self.child
                .lock()
                .await
                .start_kill()
                .map_err(|error| SandboxError::backend("signal_job", error))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = signal;
            self.child
                .lock()
                .await
                .start_kill()
                .map_err(|error| SandboxError::backend("signal", error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;

    /// Operator launcher resolution. Causes: C1 absolute search directory with
    /// an executable regular file; C2 relative search directory; C3 matching
    /// name that is not executable, or a directory; C4 a name with a path
    /// separator; C5 an absolute name. Effects: C1 => the canonical file (a
    /// symlink resolves to its target); C2/C3 => skipped, search continues;
    /// C4 => refused; C5 => itself when executable. Nothing else is searched.
    #[cfg(unix)]
    #[test]
    fn operator_launcher_is_an_absolute_canonical_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let (plain, executable, linked) = (
            root.path().join("plain"),
            root.path().join("exec"),
            root.path().join("linked"),
        );
        for dir in [&plain, &executable, &linked] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(plain.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::create_dir(plain.join("dir-tool")).unwrap();
        let target = executable.join("tool");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, linked.join("tool")).unwrap();
        let canonical = std::fs::canonicalize(&target).unwrap();
        let search = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();

        let path = search(&[Path::new("relative"), &plain, &executable]);
        assert_eq!(
            resolve_operator_executable("tool", &path),
            Some(canonical.clone()),
            "C1-C3"
        );
        let path = search(&[&linked]);
        assert_eq!(
            resolve_operator_executable("tool", &path),
            Some(canonical.clone()),
            "C1 link"
        );
        assert_eq!(
            resolve_operator_executable("dir-tool", &search(&[&plain])),
            None,
            "C3"
        );
        assert_eq!(
            resolve_operator_executable("exec/tool", &search(&[root.path()])),
            None,
            "C4"
        );
        assert_eq!(
            resolve_operator_executable(target.to_str().unwrap(), std::ffi::OsStr::new("")),
            Some(canonical),
            "C5"
        );
        assert_eq!(
            resolve_operator_executable(plain.join("tool").to_str().unwrap(), &path),
            None,
            "C5 not executable"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_operator_launcher_resolves_only_a_canonical_exe() {
        // Causes: C1 absolute PATH directory with tool.exe/only a script;
        // C2 relative PATH directory; C3 bare/cmd/drive-relative/absolute
        // launcher name. R1 bare tool resolves the exact canonical .exe;
        // R2 scripts and relative search roots are skipped; R3 non-exe and
        // drive-relative names are refused before any process is launched.
        let temp = tempfile::tempdir().unwrap();
        let scripts = temp.path().join("scripts");
        let binaries = temp.path().join("binaries");
        std::fs::create_dir(&scripts).unwrap();
        std::fs::create_dir(&binaries).unwrap();
        std::fs::write(scripts.join("tool.cmd"), b"echo hostile\r\n").unwrap();
        std::fs::write(scripts.join("tool"), b"untrusted extensionless").unwrap();
        let executable = binaries.join("tool.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let expected = std::fs::canonicalize(&executable).unwrap();
        let path =
            std::env::join_paths([std::path::Path::new("relative"), &scripts, &binaries]).unwrap();
        assert_eq!(
            resolve_operator_executable("tool", &path),
            Some(expected.clone()),
            "R1/R2"
        );
        assert_eq!(
            resolve_operator_executable("tool.exe", &path),
            Some(expected),
            "R1"
        );
        for invalid in ["tool.cmd", "C:tool", "nested\\tool"] {
            assert_eq!(
                resolve_operator_executable(invalid, &path),
                None,
                "R3 {invalid}"
            );
        }
        assert_eq!(
            resolve_operator_executable(scripts.join("tool.cmd").to_str().unwrap(), &path),
            None,
            "R3 absolute script"
        );
    }

    #[cfg(unix)]
    async fn wait_for_marker(path: &Path, expected: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if std::fs::read_to_string(path).is_ok_and(|value| value == expected) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("marker {} becomes {expected:?}", path.display()));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn term_reaches_the_spawned_process_and_its_descendant() {
        // Cause/effect graph and decision rule PG1:
        // C1 a command is configured before spawn; C2 it creates a descendant;
        // C3 the descendant reports its TERM trap is installed; C4 ProcessHandle
        // receives TERM. C1+C2+C3+C4 => E1 the leader exits and E2 the
        // descendant's TERM trap completes. Waiting for marker contents, rather
        // than file existence, covers the redirection-before-write shell rule.
        // Signal-kind mapping is exhaustive in `signal`; this ordinary-process
        // test does not qualify detached descendants or crash containment.
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let ready = directory.path().join("descendant-ready");
        let marker = directory.path().join("descendant-terminated");
        let script = format!(
            "sh -c 'trap \"printf child > {} ; exit 0\" TERM; printf ready > {}; while :; do sleep 1; done' & wait",
            marker.display(),
            ready.display()
        );
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let process = LocalProcess::spawn(&mut command).expect("spawn process group");

        wait_for_marker(&ready, "ready").await;
        process.signal(Signal::Term).await.expect("signal group");
        tokio::time::timeout(Duration::from_secs(2), process.wait())
            .await
            .expect("leader settles")
            .expect("wait succeeds");
        wait_for_marker(&marker, "child").await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn waiting_keeps_local_poll_and_signal_available_after_cancellation() {
        // L1 a private local child waits on its owned stdin; L2 wait is actually
        // polled before a concurrent poll; L3 that waiter is cancelled; L4 a new
        // waiter and TERM race. Effects: poll returns None without waiting for
        // exit, cancellation preserves the child, TERM ends it, both subsequent
        // observers retain the exact OS status. Local/Namespace/trusted ACP all
        // reuse this adapter, without another monitor or exit-status registry.
        let mut command = Command::new("sh");
        command
            .args(["-c", "read input"])
            .stdin(Stdio::piped())
            .kill_on_drop(true);
        let mut process = LocalProcess::spawn(&mut command).unwrap();
        let _input = process.take_stdin().unwrap();
        let mut waiting = Box::pin(process.wait());
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending(), "L2 wait entered");
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), process.poll())
                .await
                .expect("L2 poll does not wait for child exit")
                .unwrap(),
            None
        );
        drop(waiting);
        assert_eq!(process.poll().await.unwrap(), None, "L3 still observable");
        let (status, signal) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(process.wait(), process.signal(Signal::Term))
        })
        .await
        .expect("L4 wait/signal make progress");
        signal.unwrap();
        let status = status.unwrap();
        assert!(status.signaled, "L4 actual OS signal exit");
        assert_eq!(process.poll().await.unwrap(), Some(status.clone()));
        assert_eq!(process.wait().await.unwrap(), status);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn signal_accepts_a_process_group_that_already_exited() {
        // G1 natural exit before signal => exact ESRCH is an idempotent success.
        // G2 macOS may report ambiguous EPERM instead: retain that error even
        // though the leader exited. The exhaustive errno table below pins G2
        // on Linux as well; this integration row observes the executing kernel.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let process = LocalProcess::spawn(&mut command).expect("spawn short-lived process");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if process.poll().await.expect("poll child").is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child exits");

        let result = process.signal(Signal::Term).await;
        #[cfg(target_os = "macos")]
        if let Err(error) = &result {
            assert!(error.to_string().contains("EPERM"), "G2: {error}");
            return;
        }
        result.expect("G1 an absent group is an idempotent signal outcome");
    }

    #[test]
    #[cfg(unix)]
    fn group_signal_errors_require_exact_absence_evidence() {
        // Decision table G3: delivery accepted => Ok; G4 ESRCH => Ok;
        // G5 EPERM/EACCES/EINVAL/other failure => Err. Enumerate errno codes
        // 0..4095, including nix's unknown-error class. Leader exit is NOT an
        // input: poll/wait before or after the OS result cannot waive G5.
        // Success is signal acceptance/absence only, never tree quiescence.
        assert!(group_signal_result(Ok(())).is_ok());
        for code in 0..=4095 {
            let errno = nix::errno::Errno::from_raw(code);
            let result = group_signal_result(Err(errno));
            assert_eq!(
                result.is_ok(),
                errno == nix::errno::Errno::ESRCH,
                "G4/G5 {errno}"
            );
            if let Err(error) = result {
                assert!(error.to_string().contains(&errno.to_string()));
            }
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reaped_leader_does_not_prevent_signalling_a_live_descendant() {
        // Timing rule G6: private group -> fork -> child-ready barrier ->
        // reaped leader -> group KILL => descendant-held stdout reaches EOF.
        // The pipe and live group probe establish survival before signalling;
        // the 10s backstop exceeds the 2s oracle and bounds leaks on assertion
        // failure. No PID scan or leader-exit-as-tree-exit assumption is used.
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sh -c 'printf \"AWAKEN_DESCENDANT_READY\\n\"; exec sleep 10' & exit 0",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = LocalProcess::spawn(&mut command).unwrap();
        let mut output = BufReader::new(process.take_stdout().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "AWAKEN_DESCENDANT_READY\n");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), process.wait())
                .await
                .unwrap()
                .unwrap()
                .code,
            Some(0)
        );
        nix::sys::signal::killpg(process.process_group.unwrap(), None)
            .expect("G6 group still live");
        process.signal(Signal::Kill).await.unwrap();
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), output.read_to_end(&mut remaining))
            .await
            .expect("G6 descendant exits before its voluntary backstop")
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    #[ignore = "child participant, invoked by the cross-platform lifecycle matrix"]
    fn lifecycle_child() {
        if std::env::var("AWAKEN_LIFECYCLE_CHILD").as_deref() == Ok("io") {
            use std::io::{Read, Write};
            assert_eq!(
                std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
                std::fs::canonicalize(std::env::var_os("AWAKEN_EXPECTED_CWD").unwrap()).unwrap()
            );
            assert_eq!(
                std::env::var("AWAKEN_PROJECTED_VALUE").unwrap(),
                "空 格 $ literal"
            );
            let mut input = [0_u8; 1];
            std::io::stdin().read_exact(&mut input).unwrap();
            assert_eq!(input, [42]);
            // libtest may print its progress prefix without a newline when
            // RUST_TEST_THREADS=1. Keep the private acknowledgement on its own
            // complete line; the parent still requires exact marker equality.
            println!("\nAWAKEN_CHILD_READY");
            std::io::stdout().flush().unwrap();
            // Parent deliberately keeps this pipe open during the Drop oracle.
            // If that oracle fails, unwinding closes stdin and releases the
            // participant instead of leaking an immortal test process.
            std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
        }
        if std::env::var("AWAKEN_LIFECYCLE_CHILD").as_deref() == Ok("wait") {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    #[tokio::test]
    async fn sole_spawn_preserves_projection_and_drop_terminates_the_owned_child() {
        // Cause/effect decision table:
        // S1 valid executable + cwd with spaces/Unicode + literal projected env
        //    + piped IO => exact child observations and two one-shot pipe takes.
        // S2 caller explicitly disables kill_on_drop + S1 + owner Drop while
        //    the detached stdin is STILL open => child stdout reaches EOF.
        // S3 missing executable => spawn error, no lifecycle handle fabricated.
        // The child acknowledges projection before S2; it cannot voluntarily
        // exit while stdin is held open. EOF detects an orphan regression without
        // a PID scan, shell, credentials, or timing-based survival assertion.
        // This proves graceful owner Drop only, NOT Worker SIGKILL or descendants.
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work 空 格");
        std::fs::create_dir(&cwd).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::lifecycle_child",
                "--ignored",
                "--nocapture",
            ])
            .current_dir(&cwd)
            .env("AWAKEN_LIFECYCLE_CHILD", "io")
            .env("AWAKEN_EXPECTED_CWD", &cwd)
            .env("AWAKEN_PROJECTED_VALUE", "空 格 $ literal")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        let mut process = LocalProcess::spawn(&mut command).unwrap();
        let mut stdin = process.take_stdin().unwrap();
        let mut stdout = BufReader::new(process.take_stdout().unwrap());
        assert!(process.take_stdin().is_none());
        assert!(process.take_stdout().is_none());
        stdin.write_all(&[42]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    stdout.read_line(&mut line).await.unwrap(),
                    0,
                    "child acknowledges projection"
                );
                if line.trim() == "AWAKEN_CHILD_READY" {
                    break;
                }
            }
        })
        .await
        .expect("child startup barrier");
        drop(process);
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut tail))
            .await
            .expect("Drop terminates child even with its stdin held open")
            .unwrap();
        drop(stdin);
        assert!(
            LocalProcess::spawn(&mut Command::new(root.path().join("missing-executable"))).is_err()
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sole_spawn_overrides_a_callers_shared_process_group() {
        // S4 caller asks to join the parent's process group => spawn replaces it
        // atomically with the child's private group. A forgotten or overwritten
        // configure step must never signal the Worker/test runner's own group.
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::lifecycle_child", "--ignored"])
            .env("AWAKEN_LIFECYCLE_CHILD", "wait")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .as_std_mut()
            .process_group(nix::unistd::getpgrp().as_raw());
        let process = LocalProcess::spawn(&mut command).unwrap();
        let pid = nix::unistd::Pid::from_raw(process.id().parse().unwrap());
        assert_eq!(nix::unistd::getpgid(Some(pid)).unwrap(), pid);
        assert_ne!(pid, nix::unistd::getpgrp());
        process.signal(Signal::Kill).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn wait_poll_and_cancel_interleavings_remain_bounded() {
        // W1 wait first, W2 signal first, W3 natural exit, W4 cancelled waiter.
        // All OS rows use a real native child (no shell portability assumption).
        // Effects: poll/signal are not blocked by wait, two waiters converge,
        // repeated wait/cancel settles. This tests direct-child ownership; the
        // separate Unix group test owns descendant propagation.
        for order in 0..4 {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "tests::lifecycle_child", "--ignored"])
                .env(
                    "AWAKEN_LIFECYCLE_CHILD",
                    if order == 2 { "exit" } else { "wait" },
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let process = LocalProcess::spawn(&mut command).unwrap();
            let campaign = async {
                if order == 0 || order == 3 {
                    let waiting = process.wait();
                    tokio::pin!(waiting);
                    tokio::select! {
                        biased;
                        result = &mut waiting => panic!("long child exited: {result:?}"),
                        () = tokio::task::yield_now() => {}
                    }
                    assert!(
                        process.poll().await.unwrap().is_none(),
                        "W1 wait cannot retain lock"
                    );
                    if order == 0 {
                        process.signal(Signal::Kill).await.unwrap();
                        waiting.await.unwrap();
                    }
                }
                if order != 2 {
                    process.signal(Signal::Kill).await.unwrap();
                }
                let (a, b) = tokio::join!(process.wait(), process.wait());
                assert_eq!(a.unwrap().code, b.unwrap().code, "same cached OS exit");
                process.signal(Signal::Kill).await.unwrap();
                assert!(process.poll().await.unwrap().is_some());
            };
            tokio::time::timeout(Duration::from_secs(5), campaign)
                .await
                .expect("W1-W4 bounded lifecycle");
        }
    }
}
