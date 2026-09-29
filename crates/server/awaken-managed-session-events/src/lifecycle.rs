// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Coordinated child Thread lifecycle: the status machine of each child and
//! the facts its committed Run lifecycle, disposition and the parent terminal
//! publish.
//!
//! Consumer: the protocol adapter's delegation projector, which feeds each
//! child's published status in, encodes the returned facts in order and writes
//! the advanced status back to its Thread record.

use std::collections::HashMap;

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_application_contract::Pending;
use awaken_session_event_contract::SessionAgentRunBoundary;

use crate::fault::run_fault;
use crate::stop::terminal_stop;
use crate::{FactKey, FactKind, FactProvenance, FactRole, ProjectionError, TimelineFact};

/// The published status of one coordinated Thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    Running,
    Rescheduling,
    Idle,
    Terminated,
}

/// One coordinated child Thread as the timeline has published it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatedThread {
    pub thread: String,
    /// An Advisor consultation owns one Thread and closes after its one Run.
    pub advisor: bool,
    pub status: ThreadStatus,
}

/// The facts one lifecycle suffix publishes and the Run boundaries it
/// consumed.
#[derive(Debug, Default)]
pub struct ChildLifecycle {
    pub facts: Vec<TimelineFact>,
    /// Every Run boundary projected as a Thread terminal.
    pub terminal_cursors: Vec<RunLifecycleCursor>,
}

/// Every primary Thread opening in the complete accepted lifecycle prefix.
///
/// Consumer: the Managed wire encoder. A boundary with no opening since the
/// previous boundary needs its own synthetic Running fact even if a live
/// replica saw the earlier boundary. Decisions never read published events,
/// terminal checkpoints, or a current-status cache. The committed cursor is
/// the exact existing public identity; later lifecycle facts only append.
#[must_use]
pub fn root_lifecycle_opening_facts(
    session_id: &str,
    lifecycle: &[RunLifecycleEvent],
) -> Vec<TimelineFact> {
    let mut facts = Vec::new();
    let mut open = false;
    for event in lifecycle
        .iter()
        .filter(|event| event.thread_id.0 == session_id)
    {
        let at = |role, kind| {
            TimelineFact::new(
                FactKey::new(
                    session_id,
                    role,
                    FactProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                ),
                kind,
            )
        };
        if !crate::is_opening(event.kind) {
            if SessionAgentRunBoundary::is_boundary(event.kind) {
                if !open {
                    facts.push(at(
                        FactRole::RootTerminalStatusRunning,
                        FactKind::ThreadRunning,
                    ));
                }
                open = false;
            }
            continue;
        }
        match event.kind {
            RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed => {
                facts.push(at(
                    FactRole::RootLifecycleStatusRunning,
                    FactKind::ThreadRunning,
                ));
                open = true;
            }
            RunLifecycleEventKind::Rescheduled => {
                facts.push(at(
                    FactRole::RootLifecycleSessionStatusRescheduled,
                    FactKind::SessionRescheduled,
                ));
                facts.push(at(
                    FactRole::RootLifecycleStatusRescheduled,
                    FactKind::ThreadRescheduled,
                ));
                facts.push(at(
                    FactRole::RootRescheduledLifecycleStatusRunning,
                    FactKind::ThreadRunning,
                ));
                open = true;
            }
            _ => unreachable!("the canonical opening law admitted only Run openings"),
        }
    }
    facts
}

fn find<'a>(
    threads: &'a mut [CoordinatedThread],
    thread: &str,
) -> Option<&'a mut CoordinatedThread> {
    threads.iter_mut().find(|child| child.thread == thread)
}

