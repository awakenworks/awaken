// @formal evidence sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Single synchronous grant seam shared with the current Session authority guard.
use super::invalid;
use std::{cell::Cell, io};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
const JOINED: u8 = 1;
const GO: u8 = 2;
pub(super) const TERMINATE: u8 = 3;
pub(super) const INTERRUPT: u8 = 4;

/// A single control byte is atomic even with concurrent signal callers. A
/// successful send requests delivery only; it never certifies process exit.
pub(super) async fn send_signal(
    control: &UnixStream,
    writer: &std::sync::Mutex<()>,
    byte: u8,
) -> io::Result<()> {
    loop {
        control.writable().await?;
        let result = {
            let _guard = writer.lock().map_err(|_| invalid())?;
            write_complete(control, &[byte])
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
}

/// A short stream write is NOT an atomic frame. Retrying its suffix after an
/// authorization guard is released could mix generations or other signals.
/// Poison the channel instead; helper EOF fails closed without a new registry.
pub(super) fn write_complete(control: &UnixStream, frame: &[u8]) -> io::Result<()> {
    let written = control.try_write(frame)?;
    complete_write_result(written, frame.len(), || {
        let _ = rustix::net::shutdown(control, rustix::net::Shutdown::Both);
    })
}

fn complete_write_result(written: usize, expected: usize, poison: impl FnOnce()) -> io::Result<()> {
    if written == expected {
        Ok(())
    } else {
        poison();
        Err(io::ErrorKind::WriteZero.into())
    }
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
        write_complete(self.control, &[GO])?;
        self.committed.set(true);
        Ok(())
    }
}

pub(super) async fn authorize_checked(
    control: &mut UnixStream,
    frame: &[u8],
    mut authorize_effect: impl FnMut(ScopedLaunchGrant<'_>) -> io::Result<()>,
) -> io::Result<()> {
    control.write_u32(frame.len() as u32).await?;
    control.write_all(frame).await?;
    if control.read_u8().await? != JOINED {
        return Err(invalid());
    }
    commit_checked(control, |committed| {
        authorize_effect(ScopedLaunchGrant { control, committed })
    })
    .await
}

pub(super) async fn commit_checked(
    control: &UnixStream,
    mut authorize_effect: impl FnMut(&Cell<bool>) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        control.writable().await?;
        let committed = Cell::new(false);
        let result = authorize_effect(&committed);
        // Once a grant has been sent, a later adapter error cannot undo it.
        // For GO this also preserves the only live process handle.
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

pub(super) async fn await_grant(control: &mut UnixStream) -> io::Result<()> {
    control.write_u8(JOINED).await?;
    if control.read_u8().await? != GO {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn incomplete_frame_poisons_instead_of_retrying_a_suffix() {
        // W1 exact write => committed/no shutdown. W2 zero/partial/impossible
        // oversized result => WriteZero + shutdown exactly once. A suffix
        // retry outside the authority lock could mix generations or signals.
        for written in [0, 1, 8, 9, 10] {
            let poisoned = Cell::new(0);
            let result = complete_write_result(written, 9, || poisoned.set(poisoned.get() + 1));
            assert_eq!(result.is_ok(), written == 9);
            assert_eq!(poisoned.get(), usize::from(written != 9));
            if let Err(error) = result {
                assert_eq!(error.kind(), io::ErrorKind::WriteZero);
            }
        }
    }

    #[tokio::test]
    async fn real_short_write_shuts_down_the_channel_but_backpressure_does_not() {
        // W3 a real nonblocking stream accepts only a prefix => WriteZero,
        // queued prefix then EOF, even while sender is still owned. W4 a full
        // buffer => WouldBlock with ZERO bytes from this frame; draining makes
        // a fresh complete frame possible. No synthetic short-write result is
        // used as evidence of actual kernel channel shutdown.
        let (parent, mut child) = UnixStream::pair().unwrap();
        rustix::net::sockopt::set_socket_send_buffer_size(&parent, 4096).unwrap();
        parent.writable().await.unwrap();
        let frame = vec![7; 1024 * 1024];
        assert_eq!(
            write_complete(&parent, &frame).unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        let mut prefix = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), child.read_to_end(&mut prefix))
            .await
            .unwrap()
            .unwrap();
        assert!(!prefix.is_empty() && prefix.len() < frame.len());
        assert!(prefix.iter().all(|b| *b == 7));
        drop(parent);

        let (parent, mut child) = UnixStream::pair().unwrap();
        rustix::net::sockopt::set_socket_send_buffer_size(&parent, 4096).unwrap();
        parent.writable().await.unwrap();
        let mut queued = 0;
        loop {
            match parent.try_write(&[8; 4096]) {
                Ok(n) => queued += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected fill result: {other:?}"),
            }
        }
        assert_eq!(
            write_complete(&parent, &[GO]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        child.read_exact(&mut vec![0; queued]).await.unwrap();
        parent.writable().await.unwrap();
        write_complete(&parent, &[GO]).unwrap();
        assert_eq!(child.read_u8().await.unwrap(), GO);
    }
    async fn authorize(control: &mut UnixStream, frame: &[u8]) -> io::Result<()> {
        authorize_checked(control, frame, |g| g.commit()).await
    }
    #[tokio::test]
    async fn private_control_requires_join_ack_before_go() {
        // B4 private inherited channel + JOINED => exactly one GO;
        // B6 wrong ACK/disconnect => no GO; B7 caller
        // cancellation while awaiting ACK => EOF, never a workload grant.
        for ack in [Some(JOINED), Some(99), None] {
            let (mut parent, mut child) = UnixStream::pair().unwrap();
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
}
