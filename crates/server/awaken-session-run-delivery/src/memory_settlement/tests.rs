use super::*;
use crate::{AdvisorModelSource, SessionCommitSource};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::terminal::RunTerminalObserverError;
use awaken_session_types::RunError;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Source {
    commit: Arc<SessionCommit>,
    unavailable: bool,
    partitions: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SessionCommitSource for Source {
    async fn open_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError> {
        self.partitions.lock().unwrap().push(session.into());
        if self.unavailable {
            Err(RunError::unavailable("commit source unavailable"))
        } else {
            Ok(self.commit.clone())
        }
    }

    async fn durable_thread_exists(&self, _: &str) -> Result<bool, RunError> {
        unreachable!("terminal observation only resolves the selected commit")
    }
}

struct NoModels;
impl AdvisorModelSource for NoModels {
    fn advisor_model(&self, _: &str) -> Result<Option<String>, RunError> {
        unreachable!("terminal observation never materializes a model")
    }
}

fn fixture(unavailable: bool) -> (Arc<Source>, CommittedReads, RunDispatch, RunClaim) {
    let source = Arc::new(Source {
        commit: Arc::new(SessionCommit::Local(Arc::new(
            awaken_dispatch_erasure::LocalCommitAdapter::projected(
                awaken_store_inmem::MemoryCommitCoordinator::new(awaken_time::system_clock()),
            ),
        ))),
        unavailable,
        partitions: Mutex::new(Vec::new()),
    });
    let reads = CommittedReads::new(source.clone(), Arc::new(NoModels));
    let request = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        RunId("child-run".into()),
        ThreadId("child-thread".into()),
        crate::test_support::publication("agent"),
        Vec::new(),
    ))
    .for_session(ThreadId("parent-session".into()));
    let claim = RunClaim {
        run_id: request.run_id().clone(),
        owner: "worker".into(),
        epoch: 1,
    };
    (source, reads, request, claim)
}

#[tokio::test]
async fn memory_settlement_checks_claim_and_state_before_opening_the_selected_partition() {
    // Causes: C1 exact/foreign claim Run; C2 committed Running/Awaiting/Ended.
    // Decision table: R1 !C1 => refuse before reads/factory for every C2;
    // R2 C1+Running => refuse, no reads/factory; R3 C1+Awaiting => success,
    // no reads/factory; R4 C1+Ended => open only the physical parent partition
    // and supply that exact commit once. The callback owns observer selection;
    // no model, Session installation or alternate partition participates.
    for exact in [false, true] {
        for state in [
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
        ] {
            let (source, reads, dispatch, mut claim) = fixture(false);
            if !exact {
                claim.run_id = RunId("foreign".into());
            }
            let calls = AtomicUsize::new(0);
            let factory_calls = &calls;
            let expected_commit = &source.commit;
            let result = observe_dispatched_memory_terminal(
                &reads,
                &dispatch,
                &claim,
                &state,
                |commit| async move {
                    assert!(Arc::ptr_eq(&commit, expected_commit), "R4");
                    factory_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                },
            )
            .await;
            let opened = exact && matches!(state, RunState::Ended(_));
            assert_eq!(result.is_ok(), exact && state != RunState::Running, "R1-R4");
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(opened), "R1-R4");
            assert_eq!(
                *source.partitions.lock().unwrap(),
                if opened {
                    vec!["parent-session"]
                } else {
                    Vec::new()
                },
                "R1-R4"
            );
        }
    }
}

struct RecordingObserver {
    fails: bool,
    facts: Mutex<Vec<CommittedTerminalRun>>,
}

#[async_trait::async_trait]
impl RunTerminalObserver for RecordingObserver {
    fn observer_id(&self) -> &str {
        "memory-extraction"
    }

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError> {
        self.facts.lock().unwrap().push(terminal.clone());
        if self.fails {
            Err(RunTerminalObserverError(
                "intent persistence refused".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn memory_settlement_preserves_failures_and_redelivers_the_same_committed_fact() {
    // Causes: C1 commit source succeeds/fails; C2 factory succeeds/fails;
    // C3 selected observer absent/present; C4 observer succeeds/fails.
    // Ordered rules: S1 !C1 => original read error, no factory/observation;
    // S2 C1+!C2 => original selection error, no observation; S3 C1+C2+!C3
    // => success; S4 C1+C2+C3+!C4 => SDK failure with stable observer id;
    // S5 all succeed => exact logical Run/Thread/cause, including redelivery.
    // S5 deliberately delivers twice: the existing Memory intent owns
    // idempotency; this adapter cannot suppress an unacknowledged observation.
    for row in ["read", "factory", "absent", "observer", "success"] {
        let (source, reads, dispatch, claim) = fixture(row == "read");
        let observer = Arc::new(RecordingObserver {
            fails: row == "observer",
            facts: Mutex::new(Vec::new()),
        });
        let calls = AtomicUsize::new(0);
        let repeats = if row == "success" { 2 } else { 1 };
        for _ in 0..repeats {
            let result = observe_dispatched_memory_terminal(
                &reads,
                &dispatch,
                &claim,
                &RunState::Ended(EndCause::Cancelled),
                |_| async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if row == "factory" {
                        return Err(DispatchSettlementError("frozen binding refused".into()));
                    }
                    Ok((row != "absent").then(|| observer.clone() as Arc<dyn RunTerminalObserver>))
                },
            )
            .await;
            let expected_error = match row {
                "read" => Some(RunError::unavailable("commit source unavailable").to_string()),
                "factory" => Some("frozen binding refused".into()),
                "observer" => Some(
                    "memory-extraction: terminal observer failed: intent persistence refused"
                        .into(),
                ),
                _ => None,
            };
            assert_eq!(
                result.err().map(|error| error.0),
                expected_error,
                "S1-S5 {row}"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            if row == "read" { 0 } else { repeats }
        );
        let facts = observer.facts.lock().unwrap();
        let expected_facts = if matches!(row, "observer" | "success") {
            repeats
        } else {
            0
        };
        assert_eq!(facts.len(), expected_facts, "S1-S5 {row}");
        for fact in facts.iter() {
            assert_eq!(fact.run_id, *dispatch.run_id());
            assert_eq!(fact.thread_id, *dispatch.thread_id());
            assert_eq!(fact.cause, EndCause::Cancelled);
        }
        assert_eq!(source.partitions.lock().unwrap().len(), repeats);
    }
}
