// @formal surface exclude=read_port_contract
// @formal owner docs/design/runtime-persistence.md
// @formal reason: Serializable recovery snapshot and asynchronous read-port signature only; it cannot
// @formal reason: mutate, fence, or select committed truth.
//! Claim-scoped recovery reads for a database-independent Worker.
//!
//! A remote Worker cannot reconstruct a Run by calling the synchronous read
//! ports independently: a commit between those calls could combine values from
//! different committed prefixes. This port returns the complete execution view
//! under one backend consistency boundary. The snapshot is read-only; commit
//! authority remains with the coordinator.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::awaiting::ResumeTicket;
use crate::agent::message::Message;
use crate::agent::run::{Id as RunId, Record as RunRecord, RunState};
use crate::agent::state::Command as StateCommand;
use crate::agent::thread::Id as ThreadId;
use crate::audit::record::Record as EventRecord;

/// One active resume ticket paired with the Run whose disposition owns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResumeTicket {
    pub run_id: RunId,
    pub ticket: ResumeTicket,
}

/// A persisted waiting row is usable only when its closed ticket decodes and
/// names the same Run/Thread as the row and recovery partition. Keeping this
/// invariant in the neutral recovery contract prevents each durable adapter or
/// protocol projection from inventing a different corruption policy.
#[derive(Debug, thiserror::Error)]
pub enum ResumeTicketRecoveryError {
    #[error("persisted ResumeTicket is unreadable: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("ResumeTicket run_id does not match its waiting row")]
    RunMismatch,
    #[error("ResumeTicket thread_id does not match its recovery partition")]
    ThreadMismatch,
}

pub fn validate_resume_ticket_owner(
    ticket: &ResumeTicket,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<(), ResumeTicketRecoveryError> {
    if &ticket.run_id != row_run_id {
        return Err(ResumeTicketRecoveryError::RunMismatch);
    }
    if &ticket.thread_id != expected_thread_id {
        return Err(ResumeTicketRecoveryError::ThreadMismatch);
    }
    Ok(())
}

pub fn decode_resume_ticket_value_for_owner(
    value: serde_json::Value,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<ResumeTicket, ResumeTicketRecoveryError> {
    let ticket = serde_json::from_value(value)?;
    validate_resume_ticket_owner(&ticket, row_run_id, expected_thread_id)?;
    Ok(ticket)
}

pub fn decode_resume_ticket_json_for_owner(
    raw: &str,
    row_run_id: &RunId,
    expected_thread_id: &ThreadId,
) -> Result<ResumeTicket, ResumeTicketRecoveryError> {
    let ticket = serde_json::from_str(raw)?;
    validate_resume_ticket_owner(&ticket, row_run_id, expected_thread_id)?;
    Ok(ticket)
}

/// Project decoded commit facts into first-seen Run order and latest Run state.
///
/// Both durable adapters use this reducer inside their recovery transaction.
/// Each row is still decoded in commit order, including overwritten states;
/// the first decoder error is returned without reading later rows. A temporary
/// identity index avoids rescanning every previous Run for each commit. It is
/// discarded with this query and never replaces the committed fact authority.
/// Transition admission remains owned by the commit boundary.
pub fn project_committed_run_records<E>(
    thread_id: &ThreadId,
    commits: impl IntoIterator<Item = Result<(RunId, RunState), E>>,
) -> Result<(Vec<RunRecord>, Option<RunId>), E> {
    let mut positions = std::collections::HashMap::<RunId, usize>::new();
    let mut runs = Vec::new();
    let mut latest_run_id = None;
    for row in commits {
        let (run_id, state) = row?;
        latest_run_id = Some(run_id.clone());
        let record = RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state,
        };
        match positions.entry(run_id) {
            std::collections::hash_map::Entry::Occupied(entry) => runs[*entry.get()] = record,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(runs.len());
                runs.push(record);
            }
        }
    }
    Ok((runs, latest_run_id))
}

