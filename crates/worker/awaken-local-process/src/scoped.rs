//! Trusted Linux bootstrap for the existing local process owner.
//!
//! Admission -> sanitized helper -> exact cgroup join -> JOINED -> GO -> exec.
//! No workload environment (including loader hooks) reaches the helper. A
//! private peer-PID-checked abstract Unix socket carries transient materialized
//! inputs, never a planning record, argv secret, file, or business-stdio frame.
//! Cancellation before GO cannot run workload code. Cancellation may leave a
//! briefly exiting trusted helper; it is not a durable subtree-empty receipt.
//! External supervision and Session/realization fencing remain caller duties.

use std::cell::Cell;
use std::ffi::OsStr;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixListener as StdListener};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use awaken_provisioning_contract::{MaterializedCommand, Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use zeroize::Zeroizing;

use crate::LocalProcess;
use crate::cgroup::{CgroupIdentity, LinuxProcessScope};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_ITEMS: usize = 4096;
const MAX_WAIT: Duration = Duration::from_secs(30);
const JOINED: u8 = 1;
const GO: u8 = 2;
static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid scoped launch input")
}

fn deadline_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "scoped launch handshake timed out")
}

fn address(name: &str) -> io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(name.as_bytes())
}

fn peer(stream: &UnixStream, expected: u32) -> io::Result<()> {
    if expected == 0 || stream.peer_cred()?.pid().map(i64::from) != Some(i64::from(expected)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scoped launch peer identity mismatch",
        ));
    }
    Ok(())
}

/// One-use GO operation supplied only after the trusted helper acknowledged
/// membership and the socket became writable. The authorization adapter calls
/// `commit` INSIDE its synchronous lease-read critical section; returning a
/// boolean before that section is released would leave a revoke/grant race.
pub struct ScopedLaunchGrant<'a> {
    control: &'a UnixStream,
    committed: &'a Cell<bool>,
}

impl ScopedLaunchGrant<'_> {
    pub fn commit(self) -> io::Result<()> {
        match self.control.try_write(&[GO]) {
            Ok(1) => {
                self.committed.set(true);
                Ok(())
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "incomplete scoped grant",
            )),
            Err(error) => Err(error),
        }
    }
}

impl LocalProcess {
    /// Launch a materialized command after joining this exact scope. `cwd` is
    /// the physical cwd already resolved by the provider's existing path view;
    /// this OS adapter must not re-project the command's logical cwd.
    /// The provider must retain its realization/path authorization through this
    /// call. A cwd pathname is not a pinned filesystem-generation identity.
    ///
    /// `helper` is an absolute trusted deployment path to awaken-sandbox, NOT
    /// selected by workload input. Its own environment is empty and cwd is `/`.
    /// `command.stdio` applies to all three business streams. Success means GO
    /// was issued, not that exec or the ACP/Hand protocol became ready. There is
    /// no await after issuing GO and before returning ownership to the caller.
    /// The handshake timeout must be positive and no more than 30 seconds.
    pub async fn spawn_scoped(
        scope: &LinuxProcessScope,
        helper: &Path,
        command: &MaterializedCommand,
        cwd: &Path,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::spawn_scoped_authorized(scope, helper, command, cwd, timeout, |grant| grant.commit())
            .await
    }

