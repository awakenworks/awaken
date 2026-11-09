// @formal entrypoint tool_result.resume_receipt_is_atomic_with_ticket_consumption
//! Exact resume metadata consumed by the existing ThreadCommit aggregate.
use super::{ThreadCommit, ValidationError};
use crate::agent::awaiting::ResumeTicket;
use crate::audit::run_event::RunEvent;

/// Accepted input carried only until the first successful replacing commit.
/// This value grants no authority and owns no durable receipt store.
#[derive(Debug, Clone)]
pub struct ResumeCommit {
    operation_id: Option<String>,
    answered_ticket: ResumeTicket,
}

impl ResumeCommit {
    #[must_use]
    pub fn new(operation_id: Option<String>, answered_ticket: &ResumeTicket) -> Self {
        Self {
            operation_id,
            answered_ticket: answered_ticket.clone(),
        }
    }

    #[must_use]
    pub fn answered_ticket(&self) -> &ResumeTicket {
        &self.answered_ticket
    }
}

const fn resume_commit_is_coherent(
    same_thread: bool,
    same_run: bool,
    retains_answered_ticket: bool,
) -> bool {
    same_thread && same_run && !retains_answered_ticket
}

impl ThreadCommit {
    /// Bind a resume receipt to the disposition consuming its exact ticket.
    /// Unnamed direct input has the same identity check and no named audit fact.
    pub fn with_consumed_resume(mut self, resume: &ResumeCommit) -> Result<Self, ValidationError> {
        let answered = &resume.answered_ticket;
        let retains_answered_ticket = self
            .resume_ticket()
            .is_some_and(|next| next.correlation_id == answered.correlation_id);
        if !resume_commit_is_coherent(
            answered.thread_id == self.thread_id,
            self.run_id() == &answered.run_id,
            retains_answered_ticket,
        ) {
            return Err(ValidationError(
                "resume receipt commit does not replace the exact answered ticket".into(),
            ));
        }
        if let Some(operation_id) = &resume.operation_id {
            self.events.push(
                RunEvent::ResumeApplied {
                    operation_id: operation_id.clone(),
                    correlation_id: answered.correlation_id.clone(),
                }
                .into(),
            );
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::awaiting::{AwaitTarget, PauseReason};
    use crate::agent::run::{EndCause, Id as RunId};
    use crate::agent::thread::Id as ThreadId;
    use crate::audit::kind::Kind;
    use crate::thread::commit::staged::RunDisposition;

    /// @exhaustive bool(2) x bool(2) x bool(2) = 8
    /// @obligation tool_result.resume_receipt_is_atomic_with_ticket_consumption
    #[test]
    fn a_resume_receipt_requires_consumption_of_the_exact_ticket() {
        // C1 exact Thread, C2 exact Run, C3 answered correlation retained.
        // Complete eight-rule table: only C1+C2+!C3 admits one replacing
        // commit. This is the unchanged predicate moved from Native's private
        // progress constructor so ACP and Native cannot own different rules.
        for same_thread in [false, true] {
            for same_run in [false, true] {
                for retains_answered_ticket in [false, true] {
                    assert_eq!(
                        resume_commit_is_coherent(same_thread, same_run, retains_answered_ticket),
                        same_thread && same_run && !retains_answered_ticket,
                    );
                }
            }
        }
    }

    #[test]
    fn actual_resume_commit_covers_named_unnamed_and_every_identity_rule() {
        // Graph: C1/C2 exact Thread/Run and C3 replaced ticket cause E1
        // admission; C4 named input additionally causes E2 exactly one matching
        // ResumeApplied in that same ThreadCommit. All 2^4 interacting rules
        // are enumerated. No named operation means no fabricated receipt.
        let answered = ResumeTicket::new(
            "correlation",
            RunId("run".into()),
            ThreadId("thread".into()),
            "snapshot",
            "fingerprint",
            AwaitTarget::Pause(PauseReason::Manual),
        );
        for named in [false, true] {
            for same_thread in [false, true] {
                for same_run in [false, true] {
                    for retains in [false, true] {
                        let thread = ThreadId(if same_thread { "thread" } else { "other" }.into());
                        let run = RunId(if same_run { "run" } else { "other" }.into());
                        let disposition = if retains {
                            let mut next = answered.clone();
                            next.thread_id = thread.clone();
                            next.run_id = run;
                            RunDisposition::awaiting(next)
                        } else {
                            RunDisposition::running(run)
                        };
                        let resume =
                            ResumeCommit::new(named.then(|| "operation".into()), &answered);
                        let result = ThreadCommit::assemble(
                            thread,
                            disposition,
                            true,
                            vec![],
                            vec![],
                            vec![],
                        )
                        .with_consumed_resume(&resume);
                        assert_eq!(result.is_ok(), same_thread && same_run && !retains);
                        if let Ok(commit) = result {
                            commit.validate().unwrap();
                            let receipts: Vec<_> = commit
                                .events
                                .iter()
                                .filter(|e| e.kind == Kind::ResumeApplied)
                                .collect();
                            assert_eq!(receipts.len(), usize::from(named));
                            if named {
                                assert_eq!(receipts[0].payload["operation_id"], "operation");
                                assert_eq!(receipts[0].payload["correlation_id"], "correlation");
                            }
                        }
                    }
                }
            }
        }
        // E3 a terminal replacing disposition obeys the same receipt owner.
        let commit = ThreadCommit::assemble(
            answered.thread_id.clone(),
            RunDisposition::ended(answered.run_id.clone(), EndCause::NaturalEnd),
            true,
            vec![],
            vec![],
            vec![],
        )
        .with_consumed_resume(&ResumeCommit::new(Some("terminal".into()), &answered))
        .unwrap();
        assert_eq!(
            commit
                .events
                .iter()
                .filter(|e| e.kind == Kind::ResumeApplied)
                .count(),
            1
        );
    }
}
