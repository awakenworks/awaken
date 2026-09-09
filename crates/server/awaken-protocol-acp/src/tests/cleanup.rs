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
    // Causes: C1 exit already observed; C2 TERM succeeds/fails; C3 a grace poll
    // observes exit; C4 KILL succeeds/fails; C5 wait observes exit/errors/hangs;
    // C6 a signal RPC hangs; C7 terminal/cancellation entry. C1 and C3 are
    // exclusive evidence classes. Decision rules: D10=C1 -> no signals, success;
    // D11=!C1+C3 -> TERM only, success independent of C2;
    // D12=!C1+!C3+exit-at-wait -> TERM/KILL, success independent of C2/C4;
    // D13=!C1+!C3+wait-error/hang -> TERM/KILL, bounded failure;
    // D14=C6 -> total deadline, failure, no invented exit. A failed TERM must
    // not suppress the authorized KILL stage for that same process. Conversely
    // no signal result, channel EOF, or deadline can manufacture an ACK.
    // Cross both entries, four TERM/KILL result pairs and seven evidence/RPC
    // classes: 56 cases. Irrelevant later failures remain configured in early
    // exit cases to detect accidental escalation. No provider error text is
    // interpreted as successful termination.
    struct SignalRace {
        failures: u8,
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
            let ready = self.evidence == "initial"
                || (signalled
                    && self.evidence == "poll"
                    && self.polls.fetch_add(1, Ordering::SeqCst) >= 2);
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
            if (signal == pc::Signal::Term && self.evidence == "term-hang")
                || (signal == pc::Signal::Kill && self.evidence == "kill-hang")
            {
                return std::future::pending().await;
            }
            let bit = if signal == pc::Signal::Term { 1 } else { 2 };
            if self.failures & bit != 0 {
                Err(pc::SandboxError::new("signal failed"))
            } else {
                Ok(())
            }
        }
    }
    for ((terminal, failures), evidence) in [false, true]
        .into_iter()
        .flat_map(|terminal| (0..4).map(move |failures| (terminal, failures)))
        .flat_map(|case| {
            [
                "initial",
                "poll",
                "wait",
                "none",
                "error",
                "term-hang",
                "kill-hang",
            ]
            .map(|evidence| (case, evidence))
        })
    {
        let process = SignalRace {
            failures,
            evidence,
            polls: Default::default(),
            signals: Default::default(),
        };
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let grace = Duration::from_millis(20);
            if terminal {
                let (mut channel, _peer) = tokio::io::duplex(64);
                Supervisor::reap_after_terminal(&mut channel, &process, grace).await
            } else {
                Supervisor::reap(&process, grace).await
            }
        })
        .await
        .expect("D10-D14 remain bounded");
        let expected = match evidence {
            "initial" | "poll" => Some(false),
            "wait" => Some(true),
            _ => None,
        };
        assert_eq!(result.ok(), expected, "{terminal}/{failures}/{evidence}");
        assert_eq!(
            *process.signals.lock().unwrap(),
            match evidence {
                "initial" => vec![],
                "poll" | "term-hang" => vec![pc::Signal::Term],
                _ => vec![pc::Signal::Term, pc::Signal::Kill],
            },
            "{terminal}/{failures}/{evidence}"
        );
    }
}