/// One internally consistent prefix of committed Thread truth.
///
/// `thread_version` is the count of commits on this Thread and is therefore the
/// optimistic-concurrency version for the aggregate. `store_cursor` is the
/// backend-wide committed sequence used only for diagnostics/feed backfill.
/// They are intentionally distinct: another Thread advancing `store_cursor`
/// must not conflict this Thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecoverySnapshot {
    pub thread_id: ThreadId,
    pub claimed_run_id: RunId,
    pub runs: Vec<RunRecord>,
    pub latest_run_id: Option<RunId>,
    pub messages: Vec<Message>,
    /// Backend commit coordinate for each message at the same index. Existing
    /// stores already persist this column; exposing it prevents a cold projector
    /// from moving every historical message behind every lifecycle transition.
    /// A legacy/remote snapshot may omit the vector, in which case consumers must
    /// not claim exact warm/cold total ordering for that prefix.
    #[serde(default)]
    pub message_commit_cursors: Vec<u64>,
    /// Store-owned commit wall times from this same read transaction. Historical
    /// file journals may lack times; absence is unknown, never recovery time.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub commit_timestamps: std::collections::BTreeMap<u64, u64>,
    pub state: Vec<StateCommand>,
    /// Backend commit coordinate for each state command at the same index.
    /// State-backed public facts (for example context compaction and Outcome
    /// evaluation) use this existing durable coordinate instead of moving to a
    /// Run's earlier lifecycle cursor when discovered by a later projector.
    #[serde(default)]
    pub state_commit_cursors: Vec<u64>,
    /// Committed audit facts for this Thread, ordered by their durable event
    /// sequence and read under the same consistency boundary as messages,
    /// state, tickets, and `store_cursor`.
    #[serde(default)]
    pub events: Vec<EventRecord>,
    pub resume_tickets: Vec<RunResumeTicket>,
    pub thread_version: u64,
    pub store_cursor: u64,
    /// The stable ordinal a recovered Worker uses for the claimed Run's next
    /// logical commit. It is the number of that Run's committed operations.
    pub next_commit_ordinal: u64,
}

/// Why a recovery prefix cannot name its current Run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CurrentRunError {
    /// Committed Runs are present but the prefix omits which one committed
    /// last; `runs` is in first-seen order, so it cannot stand in.
    #[error("recovery prefix has committed Runs but omits its latest Run coordinate")]
    LatestOmitted,
    /// The latest coordinate names a Run whose committed state is absent.
    #[error("recovery prefix names latest Run {0:?} without its committed state")]
    LatestStateMissing(RunId),
}

impl RunRecoverySnapshot {
    /// The Thread's current Run: the Run of the last commit in this prefix.
    ///
    /// Consumers (answerable-ticket selection, interruption, follow-up and
    /// reply admission, the Managed projection) share this one rule instead of
    /// each choosing between an error, a silent skip, and a claimed-Run
    /// fallback. `claimed_run_id` is never a fallback: it is the reader's claim
    /// coordinate (Session projection readers pass the latest Run they already
    /// read, remote Workers the Run they hold), not evidence of what committed
    /// last. `project_committed_run_records` sets `latest_run_id` exactly when
    /// a commit exists, so:
    ///
    /// | `latest_run_id` | `runs` | Result |
    /// |---|---|---|
    /// | `None` | empty | `Ok(None)`: nothing committed yet |
    /// | `None` | non-empty | `Err(LatestOmitted)` |
    /// | `Some(id)` | contains `id` | `Ok(Some(record))` |
    /// | `Some(id)` | lacks `id` | `Err(LatestStateMissing)` |
    pub fn current_run(&self) -> Result<Option<&RunRecord>, CurrentRunError> {
        let Some(latest_run_id) = self.latest_run_id.as_ref() else {
            return if self.runs.is_empty() {
                Ok(None)
            } else {
                Err(CurrentRunError::LatestOmitted)
            };
        };
        self.runs
            .iter()
            .find(|run| &run.id == latest_run_id)
            .map(Some)
            .ok_or_else(|| CurrentRunError::LatestStateMissing(latest_run_id.clone()))
    }
}

/// How many committed ResumeTicket rows a Run owns in one recovery prefix.
///
/// Consumers: Managed pending projection and reply admission, the Session
/// application's reply delivery, and the Runtime Host's reply fence, budget
/// resume and awaiting projection. They used to disagree on two rows (one
/// failed closed, others took the first); the Runtime resumes one committed
/// ticket at a time, so two rows for one Run are damaged authority everywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunTicketSelection<'a> {
    Absent,
    Multiple,
    One(&'a RunResumeTicket),
}

/// Why a Run's answerable ticket cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum AnswerableTicketError {
    #[error("recovery prefix holds more than one committed ResumeTicket for Run {0:?}")]
    Multiple(RunId),
    #[error("committed ResumeTicket for Run {run_id:?} names a different owner: {source}")]
    Owner {
        run_id: RunId,
        #[source]
        source: ResumeTicketRecoveryError,
    },
}

