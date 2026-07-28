//! One local child-process lifecycle adapter shared by trusted subprocess and
//! Local/Namespace sandbox launches.
//!
//! Protocol supervisors own *when* to terminate. This adapter owns the
//! OS-specific guarantee that one signal reaches the launched process and all
//! descendants in its private process group.

use async_trait::async_trait;
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Configure a command so the child becomes leader of a private process group.
pub fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// A locally owned process whose lifecycle signal targets its complete group.
pub struct LocalProcess {
    id: String,
    #[cfg(unix)]
    process_group: Option<nix::unistd::Pid>,
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
            child: Mutex::new(child),
        }
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
        self.child
            .lock()
            .await
            .wait()
            .await
            .map(exit)
            .map_err(|error| SandboxError::new(error.to_string()))
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
            let Some(group) = self.process_group else {
                return Ok(());
            };
            let signal = match signal {
                Signal::Term => nix::sys::signal::Signal::SIGTERM,
                Signal::Kill => nix::sys::signal::Signal::SIGKILL,
                Signal::Int => nix::sys::signal::Signal::SIGINT,
            };
            return nix::sys::signal::killpg(group, signal)
                .map_err(|error| SandboxError::new(error.to_string()));
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    #[tokio::test]
    async fn term_reaches_the_spawned_process_and_its_descendant() {
        // Cause/effect graph and decision rule PG1:
        // C1 a command is configured before spawn; C2 it creates a descendant;
        // C3 ProcessHandle receives TERM.
        // C1+C2+C3 => E1 the leader exits and E2 the descendant's TERM trap
        // runs. Signal-kind mapping is exhaustive in `signal`; the Supervisor's
        // TERM→KILL escalation tests cover a descendant that does not settle.
        let directory = tempfile::tempdir().expect("temporary marker directory");
        let marker = directory.path().join("descendant-terminated");
        let script = format!(
            "sh -c 'trap \"printf child > {} ; exit 0\" TERM; while :; do sleep 1; done' & wait",
            marker.display()
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

        tokio::time::sleep(Duration::from_millis(100)).await;
        process.signal(Signal::Term).await.expect("signal group");
        tokio::time::timeout(Duration::from_secs(2), process.wait())
            .await
            .expect("leader settles")
            .expect("wait succeeds");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant observes TERM");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "child");
    }
}
