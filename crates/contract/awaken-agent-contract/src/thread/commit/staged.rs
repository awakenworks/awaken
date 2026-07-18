use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// A commit plan was structurally invalid (G1). Returned by
/// [`ThreadCommit::validate`] before any store write.
#[derive(Debug)]
pub struct ValidationError(String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid commit plan: {}", self.0)
    }
}

/// The run portion of a thread commit. The variant owns every value that is legal
/// for that lifecycle state, so an awaiting run without a resume ticket (or a
/// running/ended run with one) cannot be constructed.
#[derive(Debug, Clone, PartialEq)]
pub enum RunDisposition {
    Running {
        run_id: crate::agent::run::Id,
    },
    Awaiting(crate::agent::awaiting::ResumeTicket),
    Ended {
        run_id: crate::agent::run::Id,
        cause: crate::agent::run::EndCause,
    },
}

impl RunDisposition {
    #[must_use]
    pub fn running(run_id: crate::agent::run::Id) -> Self {
        Self::Running { run_id }
    }

    #[must_use]
    pub fn awaiting(ticket: crate::agent::awaiting::ResumeTicket) -> Self {
        Self::Awaiting(ticket)
    }

    #[must_use]
    pub fn ended(run_id: crate::agent::run::Id, cause: crate::agent::run::EndCause) -> Self {
        Self::Ended { run_id, cause }
    }

    #[must_use]
    pub fn run_id(&self) -> &crate::agent::run::Id {
        match self {
            Self::Running { run_id } | Self::Ended { run_id, .. } => run_id,
            Self::Awaiting(ticket) => &ticket.run_id,
        }
    }

    #[must_use]
    pub fn state(&self) -> crate::agent::run::RunState {
        match self {
            Self::Running { .. } => crate::agent::run::RunState::Running,
            Self::Awaiting(_) => crate::agent::run::RunState::Awaiting,
            Self::Ended { cause, .. } => crate::agent::run::RunState::Ended(cause.clone()),
        }
    }

    #[must_use]
    pub fn resume_ticket(&self) -> Option<&crate::agent::awaiting::ResumeTicket> {
        match self {
            Self::Awaiting(ticket) => Some(ticket),
            Self::Running { .. } | Self::Ended { .. } => None,
        }
    }

