//! Systemd-free Linux execution ownership. Sanitized bubblewrap starts a
//! trusted helper; an inherited private socket carries workload input and OS
//! descriptors. Bootstrap frames and injected credentials use that private
//! channel, not business stdio; workload output remains the caller's concern.
//! Session admission, durable identity and recovery remain caller duties.

use crate::{LocalProcess, ProcessExecutionIdentity, ProcessNamespaceIdentity};
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
pub use process_io::ScopedStdio;
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
/// Suspend-inclusive clock shared by the Worker proof and physical cutoff.
/// Values are meaningful only during this boot, never durable authority.
pub fn execution_elapsed_time() -> io::Result<Duration> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let seconds = u64::try_from(time.tv_sec).map_err(|_| invalid())?;
    let nanos = u32::try_from(time.tv_nsec).map_err(|_| invalid())?;
    if nanos >= 1_000_000_000 {
        return Err(invalid());
    }
    Ok(Duration::new(seconds, nanos))
}
fn boot_millis() -> io::Result<u64> {
    u64::try_from(execution_elapsed_time()?.as_millis()).map_err(|_| invalid())
}

/// Conservative OS cutoff derived by the caller from CURRENT remaining Session
/// authority. BOOTTIME counts suspend; this is not a renewable business lease
/// and cannot be persisted across host boots or used to authorize adoption.
#[derive(Clone, Copy)]
pub struct ExecutionDeadline(u64);
impl ExecutionDeadline {
    pub fn after(remaining: Duration) -> io::Result<Self> {
        Self::from_start(boot_millis()?, remaining)
    }

    fn from_start(start: u64, remaining: Duration) -> io::Result<Self> {
        if remaining.is_zero() || remaining > MAX_WINDOW || remaining.as_millis() == 0 {
            return Err(invalid());
        }
        Ok(Self(
            start
                .checked_add(remaining.as_millis() as u64)
                .ok_or_else(invalid)?,
        ))
    }

