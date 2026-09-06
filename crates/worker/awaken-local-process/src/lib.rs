//! Process waiting policy plus the local child-process lifecycle adapter.
//! Local/Namespace and container exec handles share cancellation-safe waiting;
//! trusted subprocess and Local/Namespace launches share the OS child adapter.
//!
//! Protocol supervisors own *when* to terminate. This adapter owns the
//! OS-specific termination boundary: Unix targets the private process group;
//! other platforms currently target the direct child only.

use async_trait::async_trait;
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use tokio::process::{Child, Command};
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

/// Configure a command so the child becomes leader of a private process group.
pub fn configure_process_group(command: &mut Command) {
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
    child: Mutex<Child>,
}

impl LocalProcess {
    /// Wrap a child spawned from a command prepared by
    /// [`configure_process_group`].
    #[must_use]
    pub fn spawned(child: Child) -> Self {
        let pid = child.id();
        Self {
            id: pid.map(|value| value.to_string()).unwrap_or_default(),
            #[cfg(unix)]
            process_group: pid.map(|value| nix::unistd::Pid::from_raw(value as i32)),
            #[cfg(unix)]
            graceful_group: None,
            child: Mutex::new(child),
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
            .ok_or_else(|| SandboxError::new("invalid private signal group"))?;
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
                    Err(error) => return Err(SandboxError::new(error.to_string())),
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .map_err(|_| SandboxError::new("private signal group setup timed out"))?
    }
}

fn exit(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
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
        self.child
            .lock()
            .await
            .try_wait()
            .map(|status| status.map(exit))
            .map_err(|error| SandboxError::new(error.to_string()))
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
            return match nix::sys::signal::killpg(group, signal) {
                Ok(()) => Ok(()),
                Err(signal_error) => {
                    // `poll(None) -> natural exit -> signal` is an unavoidable OS
                    // race. macOS may report EPERM for the vanished process group
                    // (other Unix targets commonly report ESRCH). Only suppress the
                    // signal error after the owned child confirms it has exited; a
                    // still-live or unobservable child continues to fail closed.
                    match self.child.lock().await.try_wait() {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => Err(SandboxError::new(format!(
                            "{signal_error}; child {} is still running",
                            self.id
                        ))),
                        Err(poll_error) => Err(SandboxError::new(format!(
                            "{signal_error}; child exit check failed: {poll_error}"
                        ))),
                    }
                }
            };
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            self.child
                .lock()
                .await
                .start_kill()
                .map_err(|error| SandboxError::new(error.to_string()))
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
        // Signal-kind mapping is exhaustive in `signal`; the Supervisor's
        // TERM→KILL escalation tests cover a descendant that does not settle.
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
        configure_process_group(&mut command);
        let process = LocalProcess::spawned(command.spawn().expect("spawn process group"));

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
        configure_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let _input = child.stdin.take().unwrap();
        let process = LocalProcess::spawned(child);
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
        // The supervisor can observe a running child, receive its terminal protocol
        // frame, and then race its natural exit while beginning cleanup. Once the
        // owned Child confirms the exit, a stale process-group signal is successful
        // cleanup rather than a new business failure.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let process = LocalProcess::spawned(command.spawn().expect("spawn short-lived process"));

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

        process
            .signal(Signal::Term)
            .await
            .expect("an already-exited owned process is settled");
    }

    #[test]
    #[ignore = "child participant, invoked by the cross-platform lifecycle matrix"]
    fn lifecycle_child() {
        if std::env::var("AWAKEN_LIFECYCLE_CHILD").as_deref() == Ok("wait") {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
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
                .stderr(Stdio::null())
                .kill_on_drop(true);
            configure_process_group(&mut command);
            let process = LocalProcess::spawned(command.spawn().unwrap());
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