    /// The adapter must check current authority and commit the one-use grant
    /// under the SAME synchronous authorization guard. No async I/O may occur
    /// while that guard is held. The caller also retains the physical
    /// realization lock through this call. Rejection before commit cannot issue
    /// GO; after commit, success preserves the process handle even if the
    /// adapter subsequently returns an error. Callbacks must not panic.
    pub async fn spawn_scoped_authorized(
        scope: &LinuxProcessScope,
        helper: &Path,
        command: &MaterializedCommand,
        cwd: &Path,
        timeout: Duration,
        authorize_effect: impl FnMut(ScopedLaunchGrant<'_>) -> io::Result<()>,
    ) -> io::Result<Self> {
        if !helper.is_absolute() || timeout.is_zero() || timeout > MAX_WAIT {
            return Err(invalid());
        }
        let frame = encode(scope.path(), scope.identity(), command, cwd)?;
        tokio::time::timeout(timeout, async {
            let _permit = scope.launch_permit().await?;
            let name = format!(
                "awaken-scoped-{}-{}",
                std::process::id(),
                NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
            );
            let listener = StdListener::bind_addr(&address(&name)?)?;
            listener.set_nonblocking(true)?;
            let listener = UnixListener::from_std(listener)?;
            let mut bootstrap = tokio::process::Command::new(helper);
            bootstrap
                .args(["scoped-exec", &name, &std::process::id().to_string()])
                .env_clear()
                .current_dir("/")
                .stdin(stdio(command.stdio))
                .stdout(stdio(command.stdio))
                .stderr(stdio(command.stdio));
            let process = Self::spawn(&mut bootstrap)?;
            let (mut control, _) = listener.accept().await?;
            peer(&control, process.id.parse().map_err(|_| invalid())?)?;
            authorize_checked(&mut control, &frame, authorize_effect).await?;
            Ok(process)
        })
        .await
        .map_err(|_| deadline_error())?
    }
}

#[cfg(test)]
async fn authorize(control: &mut UnixStream, frame: &[u8]) -> io::Result<()> {
    authorize_checked(control, frame, |grant| grant.commit()).await
}

async fn authorize_checked(
    control: &mut UnixStream,
    frame: &[u8],
    mut authorize_effect: impl FnMut(ScopedLaunchGrant<'_>) -> io::Result<()>,
) -> io::Result<()> {
    control.write_u32(frame.len() as u32).await?;
    control.write_all(frame).await?;
    if control.read_u8().await? != JOINED {
        return Err(invalid());
    }
    loop {
        control.writable().await?;
        let committed = Cell::new(false);
        let result = authorize_effect(ScopedLaunchGrant {
            control,
            committed: &committed,
        });
        // Once GO has been sent, a later adapter error cannot turn this into
        // an unlaunched result and discard the only live process handle.
        if committed.get() {
            return Ok(());
        }
        match result {
            Ok(()) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "authorization omitted the scoped grant",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Release all callback guards before retrying; readiness can
                // be spurious. Yield also keeps cancellation/timeout bounded.
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn stdio(mode: Stdio) -> std::process::Stdio {
    match mode {
        Stdio::Inherit => std::process::Stdio::inherit(),
        Stdio::Piped => std::process::Stdio::piped(),
        Stdio::Null => std::process::Stdio::null(),
    }
}

/// Called only by the trusted binary role, never the multi-Session Worker.
/// Returns only on failure; successful exec preserves PID, cwd and stdio but
/// closes the control socket via CLOEXEC. Even an abandoned caller is bounded.
pub async fn bootstrap(socket: &str, parent: u32) -> io::Error {
    match tokio::time::timeout(MAX_WAIT, bootstrap_inner(socket, parent)).await {
        Ok(Err(error)) => error,
        Err(_) => deadline_error(),
        Ok(Ok(())) => io::Error::other("scoped exec unexpectedly returned"),
    }
}

async fn bootstrap_inner(socket: &str, parent: u32) -> io::Result<()> {
    let mut control = UnixStream::connect_addr(&address(socket)?.into()).await?;
    peer(&control, parent)?;
    let size = control.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(invalid());
    }
    let mut frame = Zeroizing::new(vec![0; size]);
    control.read_exact(&mut frame).await?;
    let (scope, identity, mut workload) = decode(&frame)?;
    scope.enter_current_process(identity)?;
    await_grant(&mut control).await?;
    Err(workload.exec())
}

async fn await_grant(control: &mut UnixStream) -> io::Result<()> {
    control.write_u8(JOINED).await?;
    if control.read_u8().await? != GO {
        return Err(invalid());
    }
    Ok(())
}

// A bounded private OS handoff, intentionally NOT serde on MaterializedCommand:
// that domain/planning type must remain non-serializable. Decode borrows the
// zeroizing frame; only the final OS Command owns copies for exec. No Debug,
// persistence, log, or error includes any payload field.
fn encode(
    path: &Path,
    identity: CgroupIdentity,
    command: &MaterializedCommand,
    cwd: &Path,
) -> io::Result<Zeroizing<Vec<u8>>> {
    if !cwd.is_absolute() || command.argv.is_empty() || command.argv[0].is_empty() {
        return Err(invalid());
    }
    // Avoid realloc leaving earlier plaintext copies in the allocator.
    let mut frame = Zeroizing::new(Vec::with_capacity(MAX_FRAME));
    frame.extend_from_slice(b"AWSC1");
    field(&mut frame, path.as_os_str().as_bytes())?;
    frame.extend_from_slice(&identity.device.to_be_bytes());
    frame.extend_from_slice(&identity.inode.to_be_bytes());
    field(&mut frame, cwd.as_os_str().as_bytes())?;
    count(&mut frame, command.argv.len())?;
    for arg in &command.argv {
        field(&mut frame, arg.as_bytes())?;
    }
    count(&mut frame, command.env.len())?;
    for env in &command.env {
        if env.name.is_empty() || env.name.contains('=') {
            return Err(invalid());
        }
        field(&mut frame, env.name.as_bytes())?;
        field(&mut frame, env.value.expose().as_bytes())?;
    }
    Ok(frame)
}

fn count(frame: &mut Vec<u8>, size: usize) -> io::Result<()> {
    if size > MAX_ITEMS || frame.len() + 4 > MAX_FRAME {
        return Err(invalid());
    }
    frame.extend_from_slice(&(size as u32).to_be_bytes());
    Ok(())
}

fn field(frame: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if bytes.contains(&0) || bytes.len() > MAX_FRAME || frame.len() + 4 + bytes.len() > MAX_FRAME {
        return Err(invalid());
    }
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(bytes);
    Ok(())
}

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if count > self.0.len() {
            return Err(invalid());
        }
        let (value, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(value)
    }

    fn u32(&mut self) -> io::Result<usize> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| invalid())?) as usize)
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| invalid())?,
        ))
    }

    fn field(&mut self) -> io::Result<&'a OsStr> {
        let size = self.u32()?;
        let value = self.take(size)?;
        if value.contains(&0) {
            return Err(invalid());
        }
        Ok(OsStr::from_bytes(value))
    }

    fn count(&mut self) -> io::Result<usize> {
        let size = self.u32()?;
        if size > MAX_ITEMS {
            return Err(invalid());
        }
        Ok(size)
    }
}

