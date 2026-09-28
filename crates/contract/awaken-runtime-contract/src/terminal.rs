//! Post-commit observation of terminal Runs.
//!
//! This is deliberately narrower than a lifecycle hook: an observer receives
//! committed terminal identity and cause, has no control authority, and cannot
//! change the Run result. Delivery may repeat after recovery, so implementations
//! must turn the stable identity into an idempotent intent/receipt before causing
//! non-idempotent effects.

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use futures_util::FutureExt;
use std::sync::Arc;

/// The neutral committed fact delivered to terminal observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedTerminalRun {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub cause: EndCause,
}

/// A failed observation attempt. The committed Run remains authoritative and
/// recovery may deliver the same terminal fact again.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("terminal observer failed: {0}")]
pub struct RunTerminalObserverError(pub String);

/// Reacts to an already-committed terminal Run.
///
/// The observer owns its durable intent and receipt. `observer_id` must be
/// stable across processes so `(observer_id, run_id)` can be used as the
/// duplicate-suppression key.
#[async_trait]
pub trait RunTerminalObserver: Send + Sync {
    fn observer_id(&self) -> &str;

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError>;
}

/// One isolated delivery failure. Callers may log/measure these failures, but
/// must not project them back into the already-committed Run result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTerminalDeliveryFailure {
    pub observer_id: String,
    pub error: String,
}

/// Exact outcome of projecting one committed terminal fact.
///
/// `IdentityConflict` is distinct from `Nonterminal`: a stable Run id already
/// owned by another Thread must fail closed rather than being executed or
/// reported under the caller-supplied Thread identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommittedTerminalProjection {
    Nonterminal,
    Exact(CommittedTerminalRun),
    IdentityConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommittedTerminalAdmission {
    Nonterminal,
    Exact,
    IdentityConflict,
}

/// Classify every scalar fact used by the committed-terminal projection.
///
/// Keeping this inside the Runtime contract prevents Worker, Host, and stable-id
/// execution adapters from growing separate definitions of terminal recovery.
#[must_use]
const fn committed_terminal_admission(
    record_present: bool,
    run_identity_matches: bool,
    thread_identity_matches: bool,
    ended: bool,
) -> CommittedTerminalAdmission {
    if !record_present {
        return CommittedTerminalAdmission::Nonterminal;
    }
    if !run_identity_matches || !thread_identity_matches {
        return CommittedTerminalAdmission::IdentityConflict;
    }
    if ended {
        CommittedTerminalAdmission::Exact
    } else {
        CommittedTerminalAdmission::Nonterminal
    }
}

/// Project the one committed terminal fact for `run_id`.
///
/// Stable-id executors and recovery callers share this predicate so neither a
/// host nor an adapter can invent a second definition of terminality before
/// redelivery. A missing, running, or awaiting Run has no terminal projection.
pub fn committed_terminal_projection(
    reader: &dyn CommittedThreadView,
    run_id: &RunId,
    thread_id: &ThreadId,
) -> CommittedTerminalProjection {
    let record = reader.run(run_id);
    let admission = committed_terminal_admission(
        record.is_some(),
        record.as_ref().is_some_and(|record| record.id == *run_id),
        record
            .as_ref()
            .is_some_and(|record| record.thread_id == *thread_id),
        record.as_ref().is_some_and(|record| {
            matches!(
                record.state,
                awaken_agent_contract::agent::run::RunState::Ended(_)
            )
        }),
    );
    match admission {
        CommittedTerminalAdmission::Nonterminal => CommittedTerminalProjection::Nonterminal,
        CommittedTerminalAdmission::IdentityConflict => {
            CommittedTerminalProjection::IdentityConflict
        }
        CommittedTerminalAdmission::Exact => {
            let record = record.expect("exact admission requires a present record");
            let awaken_agent_contract::agent::run::RunState::Ended(cause) = record.state else {
                unreachable!("exact admission requires terminal state")
            };
            CommittedTerminalProjection::Exact(CommittedTerminalRun {
                run_id: record.id,
                thread_id: record.thread_id,
                cause,
            })
        }
    }
}

