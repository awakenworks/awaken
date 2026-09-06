//! Existing Session authority may project a newer absolute OS cutoff. This
//! transport owns neither the lease nor its renewal/revocation schedule.
use super::{ExecutionDeadline, MAX_WINDOW, grant, invalid, timed_out};
use awaken_provisioning_contract::Signal;
use std::{cell::Cell, io, sync::Mutex, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::UnixStream,
};

const REFRESH: u8 = 5;

/// One-use cutoff write, called while holding the same current-authority guard
/// as GO/revocation. A precomputed bool or a remembered lease is insufficient.
pub struct ScopedDeadlineGrant<'a> {
    control: &'a UnixStream,
    writer: &'a Mutex<()>,
    committed: &'a Cell<bool>,
}

impl<'a> ScopedDeadlineGrant<'a> {
    pub(super) fn new(
        control: &'a UnixStream,
        writer: &'a Mutex<()>,
        committed: &'a Cell<bool>,
    ) -> Self {
        Self {
            control,
            writer,
            committed,
        }
    }

    pub fn commit(self, deadline: ExecutionDeadline) -> io::Result<()> {
        deadline.remaining()?;
        let mut frame = [0; 9];
        frame[0] = REFRESH;
        frame[1..].copy_from_slice(&deadline.0.to_be_bytes());
        let _guard = self.writer.lock().map_err(|_| invalid())?;
        grant::write_complete(self.control, &frame)?;
        self.committed.set(true);
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ControlMessage {
    Signal(Signal),
    Deadline(u64),
}

async fn receive(input: &mut (impl AsyncRead + Unpin)) -> io::Result<ControlMessage> {
    match input.read_u8().await? {
        grant::TERMINATE => Ok(ControlMessage::Signal(Signal::Term)),
        grant::INTERRUPT => Ok(ControlMessage::Signal(Signal::Int)),
        REFRESH => Ok(ControlMessage::Deadline(input.read_u64().await?)),
        _ => Err(invalid()),
    }
}

/// Keep the SAME partial-read future across clock ticks. Cancelling read_u64
/// and starting again could reinterpret its tail as a new frame. BOOTTIME is
/// checked while a peer sends nothing or stalls half way through the payload.
pub(super) async fn receive_before_cutoff(
    input: &mut (impl AsyncRead + Unpin),
    deadline: ExecutionDeadline,
) -> io::Result<ControlMessage> {
    let read = receive(input);
    tokio::pin!(read);
    loop {
        let remaining = deadline.remaining()?;
        tokio::select! {
            message = &mut read => return message,
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(100))) => {}
        }
    }
}