/// Project a lifecycle suffix onto the coordinated children it names.
///
/// Rules: a terminated child absorbs every later commit; an opening publishes
/// Running unless the child already runs; a Rescheduled receipt publishes the
/// retry and its replacement opening under one cursor; a Run boundary
/// publishes the Run's fault (if any) and then Idle, or Terminated directly
/// for a failed ordinary Run; an Advisor consultation terminates after its
/// Idle. An Awaiting whose committed audit names no answerable target
/// publishes nothing: `requires_action` with no answerable call would be a
/// false state. Whether a ticket is still live never decides it.
pub fn project_child_lifecycle(
    threads: &mut [CoordinatedThread],
    events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
    snapshots: &HashMap<String, RunRecoverySnapshot>,
) -> Result<ChildLifecycle, ProjectionError> {
    let mut projection = ChildLifecycle::default();
    for event in events {
        let thread_id = event.thread_id.0.as_str();
        let Some(child) = find(threads, thread_id) else {
            continue;
        };
        if child.status == ThreadStatus::Terminated {
            continue;
        }
        let fact = |role, kind| {
            TimelineFact::new(
                FactKey::new(
                    thread_id,
                    role,
                    FactProvenance::Lifecycle {
                        cursor: event.cursor.0,
                    },
                ),
                kind,
            )
        };
        let run_terminates_thread = awaken_session_execution_contract::coordinated_thread_closed(
            awaken_session_execution_contract::CoordinatedThreadKind::from_is_advisor(child.advisor),
            &event.state,
        );
        match SessionAgentRunBoundary::of_lifecycle_kind(event.kind) {
            None if event.kind != RunLifecycleEventKind::Rescheduled => {
                if child.status != ThreadStatus::Running {
                    child.status = ThreadStatus::Running;
                    projection.facts.push(fact(
                        FactRole::LifecycleStatusRunning,
                        FactKind::ThreadRunning,
                    ));
                }
            }
            None => {
                // One committed retry receipt represents both observable edges:
                // the old attempt was rescheduled, then its already claimed
                // replacement began running. Keeping both under the same
                // lifecycle cursor makes warm replay and cold rebuild converge
                // without a protocol-owned retry registry.
                child.status = ThreadStatus::Running;
                projection.facts.push(fact(
                    FactRole::LifecycleStatusRescheduled,
                    FactKind::ThreadRescheduled,
                ));
                projection.facts.push(fact(
                    FactRole::RescheduledLifecycleStatusRunning,
                    FactKind::ThreadRunning,
                ));
            }
            Some(_) => {
                // The pause's answerable target comes from its own committed
                // audit, so it is the same while the ticket is live and after
                // it was consumed.
                let terminal_pending = historical_pending.get(&event.cursor);
                let budget_reached = event.await_reason.as_ref()
                    == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
                if event.kind == RunLifecycleEventKind::Awaiting
                    && terminal_pending.is_none()
                    && !budget_reached
                {
                    continue;
                }
                let stop = terminal_stop(
                    &event.state,
                    terminal_pending,
                    event.await_reason.as_ref(),
                    false,
                );
                if let Some(fault) = run_fault(event, snapshots.get(thread_id))? {
                    projection.facts.push(fact(
                        FactRole::LifecycleError,
                        FactKind::RunError {
                            fault,
                            advisor: child.advisor,
                        },
                    ));
                }
                projection.terminal_cursors.push(event.cursor);
                if run_terminates_thread && !child.advisor {
                    // A committed Failed Run is the absorbing admission fence
                    // for an ordinary coordinated Thread: Idle would promise
                    // follow-up input the Runtime must reject.
                    child.status = ThreadStatus::Terminated;
                    projection.facts.push(fact(
                        FactRole::FailedLifecycleStatusTerminated,
                        FactKind::ThreadTerminated,
                    ));
                    continue;
                }
                child.status = ThreadStatus::Idle;
                projection.facts.push(fact(
                    FactRole::LifecycleStatusIdle,
                    FactKind::ThreadIdle(stop),
                ));
                if run_terminates_thread {
                    // Each consultation owns one Thread and self-terminates
                    // after its one ordinary Run.
                    child.status = ThreadStatus::Terminated;
                    projection.facts.push(fact(
                        FactRole::AdvisorLifecycleStatusTerminated,
                        FactKind::ThreadTerminated,
                    ));
                }
            }
        }
    }
    Ok(projection)
}