fn parse(frame: &[u8]) -> io::Result<(&Path, CgroupIdentity, std::process::Command)> {
    if frame.len() > MAX_FRAME {
        return Err(invalid());
    }
    let mut input = Input(frame);
    if input.take(5)? != b"AWSC1" {
        return Err(invalid());
    }
    let path = Path::new(input.field()?);
    let identity = CgroupIdentity {
        device: input.u64()?,
        inode: input.u64()?,
    };
    let cwd = Path::new(input.field()?);
    let argc = input.count()?;
    if !path.is_absolute() || !cwd.is_absolute() || argc == 0 {
        return Err(invalid());
    }
    let program = input.field()?;
    if program.is_empty() {
        return Err(invalid());
    }
    let mut command = std::process::Command::new(program);
    command.env_clear().current_dir(cwd);
    for _ in 1..argc {
        command.arg(input.field()?);
    }
    for _ in 0..input.count()? {
        let name = input.field()?;
        if name.is_empty() || name.as_bytes().contains(&b'=') {
            return Err(invalid());
        }
        command.env(name, input.field()?);
    }
    if !input.0.is_empty() {
        return Err(invalid());
    }
    Ok((path, identity, command))
}

fn decode(frame: &[u8]) -> io::Result<(LinuxProcessScope, CgroupIdentity, std::process::Command)> {
    let (path, identity, command) = parse(frame)?;
    Ok((LinuxProcessScope::open(path)?, identity, command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{MaterializedEnvValue, MaterializedEnvVar};

    fn identity() -> CgroupIdentity {
        CgroupIdentity {
            device: 17,
            inode: 42,
        }
    }

    fn fixture() -> MaterializedCommand {
        let mut command = MaterializedCommand::new(["/bin/echo", "", "a b\n$(not-a-shell)"]);
        command.env.push(MaterializedEnvVar {
            name: "PRIVATE_VALUE".into(),
            value: MaterializedEnvValue::Secret("test-only-secret".to_owned().into()),
        });
        command
    }

    #[test]
    fn private_handoff_roundtrips_exact_fields_and_rejects_malformed_input() {
        // Cause/effect decision table (no kernel prerequisites):
        // B1 valid absolute paths + argv/env => byte-exact cwd, args, env;
        // B2 truncation, trailing data, wrong version, NUL, bad count => reject;
        // B3 relative cwd, empty program, invalid env key, size overflow => reject
        // before any spawn/control write. Shell metacharacters are data (B1).
        // Physical cwd may contain non-UTF8 bytes on Linux, unlike planning text.
        let cwd = Path::new(OsStr::from_bytes(b"/tmp/space \xff"));
        let command = fixture();
        let frame = encode(Path::new("/scope"), identity(), &command, cwd).unwrap();
        let (path, actual, decoded) = parse(&frame).unwrap();
        assert_eq!(path, Path::new("/scope"));
        assert_eq!(actual, identity());
        assert_eq!(decoded.get_current_dir(), Some(cwd));
        assert_eq!(decoded.get_program(), OsStr::new("/bin/echo"));
        assert_eq!(
            decoded.get_args().collect::<Vec<_>>(),
            [OsStr::new(""), OsStr::new("a b\n$(not-a-shell)")]
        );
        assert_eq!(
            decoded.get_envs().collect::<Vec<_>>(),
            [(
                OsStr::new("PRIVATE_VALUE"),
                Some(OsStr::new("test-only-secret"))
            )]
        );
        for end in 0..frame.len() {
            assert!(parse(&frame[..end]).is_err(), "B2 truncated at {end}");
        }
        let mut malformed = frame.to_vec();
        malformed.push(1);
        assert!(parse(&malformed).is_err());
        malformed = frame.to_vec();
        malformed[0] = b'X';
        assert!(parse(&malformed).is_err());
        malformed = frame.to_vec();
        malformed[10] = 0;
        assert!(parse(&malformed).is_err());
        assert!(Input(&u32::MAX.to_be_bytes()).count().is_err());
        assert!(
            encode(
                Path::new("/scope"),
                identity(),
                &command,
                Path::new("relative")
            )
            .is_err()
        );
        for argv in [
            vec![],
            vec!["".into()],
            vec!["nul\0argument".into()],
            vec!["x".repeat(MAX_FRAME)],
        ] {
            let command = MaterializedCommand::new(argv);
            assert!(encode(Path::new("/scope"), identity(), &command, cwd).is_err());
        }
        for name in ["", "bad=key", "bad\0key"] {
            let mut command = fixture();
            command.env[0].name = name.into();
            assert!(encode(Path::new("/scope"), identity(), &command, cwd).is_err());
        }
        assert!(decode(&frame).is_err(), "lookalike path is not a cgroup");
    }

    #[tokio::test]
    async fn private_control_requires_exact_peer_and_join_ack_before_go() {
        // B4 exact live peer + JOINED => exactly one GO; B5 wrong peer => deny
        // before secret delivery; B6 wrong ACK/disconnect => no GO; B7 caller
        // cancellation while awaiting ACK => EOF, never a workload grant.
        for ack in [Some(JOINED), Some(99), None] {
            let (mut parent, mut child) = UnixStream::pair().unwrap();
            peer(&parent, std::process::id()).unwrap();
            assert_eq!(
                peer(&parent, 0).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(peer(&parent, std::process::id() + 1).is_err());
            let task = tokio::spawn(async move { authorize(&mut parent, b"fixture").await });
            assert_eq!(child.read_u32().await.unwrap(), 7);
            let mut bytes = [0; 7];
            child.read_exact(&mut bytes).await.unwrap();
            if let Some(ack) = ack {
                child.write_u8(ack).await.unwrap();
                if ack == JOINED {
                    assert_eq!(child.read_u8().await.unwrap(), GO);
                    assert!(task.await.unwrap().is_ok());
                } else {
                    assert!(task.await.unwrap().is_err());
                    assert!(child.read_u8().await.is_err());
                }
            } else {
                drop(child);
                assert!(task.await.unwrap().is_err());
            }
        }
        let (mut parent, mut child) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move { authorize(&mut parent, b"fixture").await });
        assert_eq!(child.read_u32().await.unwrap(), 7);
        let mut bytes = [0; 7];
        child.read_exact(&mut bytes).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(child.read_u8().await.is_err());
    }

    #[tokio::test]
    async fn final_provider_authorization_is_after_join_and_before_go() {
        // B24 JOINED + live final authorization => one GO. B25 JOINED +
        // expired/revoked authorization => EOF and no GO. B26 wrong ACK =>
        // authorization callback is never evaluated. These 2x2 rows pin the
        // production last-mile gate, not a parallel model of lease validity.
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        for joined in [false, true] {
            for authorized in [false, true] {
                let called = Arc::new(AtomicBool::new(false));
                let observed = called.clone();
                let (mut parent, mut child) = UnixStream::pair().unwrap();
                let task = tokio::spawn(async move {
                    authorize_checked(&mut parent, b"", |grant| {
                        called.store(true, Ordering::SeqCst);
                        if authorized {
                            grant.commit()
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "expired fixture",
                            ))
                        }
                    })
                    .await
                });
                assert_eq!(child.read_u32().await.unwrap(), 0);
                assert!(!observed.load(Ordering::SeqCst));
                child
                    .write_u8(if joined { JOINED } else { 99 })
                    .await
                    .unwrap();
                let result = task.await.unwrap();
                assert_eq!(result.is_ok(), joined && authorized);
                assert_eq!(observed.load(Ordering::SeqCst), joined);
                let grant = child.read_u8().await;
                if joined && authorized {
                    assert_eq!(grant.unwrap(), GO);
                } else {
                    assert!(grant.is_err());
                }
            }
        }
    }

    #[tokio::test]
    async fn grant_result_tracks_the_irreversible_write_not_callback_status() {
        // Decision table: G1 omitted commit + Ok => deny/no GO; G2 committed
        // GO + adapter error => preserve the launched result (and its handle);
        // G3 retry + renewed authorization => one GO; G4 retry + revocation
        // => deny/no GO. WouldBlock never reuses an earlier authorization.
        for rule in 1..=4 {
            let (mut parent, mut child) = UnixStream::pair().unwrap();
            let task = tokio::spawn(async move {
                let mut calls = 0;
                let result = authorize_checked(&mut parent, b"", |grant| {
                    calls += 1;
                    match (rule, calls) {
                        (1, _) => Ok(()),
                        (2, _) => {
                            grant.commit()?;
                            Err(io::Error::other("failure after irreversible GO"))
                        }
                        (3 | 4, 1) => Err(io::ErrorKind::WouldBlock.into()),
                        (3, _) => grant.commit(),
                        (4, _) => Err(io::ErrorKind::PermissionDenied.into()),
                        _ => unreachable!(),
                    }
                })
                .await;
                (result, calls)
            });
            assert_eq!(child.read_u32().await.unwrap(), 0);
            child.write_u8(JOINED).await.unwrap();
            let (result, calls) = task.await.unwrap();
            assert_eq!(calls, if rule >= 3 { 2 } else { 1 });
            assert_eq!(result.is_ok(), matches!(rule, 2 | 3));
            match result {
                Ok(()) => assert_eq!(child.read_u8().await.unwrap(), GO),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
            }
            assert!(child.read_u8().await.is_err(), "no extra grant for G{rule}");
        }
    }

    #[tokio::test]
    async fn grant_can_be_committed_under_the_same_cross_thread_authority_guard() {
        // G5 JOINED + live authority locked by callback + concurrent revoker:
        // the revoker MUST observe contention until GO is committed. A mere
        // check-then-return callback would release this guard before GO. This
        // checks the production grant seam, not a Session lease simulation.
        use std::sync::{Arc, Mutex, TryLockError, mpsc};
        let authority = Arc::new(Mutex::new(true));
        let revoker_authority = authority.clone();
        let (start_tx, start_rx) = mpsc::sync_channel(1);
        let (checked_tx, checked_rx) = mpsc::sync_channel(1);
        let revoker = std::thread::spawn(move || {
            start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                revoker_authority.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            checked_tx.send(()).unwrap();
            *revoker_authority.lock().unwrap() = false;
        });
        let (mut parent, mut child) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            authorize_checked(&mut parent, b"", move |grant| {
                let live = authority.lock().unwrap();
                assert!(*live);
                start_tx.send(()).unwrap();
                checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let result = grant.commit();
                assert!(*live, "revocation cannot interleave before GO");
                result
            })
            .await
        });
        assert_eq!(child.read_u32().await.unwrap(), 0);
        child.write_u8(JOINED).await.unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(child.read_u8().await.unwrap(), GO);
        revoker.join().unwrap();
    }

    #[tokio::test]
    async fn repeated_grant_backpressure_remains_cancellable() {
        // G6 repeated WouldBlock + deadline: no GO and timeout completes even
        // with writable socket readiness. Guards are not retained over retry.
        let (mut parent, mut child) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_millis(100),
                authorize_checked(&mut parent, b"", |_grant| {
                    Err(io::ErrorKind::WouldBlock.into())
                }),
            )
            .await
        });
        assert_eq!(child.read_u32().await.unwrap(), 0);
        child.write_u8(JOINED).await.unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(child.read_u8().await.is_err());
    }

    #[tokio::test]
    async fn joined_bootstrap_requires_go_and_never_treats_disconnect_as_grant() {
        // B22 helper has joined + correct GO => exec may proceed; wrong GO or
        // owner disconnect AFTER JOINED => error, not exec authorization. The
        // paired-socket test pins the production child-side grant gate itself;
        // Linux integration separately qualifies the preceding actual join.
        for grant in [Some(GO), Some(JOINED), None] {
            let (mut parent, mut child) = UnixStream::pair().unwrap();
            let task = tokio::spawn(async move { await_grant(&mut child).await });
            assert_eq!(parent.read_u8().await.unwrap(), JOINED);
            if let Some(grant) = grant {
                parent.write_u8(grant).await.unwrap();
            }
            drop(parent);
            assert_eq!(task.await.unwrap().is_ok(), grant == Some(GO));
        }
    }

    #[tokio::test]
    async fn bootstrap_rejects_bad_frames_without_joining_or_acknowledging() {
        // B23 exact parent but zero/oversized/truncated/invalid frame => bounded
        // rejection before opening or joining a scope; no JOINED, no plaintext
        // diagnostic. This drives the actual bootstrap receive path, not just
        // the parser. All inputs are invalid before any membership operation.
        for (size, payload) in [
            (0, &b""[..]),
            (MAX_FRAME as u32 + 1, &b""[..]),
            (32, &b"test-only-secret"[..]),
            (5, &b"WRONG"[..]),
        ] {
            let name = format!(
                "awaken-frame-test-{}-{}",
                std::process::id(),
                NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
            );
            let listener = StdListener::bind_addr(&address(&name).unwrap()).unwrap();
            listener.set_nonblocking(true).unwrap();
            let listener = UnixListener::from_std(listener).unwrap();
            let task = tokio::spawn(async move { bootstrap(&name, std::process::id()).await });
            let (mut control, _) = listener.accept().await.unwrap();
            control.write_u32(size).await.unwrap();
            control.write_all(payload).await.unwrap();
            control.shutdown().await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert!(!error.to_string().contains("test-only-secret"));
            assert!(matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof
            ));
            assert!(control.read_u8().await.is_err());
        }
    }
}
