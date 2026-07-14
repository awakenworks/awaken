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
    pub run_fact: crate::fact::run::Fact,
    pub messages: Vec<crate::agent::message::Message>,
    pub state: Vec<crate::agent::state::Command>,
    pub events: Vec<crate::event::draft::Draft>,
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
    pub fn assemble(
        thread_id: crate::agent::thread::Id,
        run_id: crate::agent::run::Id,
        phase: crate::agent::run::Phase,
        phase_changed: bool,
        messages: Vec<crate::agent::message::Message>,
        state: Vec<crate::agent::state::Command>,
        waiting: Option<crate::agent::waiting::WaitingTicket>,
        extra_events: Vec<crate::event::draft::Draft>,
    ) -> Self {
        use crate::event::run_event::RunEvent;
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
            run_fact: crate::fact::run::Fact { run_id, phase },
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
    use crate::event::draft::Draft;
    use crate::event::kind::Kind;

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
}