/// Archive is an absorbing Thread disposition: every archived child not yet
/// terminated publishes one terminal.
pub fn archive_children(
    threads: &mut [CoordinatedThread],
    dispositions: &HashMap<String, awaken_agent_contract::ThreadDisposition>,
) -> Vec<TimelineFact> {
    threads
        .iter_mut()
        .filter(|child| {
            dispositions.get(&child.thread)
                == Some(&awaken_agent_contract::ThreadDisposition::Archived)
                && child.status != ThreadStatus::Terminated
        })
        .map(|child| {
            child.status = ThreadStatus::Terminated;
            TimelineFact::new(
                FactKey::new(
                    child.thread.clone(),
                    FactRole::ArchivedStatusTerminated,
                    FactProvenance::ArchivedDisposition,
                ),
                FactKind::ThreadTerminated,
            )
        })
        .collect()
}

/// The absorbing parent Session terminal closes every derived Thread: each
/// open child, then the primary Thread, then the Session itself. Already
/// published primary/Session terminals are not repeated.
pub fn terminate_session(
    session_id: &str,
    threads: &mut [CoordinatedThread],
    primary_terminated_published: bool,
    session_terminated_published: bool,
) -> Vec<TimelineFact> {
    let parent = |thread: &str, role| FactKey::new(thread, role, FactProvenance::ParentTerminal);
    let mut facts = threads
        .iter_mut()
        .filter(|child| child.status != ThreadStatus::Terminated)
        .map(|child| {
            child.status = ThreadStatus::Terminated;
            TimelineFact::new(
                parent(&child.thread, FactRole::ParentTerminalChildStatusTerminated),
                FactKind::ThreadTerminated,
            )
        })
        .collect::<Vec<_>>();
    if !primary_terminated_published {
        facts.push(TimelineFact::new(
            parent(session_id, FactRole::ParentTerminalPrimaryStatusTerminated),
            FactKind::ThreadTerminated,
        ));
    }
    if !session_terminated_published {
        facts.push(TimelineFact::new(
            parent(session_id, FactRole::ParentTerminalSessionStatusTerminated),
            FactKind::SessionTerminated,
        ));
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunFault, StopFact};
    use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn event(
        source: u64,
        thread: &str,
        kind: RunLifecycleEventKind,
        state: RunState,
    ) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(source, 0).unwrap(),
            source_commit_cursor: source,
            thread_id: ThreadId(thread.into()),
            run_id: RunId(format!("run-{thread}")),
            kind,
            state,
            await_reason: None,
        }
    }

    fn child(thread: &str, advisor: bool, status: ThreadStatus) -> CoordinatedThread {
        CoordinatedThread {
            thread: thread.into(),
            advisor,
            status,
        }
    }

    fn roles(facts: &[TimelineFact]) -> Vec<(FactRole, FactKind)> {
        facts
            .iter()
            .map(|fact| (fact.key.role, fact.kind.clone()))
            .collect()
    }

    #[test]
    fn primary_openings_are_prefix_stable_across_every_boundary() {
        // Causes: O1 initial boundary without Running; O2 consecutive
        // boundaries, including cancel; O3 actual Running/Resumed between
        // boundaries; O4 a Rescheduled receipt; O5 interleaved child facts.
        // Effects: E1 each root period lacking an actual opening gets exactly
        // its boundary-owned Running key; E2 actual openings retain their
        // lifecycle roles, including all three retry edges; E3 every shorter
        // committed prefix's facts are an exact prefix of the longer one.
        // Rules R1=O1/O2=>E1; R2=O3/O4=>E2; R3=O5=>no root change;
        // R4=every R1-R3 prefix extension=>E3. No published tail, timestamp,
        // ticket or current status is an input to the production projector.
        use RunLifecycleEventKind::*;
        let e = |source, thread, kind| {
            let state = match kind {
                Awaiting => RunState::Awaiting,
                Completed => RunState::Ended(EndCause::NaturalEnd),
                Cancelled => RunState::Ended(EndCause::Cancelled),
                _ => RunState::Running,
            };
            event(source, thread, kind, state)
        };
        let cases = [
            (
                "R1 consecutive waits and cancel",
                vec![
                    e(1, "root", Awaiting),
                    e(2, "root", Awaiting),
                    e(3, "root", Cancelled),
                ],
                vec![
                    (FactRole::RootTerminalStatusRunning, 1),
                    (FactRole::RootTerminalStatusRunning, 2),
                    (FactRole::RootTerminalStatusRunning, 3),
                ],
            ),
            (
                "R2 actual reopening",
                vec![
                    e(1, "root", Running),
                    e(2, "root", Awaiting),
                    e(3, "root", Resumed),
                    e(4, "root", Completed),
                ],
                vec![
                    (FactRole::RootLifecycleStatusRunning, 1),
                    (FactRole::RootLifecycleStatusRunning, 3),
                ],
            ),
            (
                "R2 retry has three roles",
                vec![
                    e(1, "root", Running),
                    e(2, "root", Rescheduled),
                    e(3, "root", Awaiting),
                ],
                vec![
                    (FactRole::RootLifecycleStatusRunning, 1),
                    (FactRole::RootLifecycleSessionStatusRescheduled, 2),
                    (FactRole::RootLifecycleStatusRescheduled, 2),
                    (FactRole::RootRescheduledLifecycleStatusRunning, 2),
                ],
            ),
            (
                "R3 child cannot open primary",
                vec![
                    e(1, "child", Running),
                    e(2, "root", Awaiting),
                    e(3, "child", Cancelled),
                    e(4, "root", Cancelled),
                ],
                vec![
                    (FactRole::RootTerminalStatusRunning, 2),
                    (FactRole::RootTerminalStatusRunning, 4),
                ],
            ),
        ];
        for (rule, events, expected) in cases {
            let mut previous = Vec::new();
            for end in 0..=events.len() {
                let facts = root_lifecycle_opening_facts("root", &events[..end]);
                assert!(facts.starts_with(&previous), "R4/E3 {rule} prefix {end}");
                previous = facts;
            }
            let actual = previous
                .iter()
                .map(|fact| {
                    assert_eq!(fact.key.thread, "root", "{rule} root scope");
                    let FactProvenance::Lifecycle { cursor } = fact.key.provenance else {
                        panic!("{rule} requires a committed lifecycle identity");
                    };
                    (fact.key.role, cursor)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                expected
                    .iter()
                    .map(|(role, source)| (*role, e(*source, "root", Running).cursor.0))
                    .collect::<Vec<_>>(),
                "R1-R3/E1-E2 {rule}"
            );
            assert_eq!(
                previous
                    .iter()
                    .map(|fact| &fact.key)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                previous.len(),
                "each event id has one fact owner"
            );
        }
    }

    #[test]
    fn child_lifecycle_decision_table() {
        // Causes: C1 the child's published status; C2 the lifecycle kind; C3
        // ordinary child or Advisor; C4 an audited answerable target for the
        // pause. Effects: the
        // published facts in order and the child's advanced status.
        use RunLifecycleEventKind::*;
        let pending = Pending {
            tool_use_id: "call".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let failed = RunState::Ended(EndCause::Error(Failure::StateConflict));
        let natural = RunState::Ended(EndCause::NaturalEnd);
        for (rule, status, advisor, lifecycle, ticket, expected, final_status) in [
            (
                "R1 opening",
                ThreadStatus::Idle,
                false,
                event(1, "c", Running, RunState::Running),
                false,
                vec![(FactRole::LifecycleStatusRunning, FactKind::ThreadRunning)],
                ThreadStatus::Running,
            ),
            (
                "R2 already running",
                ThreadStatus::Running,
                false,
                event(1, "c", Resumed, RunState::Running),
                false,
                vec![],
                ThreadStatus::Running,
            ),
            (
                "R3 retry receipt",
                ThreadStatus::Running,
                false,
                event(1, "c", Rescheduled, RunState::Running),
                false,
                vec![
                    (
                        FactRole::LifecycleStatusRescheduled,
                        FactKind::ThreadRescheduled,
                    ),
                    (
                        FactRole::RescheduledLifecycleStatusRunning,
                        FactKind::ThreadRunning,
                    ),
                ],
                ThreadStatus::Running,
            ),
            (
                "R4 natural end",
                ThreadStatus::Running,
                false,
                event(2, "c", Completed, natural.clone()),
                false,
                vec![(
                    FactRole::LifecycleStatusIdle,
                    FactKind::ThreadIdle(StopFact::EndTurn),
                )],
                ThreadStatus::Idle,
            ),
            (
                "R5 failed ordinary Run",
                ThreadStatus::Running,
                false,
                event(2, "c", Failed, failed.clone()),
                false,
                vec![
                    (
                        FactRole::LifecycleError,
                        FactKind::RunError {
                            fault: RunFault::Failed {
                                code: Failure::StateConflict.code().into(),
                                message: Failure::StateConflict.message(),
                            },
                            advisor: false,
                        },
                    ),
                    (
                        FactRole::FailedLifecycleStatusTerminated,
                        FactKind::ThreadTerminated,
                    ),
                ],
                ThreadStatus::Terminated,
            ),
            (
                "R6 Advisor closes after its Run",
                ThreadStatus::Running,
                true,
                event(2, "c", Completed, natural.clone()),
                false,
                vec![
                    (
                        FactRole::LifecycleStatusIdle,
                        FactKind::ThreadIdle(StopFact::EndTurn),
                    ),
                    (
                        FactRole::AdvisorLifecycleStatusTerminated,
                        FactKind::ThreadTerminated,
                    ),
                ],
                ThreadStatus::Terminated,
            ),
            (
                "R7 answerable pause",
                ThreadStatus::Running,
                false,
                event(2, "c", Awaiting, RunState::Awaiting),
                true,
                vec![(
                    FactRole::LifecycleStatusIdle,
                    FactKind::ThreadIdle(StopFact::RequiresAction {
                        calls: vec!["call".into()],
                    }),
                )],
                ThreadStatus::Idle,
            ),
            (
                "R8 unaudited pause publishes nothing",
                ThreadStatus::Running,
                false,
                event(2, "c", Awaiting, RunState::Awaiting),
                false,
                vec![],
                ThreadStatus::Running,
            ),
            (
                "R9 terminated absorbs",
                ThreadStatus::Terminated,
                false,
                event(3, "c", Running, RunState::Running),
                false,
                vec![],
                ThreadStatus::Terminated,
            ),
            (
                "R10 unknown Thread",
                ThreadStatus::Running,
                false,
                event(3, "other", Running, RunState::Running),
                false,
                vec![],
                ThreadStatus::Running,
            ),
        ] {
            let mut threads = vec![child("c", advisor, status)];
            let audited = if ticket {
                HashMap::from([(lifecycle.cursor, pending.clone())])
            } else {
                HashMap::new()
            };
            let projected = project_child_lifecycle(
                &mut threads,
                std::slice::from_ref(&lifecycle),
                &audited,
                &HashMap::new(),
            )
            .unwrap();
            assert_eq!(roles(&projected.facts), expected, "{rule}");
            assert_eq!(threads[0].status, final_status, "{rule}");
            let boundary =
                SessionAgentRunBoundary::is_boundary(lifecycle.kind) && !expected.is_empty();
            assert_eq!(
                projected.terminal_cursors.len(),
                usize::from(boundary),
                "{rule}"
            );
        }
    }

    #[test]
    fn archive_and_parent_terminal_close_each_open_thread_once() {
        let mut threads = vec![
            child("a", false, ThreadStatus::Idle),
            child("b", false, ThreadStatus::Terminated),
        ];
        let dispositions = HashMap::from([
            (
                "a".to_string(),
                awaken_agent_contract::ThreadDisposition::Archived,
            ),
            (
                "b".to_string(),
                awaken_agent_contract::ThreadDisposition::Archived,
            ),
        ]);
        let archived = archive_children(&mut threads, &dispositions);
        assert_eq!(
            archived
                .iter()
                .map(|fact| fact.key.thread.as_str())
                .collect::<Vec<_>>(),
            ["a"],
            "A1 only the open archived child"
        );
        let mut threads = vec![
            child("a", false, ThreadStatus::Idle),
            child("b", false, ThreadStatus::Terminated),
        ];
        for (primary, session, expected) in [
            (
                false,
                false,
                vec![
                    FactRole::ParentTerminalChildStatusTerminated,
                    FactRole::ParentTerminalPrimaryStatusTerminated,
                    FactRole::ParentTerminalSessionStatusTerminated,
                ],
            ),
            (true, true, vec![]),
        ] {
            let facts = terminate_session("s", &mut threads, primary, session);
            assert_eq!(
                facts.iter().map(|fact| fact.key.role).collect::<Vec<_>>(),
                expected
            );
        }
    }
}