impl RunRecoverySnapshot {
    /// Select the committed ticket rows owned by `run_id`, counting before any
    /// payload is inspected.
    #[must_use]
    pub fn ticket_for_run(&self, run_id: &RunId) -> RunTicketSelection<'_> {
        let mut rows = self
            .resume_tickets
            .iter()
            .filter(|entry| &entry.run_id == run_id);
        match (rows.next(), rows.next()) {
            (None, _) => RunTicketSelection::Absent,
            (Some(_), Some(_)) => RunTicketSelection::Multiple,
            (Some(entry), None) => RunTicketSelection::One(entry),
        }
    }

    /// The one answerable ticket of `run_id`: absent is `None`, two rows or a
    /// ticket naming another Run/Thread than its row and this prefix is an
    /// error. Callers apply their own fences (call id, correlation) to the
    /// returned ticket, never to choose among several.
    pub fn answerable_ticket(
        &self,
        run_id: &RunId,
    ) -> Result<Option<&ResumeTicket>, AnswerableTicketError> {
        match self.ticket_for_run(run_id) {
            RunTicketSelection::Absent => Ok(None),
            RunTicketSelection::Multiple => Err(AnswerableTicketError::Multiple(run_id.clone())),
            RunTicketSelection::One(entry) => {
                validate_resume_ticket_owner(&entry.ticket, &entry.run_id, &self.thread_id)
                    .map_err(|source| AnswerableTicketError::Owner {
                        run_id: run_id.clone(),
                        source,
                    })?;
                Ok(Some(&entry.ticket))
            }
        }
    }
}

/// Failure to materialize an authoritative recovery prefix.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("recovery source rejected: {0}")]
    Rejected(String),
}

/// Asynchronous because a distributed store must read the authoritative
/// database in one repeatable-read (or stronger) transaction.
#[async_trait]
pub trait RunRecoverySource: Send + Sync {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError>;

