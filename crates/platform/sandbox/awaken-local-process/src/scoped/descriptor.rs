//! Private bootstrap descriptor handoff. Never a resource or process registry.
//!
//! A socket pair is inherited by the trusted launcher before workload code can
//! run. SCM_RIGHTS transfers handles, not caller-selected FD numbers or paths.
//! Received handles are CLOEXEC and are dropped on every malformed message.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::io::Interest;
use tokio::net::UnixStream;

const MAX_HANDLES: usize = 3;
const HANDLES: u8 = 0x46;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid bootstrap descriptor handoff",
    )
}

pub(super) async fn send(stream: &UnixStream, handles: &[OwnedFd]) -> io::Result<()> {
    if handles.is_empty() || handles.len() > MAX_HANDLES {
        return Err(invalid());
    }
    let borrowed = handles.iter().map(AsFd::as_fd).collect::<Vec<_>>();
    stream
        .async_io(Interest::WRITABLE, || {
            // Recreate ancillary storage on readiness retry; it is local to this
            // one syscall. No partial header can accidentally become a second send.
            let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_HANDLES))];
            let mut control = SendAncillaryBuffer::new(&mut storage);
            if !control.push(SendAncillaryMessage::ScmRights(&borrowed)) {
                return Err(invalid());
            }
            let sent = sendmsg(
                stream,
                &[IoSlice::new(&[HANDLES])],
                &mut control,
                SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
            )?;
            if sent != 1 {
                return Err(invalid());
            }
            Ok(())
        })
        .await
}

pub(super) async fn receive(stream: &UnixStream, count: usize) -> io::Result<Vec<OwnedFd>> {
    if count == 0 || count > MAX_HANDLES {
        return Err(invalid());
    }
    stream
        .async_io(Interest::READABLE, || {
            let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_HANDLES))];
            let mut control = RecvAncillaryBuffer::new(&mut storage);
            let mut byte = [0];
            let result = recvmsg(
                stream,
                &mut [IoSliceMut::new(&mut byte)],
                &mut control,
                RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
            )?;
            let mut handles = Vec::new();
            for message in control.drain() {
                match message {
                    RecvAncillaryMessage::ScmRights(rights) => handles.extend(rights),
                    _ => return Err(invalid()),
                }
            }
            if result.bytes != 1
                || byte[0] != HANDLES
                || result
                    .flags
                    .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
                || handles.len() != count
            {
                return Err(invalid());
            }
            Ok(handles)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn handoff_preserves_kernel_identity_and_never_inherits_received_handles() {
        // D1 exact 1/2/3 handles + exact framing => same kernel file identity,
        // CLOEXEC on every received descriptor, independent sender ownership.
        // Closing sender handles cannot invalidate the receiver's owned handles.
        for count in 1..=MAX_HANDLES {
            let file = tempfile::tempfile().unwrap();
            let expected = rustix::fs::fstat(&file).unwrap();
            let handles = (0..count)
                .map(|_| file.as_fd().try_clone_to_owned().unwrap())
                .collect::<Vec<_>>();
            let (sender, receiver) = UnixStream::pair().unwrap();
            send(&sender, &handles).await.unwrap();
            drop(handles);
            let received = receive(&receiver, count).await.unwrap();
            for fd in received {
                let actual = rustix::fs::fstat(&fd).unwrap();
                assert_eq!(
                    (actual.st_dev, actual.st_ino),
                    (expected.st_dev, expected.st_ino)
                );
                assert!(
                    rustix::io::fcntl_getfd(&fd)
                        .unwrap()
                        .contains(rustix::io::FdFlags::CLOEXEC)
                );
            }
        }
    }

    #[tokio::test]
    async fn handoff_rejects_wrong_count_unframed_bytes_disconnect_and_truncation() {
        // D2 count mismatch / no handles / wrong byte / EOF => deny. D3 too
        // many descriptors => MSG_CTRUNC/count failure, never a partial success.
        // D4 invalid requested counts reject without waiting for the peer.
        for actual in 0..=5 {
            let (mut sender, receiver) = UnixStream::pair().unwrap();
            if actual == 0 {
                sender.write_all(&[HANDLES]).await.unwrap();
            } else {
                let files = (0..actual)
                    .map(|_| File::open("/dev/null").unwrap())
                    .collect::<Vec<_>>();
                let borrowed = files.iter().map(AsFd::as_fd).collect::<Vec<_>>();
                let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(5))];
                let mut control = SendAncillaryBuffer::new(&mut storage);
                assert!(control.push(SendAncillaryMessage::ScmRights(&borrowed)));
                sendmsg(
                    &sender,
                    &[IoSlice::new(&[HANDLES])],
                    &mut control,
                    SendFlags::empty(),
                )
                .unwrap();
            }
            assert!(
                receive(&receiver, if actual == 1 { 2 } else { 1 })
                    .await
                    .is_err()
            );
        }
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(&[0]).await.unwrap();
        assert!(receive(&receiver, 1).await.is_err());
        drop(sender);
        assert!(receive(&receiver, 1).await.is_err());
        for count in [0, MAX_HANDLES + 1] {
            assert!(receive(&receiver, count).await.is_err());
        }
        assert!(send(&receiver, &[]).await.is_err());
    }

    #[tokio::test]
    async fn rejected_handoff_closes_every_received_descriptor() {
        // D5 wrong count / wrong marker / ancillary truncation => error AND
        // every transferred pipe writer closed. Reader EOF proves ownership
        // reclamation, without a racy process-wide /proc/self/fd count. Original
        // writers and sender are dropped before receipt; queued SCM_RIGHTS are
        // then the only writers. Covers both user-space and kernel truncation.
        use tokio::io::AsyncReadExt;
        for (count, marker) in [(1, HANDLES), (3, 0), (5, HANDLES)] {
            let (sender, receiver) = UnixStream::pair().unwrap();
            let mut readers = Vec::new();
            let mut writers = Vec::new();
            for _ in 0..count {
                let (writer, reader) = tokio::net::unix::pipe::pipe().unwrap();
                writers.push(writer.into_blocking_fd().unwrap());
                readers.push(reader);
            }
            let borrowed = writers.iter().map(AsFd::as_fd).collect::<Vec<_>>();
            let mut storage = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(5))];
            let mut control = SendAncillaryBuffer::new(&mut storage);
            assert!(control.push(SendAncillaryMessage::ScmRights(&borrowed)));
            sendmsg(
                &sender,
                &[IoSlice::new(&[marker])],
                &mut control,
                SendFlags::empty(),
            )
            .unwrap();
            drop(writers);
            drop(sender);
            assert!(receive(&receiver, 2).await.is_err());
            for mut reader in readers {
                let mut byte = [0];
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(1), reader.read(&mut byte))
                        .await
                        .expect("D5 no leaked writer")
                        .unwrap(),
                    0
                );
            }
        }
    }
}
