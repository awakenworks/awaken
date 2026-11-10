//! Current grant at each real projector-channel dispatch, and in-target UID admission.
use super::*;
use awaken_provisioning_contract as pc;
use pc::test_support::{EffectAuthority, FenceTamper as _};
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    WriteReady,
    WritePending,
    ShutdownPending,
    ReadReady,
    ReadPending,
}

#[derive(Default)]
struct Trace {
    writes: usize,
    reads: usize,
    shutdowns: usize,
    bytes: Vec<u8>,
}

struct Channel {
    cut: Cut,
    cut_done: bool,
    read_sent: bool,
    first: Option<Box<dyn FnOnce()>>,
    fault: Option<&'static str>,
    trace: Arc<Mutex<Trace>>,
}

impl AsyncWrite for Channel {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.trace.lock().unwrap().writes += 1;
        if this.fault == Some("write") {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        if this.fault == Some("zero") {
            return Poll::Ready(Ok(0));
        }
        if !this.cut_done && matches!(this.cut, Cut::WriteReady | Cut::WritePending) {
            this.cut_done = true;
            this.first.take().unwrap()();
            if this.cut == Cut::WritePending {
                return Poll::Pending;
            }
            this.trace.lock().unwrap().bytes.push(bytes[0]);
            return Poll::Ready(Ok(1));
        }
        this.trace.lock().unwrap().bytes.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.trace.lock().unwrap().shutdowns += 1;
        if this.fault == Some("shutdown") {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        if !this.cut_done && this.cut == Cut::ShutdownPending {
            this.cut_done = true;
            this.first.take().unwrap()();
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Channel {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.trace.lock().unwrap().reads += 1;
        if this.fault == Some("read") {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        if !this.cut_done && this.cut == Cut::ReadPending {
            this.cut_done = true;
            this.first.take().unwrap()();
            return Poll::Pending;
        }
        if !this.read_sent {
            this.read_sent = true;
            bytes.put_slice(b"r");
            if !this.cut_done && this.cut == Cut::ReadReady {
                this.cut_done = true;
                this.first.take().unwrap()();
            }
        }
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn every_projector_channel_dispatch_rechecks_the_original_grant() {
    // C1 partial Ready vs actual Pending at write/read, or Pending shutdown;
    // C2 revoked/higher epoch/expiry/renewal/drop. Only renewal succeeds.
    // Ready-partial can execute several leaf writes in ONE outer future poll:
    // R1 change after its first dispatch -> exactly one leaf call, no suffix or
    // shutdown. R2 Pending followed by stale grant -> no resumed leaf dispatch.
    // R3 drop is possible only at Pending and dispatches nothing afterwards.
    // R4 same-tenure renewal completes the unchanged ordered payload/output.
    // These are the actual I/O functions used by exec, not a write_all wrapper.
    for cut in [
        Cut::WriteReady,
        Cut::WritePending,
        Cut::ShutdownPending,
        Cut::ReadReady,
        Cut::ReadPending,
    ] {
        for change in ["revoke", "replace", "expire", "renew", "cancel"] {
            let pending_cut = matches!(
                cut,
                Cut::WritePending | Cut::ShutdownPending | Cut::ReadPending
            );
            if change == "cancel" && !pending_cut {
                continue;
            }
            let fence =
                pc::SandboxEffectFence::new("projector", "owner", "boot", 1, u64::MAX - 1).unwrap();
            let authority = Arc::new(EffectAuthority::new(&fence));
            let current = authority.clone();
            let mut successor = fence.clone();
            let trace = Arc::new(Mutex::new(Trace::default()));
            let mut channel = Channel {
                cut,
                cut_done: false,
                read_sent: false,
                fault: None,
                trace: trace.clone(),
                first: Some(Box::new(move || match change {
                    "revoke" => current.set(None),
                    "replace" => {
                        successor.reassign();
                        current.set(Some((successor, Duration::from_secs(1))));
                    }
                    "expire" => current.set(Some((successor, Duration::ZERO))),
                    "renew" => {
                        successor.renew();
                        current.set(Some((successor, Duration::from_secs(1))));
                    }
                    _ => {}
                })),
            };
            let checkpoint = || {
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: authority.as_ref(),
                }
                .admit()
                .map_err(|_| RuntimeError::EffectFenceExpired)
            };
            let read = matches!(cut, Cut::ReadReady | Cut::ReadPending);
            let mut operation = Box::pin(async {
                if read {
                    assert_eq!(
                        read_projector_output(&mut channel, &checkpoint).await?,
                        b"r"
                    );
                } else {
                    write_projector_input(&mut channel, b"payload", &checkpoint).await?;
                }
                Ok::<(), RuntimeError>(())
            });
            if pending_cut {
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(operation.as_mut(), cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            if change == "cancel" {
                drop(operation);
            } else {
                let result = operation.await;
                if change == "renew" {
                    assert!(result.is_ok(), "{cut:?}/{change}");
                } else {
                    assert!(
                        matches!(result, Err(RuntimeError::EffectFenceExpired)),
                        "{cut:?}/{change}"
                    );
                }
            }
            let trace = trace.lock().unwrap();
            if change == "renew" && !read {
                assert_eq!(trace.bytes, b"payload");
            }
            if change != "renew" {
                assert_eq!(
                    trace.writes,
                    usize::from(!read),
                    "no next write/{cut:?}/{change}"
                );
                assert_eq!(
                    trace.shutdowns,
                    usize::from(cut == Cut::ShutdownPending),
                    "no next shutdown/{cut:?}/{change}"
                );
                assert_eq!(
                    trace.reads,
                    usize::from(read),
                    "no next read/{cut:?}/{change}"
                );
            }
        }
    }
}

#[tokio::test]
async fn projector_channel_io_errors_keep_the_existing_backend_class() {
    // R5 raw write/read/shutdown BrokenPipe or a zero-length write stays an
    // error in the existing backend vocabulary, not success or a fence error.
    // No extra channel dispatch or compensating cleanup follows that failure.
    for fault in ["write", "zero", "shutdown", "read"] {
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut channel = Channel {
            cut: Cut::WriteReady,
            cut_done: true,
            read_sent: false,
            first: None,
            fault: Some(fault),
            trace: trace.clone(),
        };
        let result = if fault == "read" {
            read_projector_output(&mut channel, &|| Ok(()))
                .await
                .map(|_| ())
        } else {
            write_projector_input(&mut channel, b"payload", &|| Ok(())).await
        };
        assert!(
            matches!(result, Err(RuntimeError::Backend(_))),
            "R5/{fault}"
        );
        let trace = trace.lock().unwrap();
        assert_eq!(trace.reads, usize::from(fault == "read"));
        assert_eq!(trace.writes, usize::from(fault != "read"));
        assert_eq!(trace.shutdowns, usize::from(fault == "shutdown"));
    }
}

#[cfg(unix)]
#[test]
fn projector_uid_prelude_refuses_wrong_or_missing_uid_before_filesystem_mutation() {
    // U1 actual Downward API UID equals the acquired argv -> unchanged payload
    // script may write. U2 same-name replacement copies labels but its actual
    // UID differs; U3 historical projector omits attestation -> exit 72 with
    // zero mkdir/write. The real shell executes the production prelude; only
    // the safe UID-independent test mutation is supplied as the original body.
    for actual in [Some("original-pod-uid"), Some("replacement-pod-uid"), None] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("mutation");
        let argv = uid_bound_command(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "mkdir -p \"$1\"; printf payload > \"$1/result\"".into(),
                "projector-uid-fixture".into(),
                target.to_string_lossy().into_owned(),
            ],
            Some("original-pod-uid"),
        )
        .unwrap();
        let mut shell = std::process::Command::new(&argv[0]);
        shell
            .args(&argv[1..])
            .env_remove(super::super::projection::PROJECTOR_UID_ENV);
        if let Some(actual) = actual {
            shell.env(super::super::projection::PROJECTOR_UID_ENV, actual);
        }
        let status = shell.status().unwrap();
        if actual == Some("original-pod-uid") {
            assert!(status.success());
            assert_eq!(std::fs::read(target.join("result")).unwrap(), b"payload");
        } else {
            assert_eq!(status.code(), Some(72));
            assert!(!target.exists(), "U2/U3 zero filesystem mutation");
        }
    }
}
