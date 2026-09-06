//! Systemd-free Linux execution ownership. Sanitized bubblewrap starts a
//! trusted helper; an inherited private socket carries workload input and OS
//! descriptors. Bootstrap frames and injected credentials use that private
//! channel, not business stdio; workload output remains the caller's concern.
//! Session admission, durable identity and recovery remain caller duties.

use crate::LocalProcess;
use async_trait::async_trait;
use awaken_provisioning_contract::{
    ExitStatus, MaterializedCommand, ProcessHandle, SandboxError, Signal,
};
use rustix::process::{Pid, PidfdFlags, pidfd_open, pidfd_send_signal};
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    path::Path,
    time::Duration,
};
use tokio::net::UnixStream;
use tokio::net::unix::pipe::{Receiver, Sender};

mod descriptor;
mod grant;
mod process_io;
mod renewal;
mod wire;
pub use grant::ScopedLaunchGrant;
pub use renewal::ScopedDeadlineGrant;

const MAX_WAIT: Duration = Duration::from_secs(30);
const MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid scoped launch input")
}
fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "scoped execution deadline exceeded",
    )
}
fn boot_millis() -> io::Result<u64> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let seconds = u64::try_from(time.tv_sec).map_err(|_| invalid())?;
    seconds
        .checked_mul(1000)
        .and_then(|v| v.checked_add(time.tv_nsec as u64 / 1_000_000))
        .ok_or_else(invalid)
}

/// Conservative OS cutoff derived by the caller from CURRENT remaining Session
/// authority. BOOTTIME counts suspend; this is not a renewable business lease
/// and cannot be persisted across host boots or used to authorize adoption.
#[derive(Clone, Copy)]
pub struct ExecutionDeadline(u64);
impl ExecutionDeadline {
    pub fn after(remaining: Duration) -> io::Result<Self> {
        if remaining.is_zero() || remaining > MAX_WINDOW || remaining.as_millis() == 0 {
            return Err(invalid());
        }
        Ok(Self(
            boot_millis()?
                .checked_add(remaining.as_millis() as u64)
                .ok_or_else(invalid)?,
        ))
    }
    fn remaining(self) -> io::Result<Duration> {
        let millis = self
            .0
            .checked_sub(boot_millis()?)
            .filter(|v| *v > 0)
            .ok_or_else(timed_out)?;
        let remaining = Duration::from_millis(millis);
        if remaining > MAX_WINDOW {
            return Err(invalid());
        }
        Ok(remaining)
    }
}

/// Physical evidence, NOT authority to adopt or stop an arbitrary host process.
/// Durable recovery additionally needs the existing realization fence and boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessNamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}

pub struct ScopedProcess {
    process: LocalProcess,
    _namespace: OwnedFd,
    init: OwnedFd,
    control: UnixStream,
    control_writer: std::sync::Mutex<()>,
    io: process_io::ProcessIo,
    identity: ProcessNamespaceIdentity,
}

impl ScopedProcess {
    /// Caller holds the realization lock. The callback durably binds identity
    /// BEFORE GO and commits GO under the SAME synchronous guard as revocation.
    /// Inputs are trusted deployment/physical paths; no path is re-projected.
    /// Callback must not panic. This is not an installed Worker capability.
    pub async fn spawn_authorized(
        launcher: &Path,
        helper: &Path,
        command: &MaterializedCommand,
        cwd: &Path,
        deadline: ExecutionDeadline,
        timeout: Duration,
        mut authorize: impl FnMut(ProcessNamespaceIdentity, ScopedLaunchGrant<'_>) -> io::Result<()>,
    ) -> io::Result<Self> {
        if !launcher.is_absolute()
            || !helper.is_absolute()
            || timeout.is_zero()
            || timeout > MAX_WAIT
        {
            return Err(invalid());
        }
        deadline.remaining()?;
        let frame = wire::encode(command, cwd, deadline.0)?;
        tokio::time::timeout(timeout, async {
            let (mut control, inherited) = UnixStream::pair()?;
            let inherited: OwnedFd = inherited.into_std()?.into();
            let (io, handles) = process_io::ProcessIo::new(command.stdio)?;
            let mut bootstrap = tokio::process::Command::new(launcher);
            bootstrap
                .args(crate::BUBBLEWRAP_PROCESS_BOUNDARY)
                .args(["--dev-bind", "/", "/", "--proc", "/proc", "--"])
                .arg(helper)
                .arg("scoped-exec")
                .env_clear()
                .current_dir("/")
                .stdin(inherited)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let process = LocalProcess::spawn(&mut bootstrap)?;
            drop(bootstrap); // Close parent's duplicate inherited endpoint.
            let mut descriptors = descriptor::receive(&control, 2).await?.into_iter();
            let namespace = descriptors.next().ok_or_else(invalid)?;
            let init = descriptors.next().ok_or_else(invalid)?;
            if rustix::fs::fstatfs(&namespace)?.f_type != 0x6e736673 {
                return Err(invalid());
            }
            let stat = rustix::fs::fstat(&namespace)?;
            let identity = ProcessNamespaceIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            };
            descriptor::send(&control, &handles).await?;
            drop(handles);
            grant::authorize_checked(&mut control, &frame, |grant| authorize(identity, grant))
                .await?;
            // No await after GO: preserve the handle even if the callback
            // reported an error after its irreversible commit.
            Ok(Self {
                process,
                _namespace: namespace,
                init,
                control,
                control_writer: std::sync::Mutex::new(()),
                io,
                identity,
            })
        })
        .await
        .map_err(|_| timed_out())?
    }
    pub fn identity(&self) -> ProcessNamespaceIdentity {
        self.identity
    }

