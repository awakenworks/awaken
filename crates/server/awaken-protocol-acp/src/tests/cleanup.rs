//! Exit evidence remains independent of terminal protocol bytes.
use crate::*;
use pc::ProcessHandle as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

struct OutputBoundProcess(Arc<AtomicBool>);
#[async_trait]
impl pc::ProcessHandle for OutputBoundProcess {
    fn id(&self) -> &str {
        "output-bound-process"
    }
    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(self.0.load(Ordering::SeqCst).then_some(pc::ExitStatus {
            code: Some(7),
            signaled: false,
        }))
    }
    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        loop {
            if let Some(status) = self.poll().await? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    async fn signal(&self, _: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[tokio::test]
async fn cleanup_drains_trailing_output_but_never_infers_exit_from_eof() {
    // Causes: C1 terminal/cancel/injected-interrupt/deadline selects cleanup;
    // C2 64KiB trailing bytes saturate a 64-byte channel before the transport can
    // deliver exit=7; C3 EOF arrives without process evidence. Effects: E1 the
    // existing reap completes with real exit while bytes are discarded; E2 no
    // trailing fact is committed; E3 EOF alone still fails its cleanup deadline.
    // D8 each C1+C2 => E1/E2. D9 terminal+C3 => E2/E3, never fabricated ACK.
    // This models Kubernetes' stdout/status coupling without replacing its
    // production process handle or treating an independent channel close as exit.
    for mode in ["terminal", "cancel", "interrupt", "deadline", "eof-no-exit"] {
        let (mut channel, mut peer) = tokio::io::duplex(64);
        let exited = Arc::new(AtomicBool::new(false));
        let process = OutputBoundProcess(exited.clone());
        let remote = tokio::spawn(async move {
            if mode != "eof-no-exit" {
                peer.write_all(&vec![b'x'; 64 * 1024]).await.unwrap();
                exited.store(true, Ordering::SeqCst);
            }
            peer.shutdown().await.unwrap();
        });
        let grace = Duration::from_millis(50);
        if matches!(mode, "terminal" | "eof-no-exit") {
            let result = Supervisor::reap_after_terminal(&mut channel, &process, grace).await;
            if mode == "terminal" {
                assert!(result.is_ok(), "D8 {result:?}");
                assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
            } else {
                assert!(result.is_err(), "D9 EOF is not exit evidence");
            }
        } else {
            let (send, mut receive) = tokio::sync::mpsc::channel(1);
            if mode == "interrupt" {
                send.send(Injection::Interrupt).await.unwrap();
            }
            let cancel = async {
                if mode != "cancel" {
                    std::future::pending::<()>().await;
                }
            };
            let policy = SupervisePolicy {
                turn_deadline: Some(Duration::from_millis(1)),
                reap_grace: grace,
            };
            let mut sink = crate::tests::RecordingSink::default();
            let result = Supervisor::supervise(
                &mut channel,
                &process,
                &"p".repeat(64 * 1024),
                &mut sink,
                cancel,
                &mut receive,
                policy,
                Codec::Newline,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                result,
                if mode == "deadline" {
                    TerminationReason::TimedOut
                } else {
                    TerminationReason::Cancelled
                }
            );
            assert!(sink.events.is_empty(), "D8 no trailing bytes become facts");
            assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
        }
        remote.await.unwrap();
    }
}

#[tokio::test]
async fn signal_errors_require_later_exit_evidence_within_the_same_reap_budget() {
    // D10 TERM fails + a later poll proves exit => success without KILL.
    // D11 TERM fails + no exit => error, no fabricated receipt or escalation.
    // D12 TERM succeeds, KILL fails + wait proves exit => success.
    // D13 KILL fails + wait hangs/errors => bounded error. A successful signal
    // alone is never evidence. Both terminal and cancellation cleanup reuse
    // this ladder; no backend-specific ESRCH text is interpreted as success.
    struct SignalRace {
        failure: pc::Signal,
        evidence: &'static str,
        polls: std::sync::atomic::AtomicUsize,
        signals: std::sync::Mutex<Vec<pc::Signal>>,
    }
    #[async_trait]
    impl pc::ProcessHandle for SignalRace {
        fn id(&self) -> &str {
            "same-owned-process"
        }
        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            let signalled = !self.signals.lock().unwrap().is_empty();
            let ready = signalled
                && self.evidence == "poll"
                && self.polls.fetch_add(1, Ordering::SeqCst) >= 2;
            Ok(ready.then_some(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            }))
        }
        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            match self.evidence {
                "wait" => Ok(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                }),
                "error" => Err(pc::SandboxError::new("exit evidence unavailable")),
                _ => std::future::pending().await,
            }
        }
        async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
            self.signals.lock().unwrap().push(signal);
            if signal == self.failure {
                Err(pc::SandboxError::new("signal failed"))
            } else {
                Ok(())
            }
        }
    }
    for (failure, evidence, expected) in [
        (pc::Signal::Term, "poll", Some(false)),
        (pc::Signal::Term, "none", None),
        (pc::Signal::Kill, "wait", Some(true)),
        (pc::Signal::Kill, "none", None),
        (pc::Signal::Kill, "error", None),
    ] {
        let process = SignalRace {
            failure,
            evidence,
            polls: Default::default(),
            signals: Default::default(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            Supervisor::reap(&process, Duration::from_millis(20)),
        )
        .await
        .expect("D10-D13 remain bounded");
        assert_eq!(result.ok(), expected, "{failure:?}/{evidence}");
        assert_eq!(
            *process.signals.lock().unwrap(),
            if failure == pc::Signal::Term {
                vec![pc::Signal::Term]
            } else {
                vec![pc::Signal::Term, pc::Signal::Kill]
            }
        );
    }
}