    /// Intersect a current relative proof with an EARLIER OS clock sample.
    /// Sampling after authority/lock waits could add that latency to the proof.
    /// This performs the effect under the original authority guard, with one
    /// immutable cutoff; delay may shorten availability but never mint time.
    pub fn with_current_authority(
        authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        effect: &mut dyn FnMut(Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let start = boot_millis()?;
        authority.with_current(&mut |remaining| {
            let deadline = Self::from_start(start, remaining)?;
            deadline.remaining()?;
            effect(deadline)
        })
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

/// Borrowed, already-materialized physical launch input. The provider resolves
/// paths/credentials once and explicitly selects business stdio; the helper
/// must not reinterpret the logical cwd retained in `materialized`.
pub struct ScopedCommand<'a> {
    pub materialized: &'a MaterializedCommand,
    pub cwd: &'a Path,
    pub stdio: ScopedStdio,
}

pub struct ScopedProcess {
    process: LocalProcess,
    _namespace: OwnedFd,
    init: OwnedFd,
    control: UnixStream,
    control_writer: std::sync::Mutex<()>,
    io: process_io::ProcessIo,
    identity: ProcessExecutionIdentity,
}

impl ScopedProcess {
    /// Caller holds the realization lock. The callback durably binds identity
    /// BEFORE GO and commits GO under the SAME synchronous guard as revocation.
    /// Inputs are trusted deployment/physical paths; no path is re-projected.
    /// Callback must not panic. This is not an installed Worker capability.
    pub async fn spawn_authorized(
        launcher: &Path,
        helper: &Path,
        command: ScopedCommand<'_>,
        deadline: ExecutionDeadline,
        timeout: Duration,
        mut authorize: impl FnMut(&ProcessExecutionIdentity, ScopedLaunchGrant<'_>) -> io::Result<()>,
    ) -> io::Result<Self> {
        if !launcher.is_absolute()
            || !helper.is_absolute()
            || timeout.is_zero()
            || timeout > MAX_WAIT
        {
            return Err(invalid());
        }
        deadline.remaining()?;
        let frame = wire::encode(command.materialized, command.cwd, deadline.0)?;
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
            let namespace_identity = ProcessNamespaceIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            };
            let identity = ProcessExecutionIdentity::capture(namespace_identity, &init)?;
            descriptor::send(&control, &handles).await?;
            drop(handles);
            grant::authorize_checked(&mut control, &frame, |grant| authorize(&identity, grant))
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
    pub fn identity(&self) -> &ProcessExecutionIdentity {
        &self.identity
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
        crate::execution_identity::pidfd_exited(&self.init)
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
            .map_err(|e| SandboxError::backend("poll", e))?
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
                .map_err(|e| SandboxError::backend("signal", e))?
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
                        .map_err(|e| SandboxError::backend("signal", e))?
                    {
                        Ok(())
                    } else {
                        Err(SandboxError::backend("signal", error))
                    }
                }
            };
        }
        match pidfd_send_signal(&self.init, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(SandboxError::backend("signal", error)),
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

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use awaken_provisioning_contract::CurrentExecutionAuthority;

    struct Authority<F>(F);
    impl<F: Fn(&mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> + Send + Sync>
        CurrentExecutionAuthority for Authority<F>
    {
        fn with_current(
            &self,
            effect: &mut dyn FnMut(Duration) -> io::Result<()>,
        ) -> io::Result<()> {
            (self.0)(effect)
        }
    }

    #[test]
    fn cutoff_arithmetic_has_one_bounded_conservative_conversion() {
        // C1 fixed earlier anchor; C2 positive bounded/sub-ms/zero/oversized
        // budget; C3 addition overflow. E1 floor to milliseconds, never round
        // upward; E2 reject invalid/overflow. D1 normal; D2 fractional; D3 exact
        // maximum; D4-D7 each invalid input; D8 exact largest safe sum.
        for (start, budget, expected) in [
            (100, Duration::from_millis(20), Some(120)),
            (100, Duration::from_micros(1_999), Some(101)),
            (0, MAX_WINDOW, Some(MAX_WINDOW.as_millis() as u64)),
            (100, Duration::ZERO, None),
            (100, Duration::from_nanos(999_999), None),
            (100, MAX_WINDOW + Duration::from_nanos(1), None),
            (u64::MAX, Duration::from_millis(1), None),
            (u64::MAX - 1, Duration::from_millis(1), Some(u64::MAX)),
        ] {
            assert_eq!(
                ExecutionDeadline::from_start(start, budget)
                    .ok()
                    .map(|d| d.0),
                expected
            );
        }
    }

    #[test]
    fn authority_wait_cannot_mint_cutoff_time_or_invoke_an_expired_effect() {
        // D9 budget stays live across an authority wait -> fixed cutoff no
        // later than authority-entry time + budget. D10 wait consumes budget
        // -> TimedOut and zero effects. Unlike transport-delay tests, the wait
        // occurs BEFORE conversion, reproducing the prior after(remaining) bug.
        for expired in [false, true] {
            let budget = if expired {
                Duration::from_millis(20)
            } else {
                Duration::from_secs(5)
            };
            let entered = std::sync::Mutex::new(0);
            let authority = Authority(|effect: &mut dyn FnMut(Duration) -> io::Result<()>| {
                *entered.lock().unwrap() = boot_millis()?;
                std::thread::sleep(Duration::from_millis(30));
                effect(budget)
            });
            let mut delivered = None;
            let result = ExecutionDeadline::with_current_authority(&authority, &mut |deadline| {
                assert!(delivered.replace(deadline.0).is_none());
                Ok(())
            });
            if expired {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut, "D10");
                assert!(delivered.is_none(), "D10 no expired effect");
            } else {
                result.unwrap();
                assert!(
                    delivered.unwrap() <= *entered.lock().unwrap() + budget.as_millis() as u64,
                    "D9 no authority-wait time minted"
                );
            }
        }
    }

    #[test]
    fn cutoff_port_propagates_denial_and_effect_error_without_substitution() {
        // D11 root denial -> same error, no callback. D12 live effect failure
        // -> exactly one invocation and original error; no retry or new grant.
        let denied = Authority(|_: &mut dyn FnMut(Duration) -> io::Result<()>| {
            Err(io::ErrorKind::PermissionDenied.into())
        });
        assert_eq!(
            ExecutionDeadline::with_current_authority(&denied, &mut |_| panic!("D11"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let live = Authority(|effect: &mut dyn FnMut(Duration) -> io::Result<()>| {
            effect(Duration::from_secs(5))
        });
        let mut calls = 0;
        let error = ExecutionDeadline::with_current_authority(&live, &mut |_| {
            calls += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        })
        .unwrap_err();
        assert_eq!(calls, 1, "D12");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