    fn from_legacy(
        fact: crate::thread::commit::run_fact::RunFact,
        ticket: Option<crate::agent::awaiting::ResumeTicket>,
    ) -> Result<Self, ValidationError> {
        use crate::agent::run::RunState;
        let ticket_shape = match &ticket {
            None => LegacyTicketShape::Absent,
            Some(ticket) if ticket.run_id == fact.run_id => LegacyTicketShape::Matching,
            Some(_) => LegacyTicketShape::Mismatched,
        };
        if !legacy_shape_is_valid(&fact.state, ticket_shape) {
            return Err(match (&fact.state, ticket_shape) {
                (RunState::Awaiting, LegacyTicketShape::Absent) => {
                    ValidationError("Awaiting requires a resume ticket".to_string())
                }
                (RunState::Running | RunState::Ended(_), _) => {
                    ValidationError("only Awaiting may carry a resume ticket".to_string())
                }
                (RunState::Awaiting, LegacyTicketShape::Mismatched) => {
                    ValidationError("resume ticket run_id does not match commit run_id".to_string())
                }
                _ => unreachable!("all legal legacy shapes passed the guard"),
            });
        }
        match (fact.state, ticket) {
            (RunState::Running, None) => Ok(Self::running(fact.run_id)),
            (RunState::Awaiting, Some(ticket)) => Ok(Self::awaiting(ticket)),
            (RunState::Ended(cause), None) => Ok(Self::ended(fact.run_id, cause)),
            _ => unreachable!("the legacy-shape guard rejected every illegal combination"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyTicketShape {
    Absent,
    Matching,
    Mismatched,
}

fn legacy_shape_is_valid(state: &crate::agent::run::RunState, ticket: LegacyTicketShape) -> bool {
    use crate::agent::run::RunState;
    matches!(
        (state, ticket),
        (RunState::Running, LegacyTicketShape::Absent)
            | (RunState::Awaiting, LegacyTicketShape::Matching)
            | (RunState::Ended(_), LegacyTicketShape::Absent)
    )
}

#[derive(Serialize, Deserialize)]
struct ThreadCommitWire {
    thread_id: crate::agent::thread::Id,
    run_fact: crate::thread::commit::run_fact::RunFact,
    messages: Vec<crate::agent::message::Message>,
    state: Vec<crate::agent::state::Command>,
    events: Vec<crate::audit::draft::Draft>,
    #[serde(default, rename = "waiting")]
    resume_ticket: Option<crate::agent::awaiting::ResumeTicket>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCommit {
    pub thread_id: crate::agent::thread::Id,
    pub run: RunDisposition,
    pub messages: Vec<crate::agent::message::Message>,
    pub state: Vec<crate::agent::state::Command>,
    pub events: Vec<crate::audit::draft::Draft>,
}

impl Serialize for ThreadCommit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ThreadCommitWire {
            thread_id: self.thread_id.clone(),
            run_fact: self.run_fact(),
            messages: self.messages.clone(),
            state: self.state.clone(),
            events: self.events.clone(),
            resume_ticket: self.resume_ticket().cloned(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ThreadCommit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ThreadCommitWire::deserialize(deserializer)?;
        let run = RunDisposition::from_legacy(wire.run_fact, wire.resume_ticket)
            .map_err(D::Error::custom)?;
        let commit = Self {
            thread_id: wire.thread_id,
            run,
            messages: wire.messages,
            state: wire.state,
            events: wire.events,
        };
        commit.validate().map_err(D::Error::custom)?;
        Ok(commit)
    }
}

impl ThreadCommit {
    /// Assemble a run's staged effects into a commit, synthesizing the lifecycle
    /// event drafts uniformly: `RunStateChanged` on a state transition, `StateChanged`
    /// when committed state rides, `RunAwaiting` when the run awaits. `extra_events`
    /// (e.g. permission-audit drafts) ride after them. This is the single place
    /// `(messages, disposition, state)` becomes a `ThreadCommit`, so the native
    /// loop and the ACP/A2A projected executors emit the same fact trail.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        thread_id: crate::agent::thread::Id,
        run: RunDisposition,
        state_changed: bool,
        messages: Vec<crate::agent::message::Message>,
        state: Vec<crate::agent::state::Command>,
        extra_events: Vec<crate::audit::draft::Draft>,
    ) -> Self {
        use crate::audit::run_event::RunEvent;
        let mut events = Vec::with_capacity(extra_events.len() + 3);
        let run_id = run.run_id().clone();
        let run_state = run.state();
        if state_changed {
            events.push(RunEvent::RunStateChanged { state: run_state }.into());
        }
        if !state.is_empty() {
            events.push(
                RunEvent::StateChanged {
                    commands: state.len(),
                }
                .into(),
            );
        }
        if matches!(run, RunDisposition::Awaiting(_)) {
            events.push(
                RunEvent::RunAwaiting {
                    run_id: run_id.0.clone(),
                }
                .into(),
            );
        }
        events.extend(extra_events);
        Self {
            thread_id,
            run,
            messages,
            state,
            events,
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &crate::agent::run::Id {
        self.run.run_id()
    }

    #[must_use]
    pub fn run_state(&self) -> crate::agent::run::RunState {
        self.run.state()
    }

    #[must_use]
    pub fn run_fact(&self) -> crate::thread::commit::run_fact::RunFact {
        crate::thread::commit::run_fact::RunFact {
            run_id: self.run_id().clone(),
            state: self.run_state(),
        }
    }

    #[must_use]
    pub fn resume_ticket(&self) -> Option<&crate::agent::awaiting::ResumeTicket> {
        self.run.resume_ticket()
    }

    /// Validate the commit plan before it reaches the store (G1).
    ///
    /// Both `thread_id` and `run_id` must be non-empty. An awaiting ticket, when
    /// present, must reference the same `run_id` and `thread_id` as the commit
    /// itself so that awaiting can never create an orphaned or cross-run ticket.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.thread_id.0.is_empty() {
            return Err(ValidationError("thread_id must not be empty".to_string()));
        }
        if self.run_id().0.is_empty() {
            return Err(ValidationError("run_id must not be empty".to_string()));
        }
        if let Some(ticket) = self.resume_ticket()
            && ticket.thread_id != self.thread_id
        {
            return Err(ValidationError(format!(
                "resume ticket thread_id {:?} does not match commit thread_id {:?}",
                ticket.thread_id.0, self.thread_id.0
            )));
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
    use crate::agent::run::{Id as RunId, RunState};
    use crate::agent::state::{Command, MergePolicy, Scope};
    use crate::agent::thread::Id as ThreadId;
    use crate::audit::draft::Draft;
    use crate::audit::kind::Kind;

    fn kinds(commit: &ThreadCommit) -> Vec<Kind> {
        commit.events.iter().map(|e| e.kind.clone()).collect()
    }

    fn assemble(
        run_state: RunState,
        changed: bool,
        state: Vec<Command>,
        resume_ticket: Option<crate::agent::awaiting::ResumeTicket>,
        extra: Vec<Draft>,
    ) -> ThreadCommit {
        let run = match (run_state, resume_ticket) {
            (RunState::Running, None) => RunDisposition::running(RunId("r".into())),
            (RunState::Awaiting, Some(ticket)) => RunDisposition::awaiting(ticket),
            (RunState::Ended(cause), None) => RunDisposition::ended(RunId("r".into()), cause),
            _ => panic!("test helper received an invalid run checkpoint"),
        };
        ThreadCommit::assemble(ThreadId("t".into()), run, changed, Vec::new(), state, extra)
    }

    #[test]
    fn run_state_changed_rides_only_on_a_transition() {
        assert_eq!(
            kinds(&assemble(RunState::Running, true, vec![], None, vec![])),
            vec![Kind::RunStateChanged]
        );
        // A non-first per-step increment stays Running: no state event.
        assert!(kinds(&assemble(RunState::Running, false, vec![], None, vec![])).is_empty());
    }

    #[test]
    fn state_and_audit_ride_in_order_after_the_run_state_event() {
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
        let commit = assemble(RunState::Running, true, state, None, vec![audit]);
        assert_eq!(
            kinds(&commit),
            vec![
                Kind::RunStateChanged,
                Kind::StateChanged,
                Kind::PermissionDecided
            ]
        );
        assert_eq!(commit.run_id().0, "r");
    }

    fn ticket(run: &str, thread: &str) -> crate::agent::awaiting::ResumeTicket {
        use crate::agent::awaiting::{AwaitReason, ResumeTicket};
        ResumeTicket {
            correlation_id: "corr".into(),
            run_id: RunId(run.into()),
            thread_id: ThreadId(thread.into()),
            snapshot_id: "snap".into(),
            catalog_fingerprint: "fp".into(),
            reason: AwaitReason::UserInput,
            call_id: None,
            pending_tool: None,
            deadline_ms: None,
        }
    }

    #[test]
    fn run_awaiting_rides_after_run_state_and_state_when_the_run_awaits() {
        let state = vec![Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "k",
            serde_json::json!(1),
        )];
        let commit = assemble(
            RunState::Awaiting,
            true,
            state,
            Some(ticket("r", "t")),
            vec![],
        );
        assert_eq!(
            kinds(&commit),
            vec![Kind::RunStateChanged, Kind::StateChanged, Kind::RunAwaiting]
        );
        // The synthesized RunAwaiting is keyed by run_id.
        let awaiting = commit
            .events
            .iter()
            .find(|e| e.kind == Kind::RunAwaiting)
            .unwrap();
        assert_eq!(awaiting.payload["run_id"], "r");
        assert!(commit.resume_ticket().is_some());
    }

    #[test]
    fn validate_rejects_empty_ids() {
        let empty_thread = assemble(RunState::Running, true, vec![], None, vec![]);
        let mut c = empty_thread;
        c.thread_id = ThreadId("".into());
        assert!(c.validate().is_err());

        let c2 = ThreadCommit::assemble(
            ThreadId("t".into()),
            RunDisposition::running(RunId("".into())),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert!(c2.validate().is_err());
    }

    #[test]
    fn invalid_legacy_cross_run_is_rejected_and_cross_thread_fails_validation() {
        // Legacy journals carried `run_fact` and `waiting` separately. Their old
        // wire shape remains readable, but an invalid pairing is rejected while
        // decoding rather than entering the typed domain model.
        let invalid = serde_json::json!({
            "thread_id": "t",
            "run_fact": {
                "run_id": "r",
                "phase": "Awaiting"
            },
            "messages": [],
            "state": [],
            "events": [],
            "waiting": ticket("other", "t")
        });
        assert!(serde_json::from_value::<ThreadCommit>(invalid).is_err());

        let cross_thread = assemble(
            RunState::Awaiting,
            true,
            vec![],
            Some(ticket("r", "other")),
            vec![],
        );
        assert!(cross_thread.validate().is_err());
    }

    #[test]
    fn legal_legacy_wire_enters_the_typed_model_and_serializes_compatibly() {
        let legacy = serde_json::json!({
            "thread_id": "t",
            "run_fact": {
                "run_id": "r",
                "phase": "Waiting"
            },
            "messages": [],
            "state": [],
            "events": [],
            "waiting": ticket("r", "t")
        });
        let commit: ThreadCommit = serde_json::from_value(legacy).unwrap();
        assert_eq!(commit.run_state(), RunState::Awaiting);
        assert!(commit.resume_ticket().is_some());

        let encoded = serde_json::to_value(&commit).unwrap();
        assert_eq!(encoded["run_fact"]["phase"], "Awaiting");
        assert!(encoded.get("waiting").is_some());
        assert!(encoded.get("run").is_none());
    }

    #[test]
    fn validate_accepts_a_matching_commit_and_ticket() {
        let ok = assemble(
            RunState::Awaiting,
            true,
            vec![],
            Some(ticket("r", "t")),
            vec![],
        );
        assert!(ok.validate().is_ok());
        // And a plain terminal commit with no ticket.
        let terminal = assemble(RunState::Running, true, vec![], None, vec![]);
        assert!(terminal.validate().is_ok());
    }

    /// Finite model check for the legacy boundary: of the nine combinations of
    /// lifecycle class and ticket shape, exactly the three domain-valid shapes
    /// enter `RunDisposition`.
    #[test]
    fn legacy_state_ticket_product_accepts_exactly_the_legal_shapes() {
        use crate::agent::run::EndCause;
        use crate::thread::commit::run_fact::RunFact;

        let states = [
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
        ];
        for state in states {
            for ticket_shape in 0..3 {
                let resume_ticket = match ticket_shape {
                    0 => None,
                    1 => Some(ticket("r", "t")),
                    _ => Some(ticket("other", "t")),
                };
                let accepted = RunDisposition::from_legacy(
                    RunFact {
                        run_id: RunId("r".into()),
                        state: state.clone(),
                    },
                    resume_ticket,
                )
                .is_ok();
                let expected = matches!(
                    (&state, ticket_shape),
                    (RunState::Running, 0) | (RunState::Awaiting, 1) | (RunState::Ended(_), 0)
                );
                assert_eq!(accepted, expected, "{state:?}, ticket shape {ticket_shape}");
            }
        }
    }

    #[test]
    fn disposition_projection_preserves_state_ticket_coherence() {
        use crate::agent::run::EndCause;

        let dispositions = [
            RunDisposition::running(RunId("r".into())),
            RunDisposition::awaiting(ticket("r", "t")),
            RunDisposition::ended(RunId("r".into()), EndCause::NaturalEnd),
        ];
        for disposition in dispositions {
            assert_eq!(disposition.run_id().0, "r");
            assert_eq!(
                disposition.resume_ticket().is_some(),
                matches!(disposition.state(), RunState::Awaiting),
            );
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;
    use crate::agent::run::{EndCause, RunState};

    fn symbolic_state(tag: u8) -> RunState {
        match tag % 3 {
            0 => RunState::Running,
            1 => RunState::Awaiting,
            _ => RunState::Ended(EndCause::NaturalEnd),
        }
    }

    /// Covers the complete 3×3 legacy lifecycle/ticket product symbolically.
    #[kani::proof]
    fn legacy_wire_accepts_exactly_the_legal_dispositions() {
        let state = symbolic_state(kani::any());
        let ticket_shape = kani::any::<u8>() % 3;
        let shape = match ticket_shape {
            0 => LegacyTicketShape::Absent,
            1 => LegacyTicketShape::Matching,
            _ => LegacyTicketShape::Mismatched,
        };
        let accepted = legacy_shape_is_valid(&state, shape);
        let expected = matches!(
            (&state, ticket_shape),
            (RunState::Running, 0) | (RunState::Awaiting, 1) | (RunState::Ended(_), 0)
        );
        assert_eq!(accepted, expected);
    }
}