    /// Recover one logical Thread from the physical Session partition selected by
    /// the guarded dispatch. Partition-bound sources may keep the default; a
    /// process-level source must override it instead of deriving physical
    /// ownership from the logical child id.
    async fn recovery_snapshot_in_session(
        &self,
        _session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.recovery_snapshot(thread_id, claimed_run_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answerable_ticket_counts_rows_before_identity_and_never_picks_one_of_many() {
        // Causes: C1 rows owned by the selected Run: 0/1/2; C2 rows owned by
        // another Run: 0/1; C3 the single row's ticket names its row's Run and
        // this prefix's Thread, a foreign Run, or a foreign Thread. Effects:
        // E1 None; E2 the ticket; E3 Multiple; E4 Owner mismatch.
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | T1 | 0 | any | - | E1 (another Run's row is not ours) |
        // | T2 | 1 | any | exact | E2 |
        // | T3 | 1 | any | foreign Run/Thread | E4 |
        // | T4 | 2 | any | any | E3 (cardinality before identity) |
        use crate::agent::awaiting::{
            AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason, ToolCallAuthority,
        };
        let thread = ThreadId("t".into());
        let ticket = |run: &str, thread: &str| {
            ResumeTicket::new(
                "c",
                RunId(run.into()),
                ThreadId(thread.into()),
                "s",
                "f",
                AwaitTarget::ToolCall {
                    reason: ToolAwaitReason::ClientExecution,
                    call_id: "a".into(),
                    tool: PendingTool {
                        tool_id: "x".into(),
                        arguments: serde_json::Value::Null,
                    },
                    authority: ToolCallAuthority::RuntimeToolBatch,
                },
            )
        };
        let row = |row_run: &str, inner_run: &str, inner_thread: &str| RunResumeTicket {
            run_id: RunId(row_run.into()),
            ticket: ticket(inner_run, inner_thread),
        };
        let snapshot = |resume_tickets: Vec<RunResumeTicket>| RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: RunId("r".into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets,
            thread_version: 0,
            store_cursor: 0,
            next_commit_ordinal: 0,
        };
        let run = RunId("r".into());
        for other in [Vec::new(), vec![row("o", "o", "t")]] {
            let with = |mut rows: Vec<RunResumeTicket>| {
                rows.extend(other.iter().cloned());
                snapshot(rows)
            };
            assert!(
                matches!(with(Vec::new()).answerable_ticket(&run), Ok(None)),
                "T1"
            );
            let exact = with(vec![row("r", "r", "t")]);
            assert_eq!(
                exact
                    .answerable_ticket(&run)
                    .expect("T2")
                    .map(|ticket| &ticket.run_id),
                Some(&run),
                "T2"
            );
            for (inner_run, inner_thread) in [("x", "t"), ("r", "x")] {
                assert!(
                    matches!(
                        with(vec![row("r", inner_run, inner_thread)]).answerable_ticket(&run),
                        Err(AnswerableTicketError::Owner { .. })
                    ),
                    "T3 {inner_run}/{inner_thread}"
                );
            }
            for second in [row("r", "r", "t"), row("r", "x", "x")] {
                let two = with(vec![row("r", "r", "t"), second]);
                assert_eq!(two.ticket_for_run(&run), RunTicketSelection::Multiple, "T4");
                assert!(
                    matches!(
                        two.answerable_ticket(&run),
                        Err(AnswerableTicketError::Multiple(_))
                    ),
                    "T4"
                );
            }
        }
    }

    #[test]
    fn current_run_is_the_latest_commit_or_an_explicit_failure() {
        // Causes: C1 latest coordinate present; C2 committed Run vector empty;
        // C3 latest coordinate names a Run in the vector; C4 claimed Run differs
        // from latest. Effects: E1 no current Run; E2 the latest record; E3
        // LatestOmitted; E4 LatestStateMissing. Rules: R1 !C1+C2 => E1;
        // R2 !C1+!C2 => E3 (even when the claimed Run is in the vector);
        // R3 C1+C3 => E2 regardless of C4; R4 C1+!C3 => E4.
        let thread = ThreadId("t".into());
        let record = |id: &str, state: RunState| RunRecord {
            id: RunId(id.into()),
            thread_id: thread.clone(),
            state,
        };
        let snapshot =
            |claimed: &str, latest: Option<&str>, runs: Vec<RunRecord>| RunRecoverySnapshot {
                thread_id: thread.clone(),
                claimed_run_id: RunId(claimed.into()),
                runs,
                latest_run_id: latest.map(|id| RunId(id.into())),
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                commit_timestamps: Default::default(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 0,
                store_cursor: 0,
                next_commit_ordinal: 0,
            };
        let two = || {
            vec![
                record("old", RunState::Awaiting),
                record("new", RunState::Running),
            ]
        };
        assert_eq!(
            snapshot("c", None, Vec::new()).current_run(),
            Ok(None),
            "R1/E1"
        );
        assert_eq!(
            snapshot("old", None, two()).current_run(),
            Err(CurrentRunError::LatestOmitted),
            "R2/E3"
        );
        for claimed in ["new", "old"] {
            assert_eq!(
                snapshot(claimed, Some("new"), two())
                    .current_run()
                    .map(|run| run.map(|run| (run.id.0.clone(), run.state.clone()))),
                Ok(Some(("new".to_string(), RunState::Running))),
                "R3/E2 claimed={claimed}"
            );
        }
        assert_eq!(
            snapshot("new", Some("gone"), two()).current_run(),
            Err(CurrentRunError::LatestStateMissing(RunId("gone".into()))),
            "R4/E4"
        );
    }

    #[test]
    fn run_projection_retains_first_seen_order_and_last_committed_states() {
        // Causes: C1 zero/nonzero facts; C2 repeated vs distinct Run ids; C3
        // interleaved updates; C4 the last commit revisits an earlier Run; C5
        // any decoded state. Effects: empty => ([], None), otherwise one record
        // per id in FIRST appearance order, each state from its LAST fact, and
        // latest_run_id from the LAST commit (not the last distinct Run).
        // Decision rules R0=!C1; R1=C1+!C2; R2=C1+C2; R3=C3+C4. Exhaust all
        // streams of length 0..=5 over two non-lexically-ordered ids and three
        // states. Admission owns legal transitions; this reducer must not add a
        // second transition validator. Direct first/last positions are the oracle.
        let thread = ThreadId("parent-owned-thread".into());
        let alphabet = [
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(crate::agent::run::EndCause::Indeterminate),
        ];
        for length in 0..=5_u32 {
            for mut code in 0..6_usize.pow(length) {
                let input = (0..length)
                    .map(|_| {
                        let symbol = code % 6;
                        code /= 6;
                        (
                            RunId(if symbol / 3 == 0 { "z-run" } else { "a-run" }.into()),
                            alphabet[symbol % 3].clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                let (runs, latest) =
                    project_committed_run_records(&thread, input.iter().cloned().map(Ok::<_, ()>))
                        .unwrap();
                let first_ids = input
                    .iter()
                    .enumerate()
                    .filter(|(i, (id, _))| !input[..*i].iter().any(|(before, _)| before == id))
                    .map(|(_, (id, _))| id.clone())
                    .collect::<Vec<_>>();
                assert_eq!(
                    runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>(),
                    first_ids
                );
                assert_eq!(latest, input.last().map(|(id, _)| id.clone()));
                for run in &runs {
                    assert_eq!(run.thread_id, thread);
                    assert_eq!(
                        &run.state,
                        &input.iter().rev().find(|(id, _)| id == &run.id).unwrap().1
                    );
                }
            }
        }
    }

    #[test]
    fn run_projection_preserves_decoder_failure_and_stops_consumption() {
        // R4 error at first/middle/final fact => return that exact error and no
        // partial snapshot; R5 malformed historical state before a later update
        // of the same id must not be hidden by last-state selection. A second
        // error after it is never read. Counts observe the decoder boundary,
        // not internal lookup mechanics.
        let thread = ThreadId("thread".into());
        for failure_at in 0..=4 {
            let reads = std::cell::Cell::new(0);
            let rows = (0..5).map(|index| {
                reads.set(reads.get() + 1);
                if index == failure_at {
                    Err("first unreadable fact")
                } else if index > failure_at {
                    panic!("decoded beyond the first error")
                } else {
                    Ok((RunId("same-run".into()), RunState::Running))
                }
            });
            assert_eq!(
                project_committed_run_records(&thread, rows).unwrap_err(),
                "first unreadable fact"
            );
            assert_eq!(reads.get(), failure_at + 1);
        }
    }

    #[test]
    fn legacy_snapshot_without_events_remains_readable() {
        // Causes: C1 an older Worker payload omits the newly projected audit
        // prefix. Effect E1: decode succeeds with an empty event list. Decision
        // rule R1=C1=>E1; writers still emit the field for new peers.
        // Constraints/invariants: compatibility defaults only the absent audit
        // prefix and preserves every existing recovery coordinate verbatim.
        let snapshot = RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId("thread".into()),
            claimed_run_id: RunId("run".into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 0,
            store_cursor: 0,
            next_commit_ordinal: 0,
        };
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("events");
        let decoded: RunRecoverySnapshot = serde_json::from_value(legacy).expect("R1/E1");
        assert!(decoded.events.is_empty(), "R1/E1");
    }

    #[test]
    fn persisted_resume_ticket_requires_exact_row_and_partition_ownership() {
        // Cause/effect graph: C1=wire decodes vs is malformed; C2=ticket Run
        // matches its waiting-row Run; C3=ticket Thread matches the recovery
        // partition. Effect E1=return the typed reply authority only for the
        // exact product; E2=reject every damaged product without mutation.
        // Decision table: T1=C1+C2+C3=>E1; T2=!C1=>E2; T3=C1+!C2=>E2;
        // T4=C1+C2+!C3=>E2. This is the single invariant shared by stores and
        // strict protocol admission.
        let run = RunId("ticket-owner-run".into());
        let thread = ThreadId("ticket-owner-thread".into());
        let ticket = ResumeTicket::new(
            "ticket-owner-correlation",
            run.clone(),
            thread.clone(),
            "ticket-owner-snapshot",
            "ticket-owner-catalog",
            crate::agent::awaiting::AwaitTarget::Pause(crate::agent::awaiting::PauseReason::Manual),
        );
        let value = serde_json::to_value(&ticket).unwrap();
        assert_eq!(
            decode_resume_ticket_value_for_owner(value.clone(), &run, &thread).expect("T1/E1"),
            ticket
        );
        assert!(
            decode_resume_ticket_json_for_owner("{}", &run, &thread).is_err(),
            "T2/E2"
        );
        assert!(
            decode_resume_ticket_value_for_owner(
                value.clone(),
                &RunId("another-run".into()),
                &thread,
            )
            .is_err(),
            "T3/E2"
        );
        assert!(
            decode_resume_ticket_value_for_owner(value, &run, &ThreadId("another-thread".into()),)
                .is_err(),
            "T4/E2"
        );
    }
}