/// Deliver a terminal fact with uniform error and panic isolation across native,
/// ACP, A2A, and future Run executors.
pub async fn deliver_committed_terminal(
    observers: &[Arc<dyn RunTerminalObserver>],
    terminal: &CommittedTerminalRun,
) -> Vec<RunTerminalDeliveryFailure> {
    let mut failures = Vec::new();
    for observer in observers {
        let observer_id = observer.observer_id().to_string();
        let observer = observer.clone();
        let terminal = terminal.clone();
        // Poll in the caller task so execution-local context (tracing and future
        // neutral task locals) reaches the observer. Catching unwind around the
        // future retains panic isolation without a context-breaking task hop.
        match std::panic::AssertUnwindSafe(observer.observe(&terminal))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push(RunTerminalDeliveryFailure {
                observer_id,
                error: error.to_string(),
            }),
            Err(_) => failures.push(RunTerminalDeliveryFailure {
                observer_id,
                error: "observer panicked".to_string(),
            }),
        }
    }
    failures
}

/// Redeliver a terminal observation from committed truth.
///
/// Durable ingress and stable-id embedded execution call this after recovery.
/// `None` means the Run is absent or not terminal; `Some` means the committed
/// terminal fact was delivered, even when individual observers failed.
pub async fn redeliver_committed_terminal(
    reader: &dyn CommittedThreadView,
    observers: &[Arc<dyn RunTerminalObserver>],
    run_id: &RunId,
    thread_id: &ThreadId,
) -> Option<Vec<RunTerminalDeliveryFailure>> {
    let CommittedTerminalProjection::Exact(terminal) =
        committed_terminal_projection(reader, run_id, thread_id)
    else {
        return None;
    };
    Some(deliver_committed_terminal(observers, &terminal).await)
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_worker.committed_terminal_recovery_skips_execution_and_settles_dispatch
    /// @obligation remote_worker.pending_signal_is_consumed_by_commit
    /// @obligation tool_result.terminal_run_rejects_late_result
    #[kani::proof]
    fn committed_terminal_projection_admits_only_exact_ended_identity() {
        let present: bool = kani::any();
        let run_matches: bool = kani::any();
        let thread_matches: bool = kani::any();
        let ended: bool = kani::any();
        let admission = committed_terminal_admission(present, run_matches, thread_matches, ended);

        assert_eq!(
            admission == CommittedTerminalAdmission::Exact,
            present && run_matches && thread_matches && ended
        );
        assert_eq!(
            admission == CommittedTerminalAdmission::IdentityConflict,
            present && (!run_matches || !thread_matches)
        );
        assert_eq!(
            admission == CommittedTerminalAdmission::Nonterminal,
            !present || (run_matches && thread_matches && !ended)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::message::Message;
    use awaken_agent_contract::agent::run::{Failure, Record as RunRecord, RunState};
    use std::sync::Mutex;

    struct StateView(Option<RunRecord>);

    impl CommittedThreadView for StateView {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            self.0.clone()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
            // This terminal-projection fixture never represents an awaiting
            // ticket, so no Thread can expose an open wait.
            None
        }
    }

    struct PanickingObserver;

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<EndCause>>);

    #[async_trait]
    impl RunTerminalObserver for RecordingObserver {
        fn observer_id(&self) -> &str {
            "recording-observer"
        }

        async fn observe(
            &self,
            terminal: &CommittedTerminalRun,
        ) -> Result<(), RunTerminalObserverError> {
            self.0
                .lock()
                .expect("terminal observations mutex")
                .push(terminal.cause.clone());
            Ok(())
        }
    }

    #[async_trait]
    impl RunTerminalObserver for PanickingObserver {
        fn observer_id(&self) -> &str {
            "panicking-observer"
        }

        async fn observe(
            &self,
            _terminal: &CommittedTerminalRun,
        ) -> Result<(), RunTerminalObserverError> {
            panic!("expected observer panic")
        }
    }

    #[test]
    fn only_ended_committed_truth_projects_a_terminal_run() {
        // Cause/effect graph: C1 the requested Run is present; C2 its Run
        // identity is exact; C3 its Thread identity is exact; C4 its state is
        // Ended. Effects: E1 return the exact identity/cause; E2 return
        // Nonterminal; E3 fail closed as IdentityConflict.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | R1 | no | any | any | any | E2 |
        // | R2 | yes | no | any | any | E3 |
        // | R3 | yes | yes | no | any | E3 |
        // | R4 | yes | yes | yes | no | E2 |
        // | R5 | yes | yes | yes | yes | E1 |
        //
        // Redelivery and every Worker recovery path consume this same
        // projection, so this table is the only terminal-admission algorithm.
        let run_id = RunId("run".to_string());
        let thread_id = ThreadId("thread".to_string());
        for state in [None, Some(RunState::Running), Some(RunState::Awaiting)] {
            let view = StateView(state.map(|state| RunRecord {
                id: run_id.clone(),
                thread_id: thread_id.clone(),
                state,
            }));
            assert_eq!(
                committed_terminal_projection(&view, &run_id, &thread_id),
                CommittedTerminalProjection::Nonterminal,
                "R1/R4"
            );
        }

        let foreign_run = StateView(Some(RunRecord {
            id: RunId("foreign-run".to_string()),
            thread_id: thread_id.clone(),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        assert_eq!(
            committed_terminal_projection(&foreign_run, &run_id, &thread_id),
            CommittedTerminalProjection::IdentityConflict,
            "R2"
        );

        let foreign_thread = StateView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: ThreadId("foreign-thread".to_string()),
            state: RunState::Ended(EndCause::NaturalEnd),
        }));
        assert_eq!(
            committed_terminal_projection(&foreign_thread, &run_id, &thread_id),
            CommittedTerminalProjection::IdentityConflict,
            "R3"
        );

        let view = StateView(Some(RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state: RunState::Ended(EndCause::MaxSteps),
        }));
        assert_eq!(
            committed_terminal_projection(&view, &run_id, &thread_id),
            CommittedTerminalProjection::Exact(CommittedTerminalRun {
                run_id,
                thread_id,
                cause: EndCause::MaxSteps,
            }),
            "R5"
        );
    }

    #[tokio::test]
    async fn a_panicking_observer_is_reported_instead_of_unwinding_delivery() {
        let terminal = CommittedTerminalRun {
            run_id: RunId("run".to_string()),
            thread_id: ThreadId("thread".to_string()),
            cause: EndCause::NaturalEnd,
        };

        let failures = deliver_committed_terminal(&[Arc::new(PanickingObserver)], &terminal).await;

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].observer_id, "panicking-observer");
        assert_eq!(failures[0].error, "observer panicked");
    }

    #[tokio::test]
    async fn every_native_terminal_cause_uses_the_same_delivery_contract() {
        let observer = Arc::new(RecordingObserver::default());
        let erased: Arc<dyn RunTerminalObserver> = observer.clone();
        let causes = vec![
            EndCause::NaturalEnd,
            EndCause::Cancelled,
            EndCause::Stopped("budget".to_string()),
            EndCause::MaxSteps,
            EndCause::Error(Failure::Inference {
                code: "provider_error".to_string(),
                message: "failed".to_string(),
            }),
        ];

        for (index, cause) in causes.iter().enumerate() {
            let terminal = CommittedTerminalRun {
                run_id: RunId(format!("run-{index}")),
                thread_id: ThreadId("thread".to_string()),
                cause: cause.clone(),
            };
            assert!(
                deliver_committed_terminal(std::slice::from_ref(&erased), &terminal)
                    .await
                    .is_empty()
            );
        }

        assert_eq!(
            observer.0.lock().expect("observations mutex").as_slice(),
            causes.as_slice()
        );
    }
}