    /// Request a conservative cutoff extension under CURRENT Session authority.
    /// Derive the deadline and commit INSIDE the caller's existing authorization
    /// lock. A send receipt is not a helper ACK, business renewal or stop proof.
    /// Denial does not revoke an older still-valid cutoff; explicit revocation
    /// must also force-stop the scope. Backpressure is bounded and reauthorizes.
    pub async fn refresh_deadline_authorized(
        &self,
        timeout: Duration,
        mut authorize: impl FnMut(ScopedDeadlineGrant<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        if timeout.is_zero() || timeout > MAX_WAIT {
            return Err(invalid());
        }
        tokio::time::timeout(
            timeout,
            grant::commit_checked(&self.control, |committed| {
                if self.tree_exited()? {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                authorize(ScopedDeadlineGrant::new(
                    &self.control,
                    &self.control_writer,
                    committed,
                ))
            }),
        )
        .await
        .map_err(|_| timed_out())?
    }
    pub fn take_stdin(&mut self) -> Option<Sender> {
        self.io.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<Receiver> {
        self.io.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<Receiver> {
        self.io.stderr.take()
    }

    /// init exit, not leader exit, is the namespace termination observation.
    /// Uninterruptible I/O may delay it: timeout must not become a stop receipt.
    fn tree_exited(&self) -> io::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = [PollFd::new(&self.init, PollFlags::IN)];
        poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        let observed = fds[0].revents();
        if observed.intersects(PollFlags::NVAL | PollFlags::ERR) {
            return Err(invalid());
        }
        Ok(observed.intersects(PollFlags::IN | PollFlags::HUP))
    }
}

impl Drop for ScopedProcess {
    fn drop(&mut self) {
        // Exact pidfd stop request only, never an observed/durable empty receipt.
        let _ = pidfd_send_signal(&self.init, rustix::process::Signal::KILL);
    }
}

#[async_trait]
impl ProcessHandle for ScopedProcess {
    fn id(&self) -> &str {
        self.process.id()
    }
    async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        loop {
            if let Some(status) = self.poll().await? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
        let status = self.process.poll().await?;
        if self
            .tree_exited()
            .map_err(|e| SandboxError::new(e.to_string()))?
        {
            Ok(status)
        } else {
            Ok(None)
        }
    }
    async fn signal(&self, signal: Signal) -> Result<(), SandboxError> {
        if matches!(signal, Signal::Term | Signal::Int) {
            // Preserve the ProcessHandle signal contract: graceful requests go
            // through the trusted helper to the workload's private group. The
            // helper never targets a host PID. Force-stop targets init instead.
            if self
                .tree_exited()
                .map_err(|e| SandboxError::new(e.to_string()))?
            {
                return Ok(());
            }
            let byte = match signal {
                Signal::Term => grant::TERMINATE,
                Signal::Int => grant::INTERRUPT,
                Signal::Kill => unreachable!(),
            };
            return match grant::send_signal(&self.control, &self.control_writer, byte).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    // The helper can exit between the first poll and delivery.
                    // Only an exact init-exit observation makes that idempotent;
                    // a broken channel alone remains an unknown/error outcome.
                    if self
                        .tree_exited()
                        .map_err(|e| SandboxError::new(e.to_string()))?
                    {
                        Ok(())
                    } else {
                        Err(SandboxError::new(error.to_string()))
                    }
                }
            };
        }
        match pidfd_send_signal(&self.init, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(SandboxError::new(error.to_string())),
        }
    }
}

/// Trusted binary role only. stdin is the private inherited socket; the final
/// workload receives explicit replacement stdio. No unsafe FD constructor.
pub async fn bootstrap() -> io::Result<ExitStatus> {
    if std::process::id() != 2 || rustix::process::getppid() != Pid::from_raw(1) {
        return Err(invalid());
    }
    let inherited = std::io::stdin().as_fd().try_clone_to_owned()?;
    let stream = std::os::unix::net::UnixStream::from(inherited);
    stream.set_nonblocking(true)?;
    let mut control = UnixStream::from_std(stream)?;
    let (mut deadline, process) = tokio::time::timeout(MAX_WAIT, async {
        let namespace: OwnedFd = std::fs::File::open("/proc/self/ns/pid")?.into();
        let init = pidfd_open(Pid::from_raw(1).ok_or_else(invalid)?, PidfdFlags::empty())?;
        descriptor::send(&control, &[namespace, init]).await?;
        let mut stdio = descriptor::receive(&control, 3).await?.into_iter();
        let (deadline, mut command) = wire::receive(&mut control).await?;
        let deadline = ExecutionDeadline(deadline);
        deadline.remaining()?;
        command.stdin(stdio.next().ok_or_else(invalid)?);
        command.stdout(stdio.next().ok_or_else(invalid)?);
        command.stderr(stdio.next().ok_or_else(invalid)?);
        grant::await_grant(&mut control).await?;
        deadline.remaining()?;
        let process = LocalProcess::spawn(&mut command.into())?;
        Ok::<_, io::Error>((deadline, process))
    })
    .await
    .map_err(|_| timed_out())??;
    loop {
        let message = tokio::select! {
            status = process.wait() => return status.map_err(|_| invalid()),
            message = renewal::receive_before_cutoff(&mut control, deadline) => message?,
        };
        match message {
            renewal::ControlMessage::Signal(signal) => {
                process.signal(signal).await.map_err(|_| invalid())?;
            }
            renewal::ControlMessage::Deadline(incoming) => {
                deadline.0 = renewal::apply_deadline(deadline.0, incoming, boot_millis()?)?;
            }
        }
    }
}
