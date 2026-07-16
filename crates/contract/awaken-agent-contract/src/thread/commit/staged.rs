use std::fmt;

use serde::{Deserialize, Serialize};

/// A commit plan was structurally invalid (G1). Returned by
/// [`ThreadCommit::validate`] before any store write.
#[derive(Debug)]
pub struct ValidationError(String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid commit plan: {}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadCommit {
    pub thread_id: crate::agent::thread::Id,
    pub run_fact: crate::thread::commit::run_fact::RunFact,
    pub messages: Vec<crate::agent::message::Message>,
    pub state: Vec<crate::agent::state::Command>,
    pub events: Vec<crate::audit::draft::Draft>,
    /// A same-run pause committed atomically with this checkpoint. `Some` parks
    /// the run; `None` clears any prior ticket (resume/terminal).
    #[serde(default)]
    pub waiting: Option<crate::agent::waiting::WaitingTicket>,
}

impl ThreadCommit {
    /// Assemble a run's staged effects into a commit, synthesizing the lifecycle
    /// event drafts uniformly: `RunPhaseChanged` on a phase transition, `StateChanged`
    /// when committed state rides, `RunWaiting` when the run parks. `extra_events`
    /// (e.g. permission-audit drafts) ride after them. This is the single place
    /// `(messages, phase, state, waiting)` becomes a `ThreadCommit`, so the native
    /// loop and the ACP/A2A projected executors emit the same fact trail.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        thread_id: crate::agent::thread::Id,
        run_id: crate::agent::run::Id,
        phase: crate::agent::run::Phase,
        phase_changed: bool,
        messages: Vec<crate::agent::message::Message>,
        state: Vec<crate::agent::state::Command>,
        waiting: Option<crate::agent::waiting::WaitingTicket>,
        extra_events: Vec<crate::audit::draft::Draft>,
    ) -> Self {
        use crate::audit::run_event::RunEvent;
        let mut events = Vec::with_capacity(extra_events.len() + 3);
        if phase_changed {
            events.push(
                RunEvent::RunPhaseChanged {
                    phase: phase.clone(),
                }
                .into(),
            );
        }
        if !state.is_empty() {
            events.push(
                RunEvent::StateChanged {
                    commands: state.len(),
                }
                .into(),
            );
        }
        if waiting.is_some() {
            events.push(
                RunEvent::RunWaiting {
                    run_id: run_id.0.clone(),
                }
                .into(),
            );
        }
        events.extend(extra_events);
        Self {
            thread_id,
            run_fact: crate::thread::commit::run_fact::RunFact { run_id, phase },
            messages,
            state,
            events,
            waiting,
        }
    }

    /// Validate the commit plan before it reaches the store (G1).
    ///
    /// Both `thread_id` and `run_id` must be non-empty. A waiting ticket, when
    /// present, must reference the same `run_id` and `thread_id` as the commit
    /// itself so that parking can never create an orphaned or cross-run ticket.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.thread_id.0.is_empty() {
            return Err(ValidationError("thread_id must not be empty".to_string()));
        }
        if self.run_fact.run_id.0.is_empty() {
            return Err(ValidationError("run_id must not be empty".to_string()));
        }
        if let Some(ticket) = &self.waiting {
            if ticket.run_id != self.run_fact.run_id {
                return Err(ValidationError(format!(
                    "waiting ticket run_id {:?} does not match commit run_id {:?}",
                    ticket.run_id.0, self.run_fact.run_id.0
                )));
            }
            if ticket.thread_id != self.thread_id {
                return Err(ValidationError(format!(
                    "waiting ticket thread_id {:?} does not match commit thread_id {:?}",
                    ticket.thread_id.0, self.thread_id.0
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub sequence: u64,
}

#[cfg(test)]
mod assemble_tests {
    use super::*;
    use crate::agent::run::{Id as RunId, Phase};
    use crate::agent::state::{Command, MergePolicy, Scope};
    use crate::agent::thread::Id as ThreadId;
    use crate::audit::draft::Draft;
    use crate::audit::kind::Kind;

    fn kinds(commit: &ThreadCommit) -> Vec<Kind> {
        commit.events.iter().map(|e| e.kind.clone()).collect()
    }

    fn assemble(
        phase: Phase,
        changed: bool,
        state: Vec<Command>,
        waiting: Option<crate::agent::waiting::WaitingTicket>,
        extra: Vec<Draft>,
    ) -> ThreadCommit {
        ThreadCommit::assemble(
            ThreadId("t".into()),
            RunId("r".into()),
            phase,
            changed,
            Vec::new(),
            state,
            waiting,
            extra,
        )
    }

    #[test]
    fn run_phase_changed_rides_only_on_a_transition() {
        assert_eq!(
            kinds(&assemble(Phase::Running, true, vec![], None, vec![])),
            vec![Kind::RunPhaseChanged]
        );
        // A non-first per-step increment stays Running: no phase event.
        assert!(kinds(&assemble(Phase::Running, false, vec![], None, vec![])).is_empty());
    }

    #[test]
    fn state_and_audit_ride_in_order_after_the_phase_event() {
        let audit = Draft {
            kind: Kind::PermissionDecided,
            payload: serde_json::json!({}),
        };
        let state = vec![Command::set(
            Scope::Thread,
            MergePolicy::Commutative,
            "k",
            serde_json::json!(1),
        )];
        let commit = assemble(Phase::Running, true, state, None, vec![audit]);
        assert_eq!(
            kinds(&commit),
            vec![
                Kind::RunPhaseChanged,
                Kind::StateChanged,
                Kind::PermissionDecided
            ]
        );
        assert_eq!(commit.run_fact.run_id.0, "r");
    }

    fn ticket(run: &str, thread: &str) -> crate::agent::waiting::WaitingTicket {
        use crate::agent::waiting::{WaitingReason, WaitingTicket};
        WaitingTicket {
            correlation_id: "corr".into(),
            run_id: RunId(run.into()),
            thread_id: ThreadId(thread.into()),
            snapshot_id: "snap".into(),
            catalog_fingerprint: "fp".into(),
            reason: WaitingReason::UserInput,
            call_id: None,
            pending_tool: None,
            deadline_ms: None,
        }
    }

    #[test]
    fn run_waiting_rides_after_phase_and_state_when_the_run_parks() {
        let state = vec![Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "k",
            serde_json::json!(1),
        )];
        let commit = assemble(Phase::Waiting, true, state, Some(ticket("r", "t")), vec![]);
        assert_eq!(
            kinds(&commit),
            vec![Kind::RunPhaseChanged, Kind::StateChanged, Kind::RunWaiting]
        );
        // The synthesized RunWaiting is keyed by run_id.
        let waiting = commit
            .events
            .iter()
            .find(|e| e.kind == Kind::RunWaiting)
            .unwrap();
        assert_eq!(waiting.payload["run_id"], "r");
        assert!(commit.waiting.is_some());
    }

    #[test]
    fn validate_rejects_empty_ids() {
        let empty_thread = assemble(Phase::Running, true, vec![], None, vec![]);
        let mut c = empty_thread;
        c.thread_id = ThreadId("".into());
        assert!(c.validate().is_err());

        let mut c2 = assemble(Phase::Running, true, vec![], None, vec![]);
        c2.run_fact.run_id = RunId("".into());
        assert!(c2.validate().is_err());
    }

    #[test]
    fn validate_rejects_a_cross_run_or_cross_thread_ticket() {
        // Ticket run/thread must match the commit; a mismatch is an orphan risk.
        let cross_run = assemble(
            Phase::Waiting,
            true,
            vec![],
            Some(ticket("other", "t")),
            vec![],
        );
        assert!(cross_run.validate().is_err());

        let cross_thread = assemble(
            Phase::Waiting,
            true,
            vec![],
            Some(ticket("r", "other")),
            vec![],
        );
        assert!(cross_thread.validate().is_err());
    }

    #[test]
    fn validate_accepts_a_matching_commit_and_ticket() {
        let ok = assemble(Phase::Waiting, true, vec![], Some(ticket("r", "t")), vec![]);
        assert!(ok.validate().is_ok());
        // And a plain terminal commit with no ticket.
        let terminal = assemble(Phase::Running, true, vec![], None, vec![]);
        assert!(terminal.validate().is_ok());
    }
}