/// Helper-side linearization: expiry is checked BEFORE inspecting a newer
/// message. Stale/equal messages cannot shorten the live cutoff or resurrect
/// an expired scope. Absolute BOOTTIME prevents transport delay minting time.
pub(super) fn apply_deadline(current: u64, incoming: u64, now: u64) -> io::Result<u64> {
    if current <= now {
        return Err(timed_out());
    }
    if incoming <= current {
        return Ok(current);
    }
    if incoming - now > MAX_WINDOW.as_millis() as u64 {
        return Err(invalid());
    }
    Ok(incoming)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn deadline_decision_table_never_mints_time_or_revives_expiry() {
        // Causes: current live/expired, incoming older/equal/newer, maximum
        // window, delayed delivery. Effects: retain, extend EXACT absolute
        // cutoff, reject. R1-R3 live + stale/equal/new => max(current,incoming);
        // R4-R6 expired + any incoming => timeout (expiry has precedence).
        // R7 max allowed => accept; R8 beyond max/u64::MAX => invalid.
        // R9 advancing receive time does not add transport latency to cutoff.
        let window = MAX_WINDOW.as_millis() as u64;
        for (current, incoming, now, expected) in [
            (200, 99, 100, Ok(200)),
            (200, 200, 100, Ok(200)),
            (200, 300, 100, Ok(300)),
            (200, 99, 200, Err(io::ErrorKind::TimedOut)),
            (200, 200, 201, Err(io::ErrorKind::TimedOut)),
            (200, u64::MAX, 200, Err(io::ErrorKind::TimedOut)),
            (200, 100 + window, 100, Ok(100 + window)),
            (200, 101 + window, 100, Err(io::ErrorKind::InvalidInput)),
            (200, u64::MAX, 100, Err(io::ErrorKind::InvalidInput)),
            (200, 300, 199, Ok(300)),
        ] {
            assert_eq!(
                apply_deadline(current, incoming, now).map_err(|e| e.kind()),
                expected
            );
        }
    }

    #[tokio::test]
    async fn fragmented_frame_survives_clock_ticks_without_reinterpretation() {
        // R10 a renewal split across multiple 100ms BOOTTIME polls => exact
        // frame then exact next signal. Restarting a partially completed
        // read_u64 on each tick would lose consumed bytes and desynchronize.
        let (mut writer, mut reader) = tokio::io::duplex(32);
        let cutoff = ExecutionDeadline::after(Duration::from_secs(3)).unwrap();
        let sending = async {
            writer.write_all(&[REFRESH, 0, 0]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            writer.write_all(&[0, 0, 0]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            writer
                .write_all(&[0, 1, 2, grant::INTERRUPT])
                .await
                .unwrap();
        };
        let receiving = async {
            assert_eq!(
                receive_before_cutoff(&mut reader, cutoff).await.unwrap(),
                ControlMessage::Deadline(258)
            );
            assert_eq!(
                receive_before_cutoff(&mut reader, cutoff).await.unwrap(),
                ControlMessage::Signal(Signal::Int)
            );
        };
        tokio::join!(sending, receiving);
    }

    #[tokio::test]
    async fn stalled_partial_frame_cannot_disable_cutoff() {
        // R11 peer still alive + no bytes or each proper renewal prefix =>
        // original cutoff times out. Holding the writer prevents EOF from
        // accidentally satisfying the test; runtime timeout is a backstop,
        // not the expiry oracle. Includes opcode-only and 7/8 payload bytes.
        let mut frame = vec![REFRESH];
        frame.extend(123_u64.to_be_bytes());
        for prefix in 0..frame.len() {
            let (mut writer, mut reader) = tokio::io::duplex(32);
            writer.write_all(&frame[..prefix]).await.unwrap();
            let cutoff = ExecutionDeadline::after(Duration::from_millis(30)).unwrap();
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                receive_before_cutoff(&mut reader, cutoff),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "prefix {prefix}");
            drop(writer);
        }
    }

    #[tokio::test]
    async fn eof_malformed_and_already_expired_frames_fail_closed() {
        // R12 EOF at each proper prefix => UnexpectedEof, not renewal.
        // R13 unknown opcode => InvalidInput; R14 fully queued renewal after
        // cutoff => TimedOut even when input is immediately readable.
        let mut frame = vec![REFRESH];
        frame.extend(123_u64.to_be_bytes());
        for prefix in 0..frame.len() {
            let mut input = &frame[..prefix];
            assert_eq!(
                receive(&mut input).await.unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        for opcode in [0, 1, 2, 6, 255] {
            assert_eq!(
                receive(&mut [opcode].as_slice()).await.unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            receive_before_cutoff(&mut frame.as_slice(), ExecutionDeadline(0))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn renewal_uses_the_canonical_fresh_authorization_receipt() {
        // R15 omitted grant => deny; R16 committed + callback error => sent;
        // R17 WouldBlock then live => one frame; R18 WouldBlock then revoked
        // => no frame; R19 expired deadline => no frame. This checks the
        // shared grant transport, NOT a substituted Session lease authority.
        for rule in 15..=19 {
            let (parent, mut child) = UnixStream::pair().unwrap();
            let writer = Mutex::new(());
            let mut calls = 0;
            let deadline = ExecutionDeadline::after(Duration::from_secs(5)).unwrap();
            let result = grant::commit_checked(&parent, |committed| {
                calls += 1;
                let grant = ScopedDeadlineGrant::new(&parent, &writer, committed);
                match (rule, calls) {
                    (15, _) => Ok(()),
                    (16, _) => {
                        grant.commit(deadline)?;
                        Err(io::Error::other("after send"))
                    }
                    (17 | 18, 1) => Err(io::ErrorKind::WouldBlock.into()),
                    (17, _) => grant.commit(deadline),
                    (18, _) => Err(io::ErrorKind::PermissionDenied.into()),
                    (19, _) => grant.commit(ExecutionDeadline(0)),
                    _ => unreachable!(),
                }
            })
            .await;
            assert_eq!(calls, if matches!(rule, 17 | 18) { 2 } else { 1 });
            drop(parent);
            if matches!(rule, 16 | 17) {
                result.unwrap();
                assert_eq!(
                    receive(&mut child).await.unwrap(),
                    ControlMessage::Deadline(deadline.0)
                );
            } else {
                assert_eq!(
                    result.unwrap_err().kind(),
                    if rule == 19 {
                        io::ErrorKind::TimedOut
                    } else {
                        io::ErrorKind::PermissionDenied
                    }
                );
            }
            assert_eq!(
                child.read_u8().await.unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_signal_and_renewal_frames_are_not_interleaved() {
        // R20 independent threads send signals and multi-byte renewals through
        // ONE writer lock. Effects: 32 exact frames of each kind, no duplicate,
        // corruption or deadlock. Business stdio is never the control channel.
        let (parent, mut child) = UnixStream::pair().unwrap();
        let parent = std::sync::Arc::new(parent);
        let writer = std::sync::Arc::new(Mutex::new(()));
        let cutoff = ExecutionDeadline::after(Duration::from_secs(5)).unwrap();
        let signals = {
            let parent = parent.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                for _ in 0..32 {
                    grant::send_signal(&parent, &writer, grant::TERMINATE)
                        .await
                        .unwrap();
                }
            })
        };
        let renewals = tokio::spawn(async move {
            for offset in 0..32 {
                grant::commit_checked(&parent, |committed| {
                    ScopedDeadlineGrant::new(&parent, &writer, committed)
                        .commit(ExecutionDeadline(cutoff.0 + offset))
                })
                .await
                .unwrap();
            }
        });
        let mut signals_seen = 0;
        let mut deadlines = std::collections::BTreeSet::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            for _ in 0..64 {
                match receive(&mut child).await.unwrap() {
                    ControlMessage::Signal(Signal::Term) => signals_seen += 1,
                    ControlMessage::Deadline(value) => assert!(deadlines.insert(value)),
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
            signals.await.unwrap();
            renewals.await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(signals_seen, 32);
        assert_eq!(deadlines, (cutoff.0..cutoff.0 + 32).collect());
    }
}
