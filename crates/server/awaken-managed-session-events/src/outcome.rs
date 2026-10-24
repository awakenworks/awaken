// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Validates a terminal Outcome against its retained Session provenance and pairs it
// @formal reason: with the root commits that make its facts public, moved unchanged from the Managed
// @formal reason: event projector. Pure; a terminal without both anchors is withheld. The Outcome
// @formal reason: anchor regression and warm/cold Outcome projection tests cover it. Reducer TCB.
//! Terminal Outcomes paired with the root commits that make them public, and
//! the evaluation and error facts each one publishes.

use std::collections::HashMap;

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_execution_contract::{
    CommittedOutcomeProjection, outcome_terminal_commit_cursor,
};

use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, OutcomeVerdict,
    ProjectionError, SessionEventKind,
};

/// The exact root-Thread commit coordinates that make a terminal Outcome's
/// facts public: its terminal fence and each evaluation iteration's command.
/// Keeping the evidence with the value prevents admission and canonical
/// ordering from independently rediscovering (and possibly disagreeing about)
/// the same anchors.
///
/// Consumers: the Outcome evaluation/error facts and their canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeAnchor {
    pub terminal_commit_cursor: u64,
    pub evaluation_commit_cursors: HashMap<u32, u64>,
}

/// A borrowed view of one anchored terminal Outcome.
///
/// Consumer: [`crate::OrderEvidence::outcomes`], in retained order.
#[derive(Clone, Copy, Debug)]
pub struct AnchoredOutcome<'a> {
    pub outcome_id: &'a str,
    pub terminal: &'a CommittedOutcomeProjection,
    pub terminal_commit_cursor: u64,
    pub evaluation_commit_cursors: &'a HashMap<u32, u64>,
}

impl AnchoredOutcome<'_> {
    /// The root commit of one evaluation iteration's public facts.
    #[must_use]
    pub fn evaluation_commit_cursor(&self, iteration: u32) -> Option<u64> {
        self.evaluation_commit_cursors.get(&iteration).copied()
    }
}

/// One terminal Outcome selected by retained Session provenance, validated and
/// paired with the root commits that make its facts public. A terminal the
/// committed root prefix cannot anchor yet is not a `CommittedOutcome`.
///
/// Consumers: the protocol adapter's projection candidate (facts, the Run
/// failure it owns, evaluation completion times) and the order index.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedOutcome {
    pub outcome_id: String,
    pub terminal: CommittedOutcomeProjection,
    pub anchor: OutcomeAnchor,
}

impl CommittedOutcome {
    /// Validate one terminal against its retained provenance, then anchor it
    /// in the root prefix. An invalid terminal fails the candidate; one the
    /// prefix cannot anchor yet is withheld (`Ok(None)`).
    pub fn select(
        outcome_id: String,
        terminal: CommittedOutcomeProjection,
        root: Option<&RunRecoverySnapshot>,
    ) -> Result<Option<Self>, ProjectionError> {
        validate_outcome(&outcome_id, &terminal)?;
        Ok(
            anchor_outcome(&outcome_id, &terminal, root).map(|anchor| Self {
                outcome_id,
                terminal,
                anchor,
            }),
        )
    }

    /// Whether this Outcome's recorded failure came from `run_id`, so the
    /// Outcome error owns that Run's failure instead of the lifecycle error.
    #[must_use]
    pub fn owns_failure_run(&self, run_id: &awaken_agent_contract::agent::run::Id) -> bool {
        outcome_owns_failure_run(&self.terminal, run_id)
    }

    /// The borrowed view the order index positions this Outcome's facts by.
    #[must_use]
    pub fn as_anchored(&self) -> AnchoredOutcome<'_> {
        AnchoredOutcome {
            outcome_id: &self.outcome_id,
            terminal: &self.terminal,
            terminal_commit_cursor: self.anchor.terminal_commit_cursor,
            evaluation_commit_cursors: &self.anchor.evaluation_commit_cursors,
        }
    }

    /// The public facts of this Outcome on `session_id`'s primary Thread: a
    /// completed report publishes a start/ongoing/end trio per evaluation
    /// iteration, in report order; an errored one publishes one error and no
    /// partial evaluation.
    #[must_use]
    pub fn facts(&self, session_id: &str) -> Vec<DerivedSessionEvent> {
        match &self.terminal {
            CommittedOutcomeProjection::Completed(report) => report
                .iterations
                .iter()
                .flat_map(|item| {
                    let key = |role| {
                        EventIdentity::new(
                            session_id,
                            role,
                            FactProvenance::OutcomeEvaluation {
                                outcome_id: self.outcome_id.clone(),
                                iteration: item.iteration,
                            },
                        )
                    };
                    let start = key(ManagedEventType::OutcomeEvaluationStart);
                    [
                        DerivedSessionEvent::new(
                            start.clone(),
                            SessionEventKind::OutcomeEvaluationStart {
                                outcome_id: self.outcome_id.clone(),
                                iteration: item.iteration,
                            },
                        ),
                        DerivedSessionEvent::new(
                            key(ManagedEventType::OutcomeEvaluationOngoing),
                            SessionEventKind::OutcomeEvaluationOngoing {
                                outcome_id: self.outcome_id.clone(),
                                iteration: item.iteration,
                            },
                        ),
                        DerivedSessionEvent::new(
                            key(ManagedEventType::OutcomeEvaluationEnd),
                            SessionEventKind::OutcomeEvaluationEnd(Box::new(OutcomeVerdict {
                                outcome_id: self.outcome_id.clone(),
                                iteration: item.iteration,
                                start,
                                result: item.result.clone(),
                                explanation: item.explanation.clone(),
                                usage: item.usage,
                            })),
                        ),
                    ]
                })
                .collect(),
            CommittedOutcomeProjection::Errored(failure) => vec![DerivedSessionEvent::new(
                EventIdentity::new(
                    session_id,
                    ManagedEventType::OutcomeError,
                    FactProvenance::Outcome {
                        outcome_id: self.outcome_id.clone(),
                    },
                ),
                SessionEventKind::OutcomeError {
                    code: failure.code.clone(),
                    message: failure.message.clone(),
                },
            )],
        }
    }
}

/// Whether an Outcome's recorded failure came from `run_id`, so the Outcome
/// error owns that Run's failure instead of the lifecycle error.
#[must_use]
fn outcome_owns_failure_run(
    terminal: &CommittedOutcomeProjection,
    run_id: &awaken_agent_contract::agent::run::Id,
) -> bool {
    matches!(
        terminal,
        CommittedOutcomeProjection::Errored(failure)
            if failure.source_run_id.as_ref() == Some(run_id)
    )
}

/// A Completed report must belong to its retained Session provenance and list
/// strictly increasing iterations; anything else fails the candidate closed.
fn validate_outcome(
    outcome_id: &str,
    terminal: &CommittedOutcomeProjection,
) -> Result<(), ProjectionError> {
    if let CommittedOutcomeProjection::Completed(report) = terminal {
        let mut previous_iteration = None;
        for iteration in &report.iterations {
            if iteration.outcome_id != outcome_id {
                return Err(ProjectionError::corrupt(
                    "committed Outcome report id disagrees with Session provenance",
                ));
            }
            if previous_iteration.is_some_and(|previous| iteration.iteration <= previous) {
                return Err(ProjectionError::corrupt(
                    "committed Outcome report iterations are not strictly ordered",
                ));
            }
            previous_iteration = Some(iteration.iteration);
        }
    }
    Ok(())
}

/// Pair the terminal query with its exact Outcome-owned root commit. The
/// terminal state update and removal of the active pointer are emitted by one
/// commit, so their shared cursor is a typed terminal fence without decoding
/// extension-private JSON. A cancellation can synthesize its last public
/// `interrupted` cycle without a Grade; that cycle is ordered by the terminal
/// fence, while every ordinary cycle still requires its immutable evaluation
/// command.
#[must_use]
fn anchor_outcome(
    outcome_id: &str,
    terminal: &CommittedOutcomeProjection,
    snapshot: Option<&RunRecoverySnapshot>,
) -> Option<OutcomeAnchor> {
    let snapshot = snapshot?;
    let terminal_commit_cursor = outcome_terminal_commit_cursor(outcome_id, snapshot).ok()??;
    let commands = snapshot
        .state
        .iter()
        .zip(snapshot.state_commit_cursors.iter().copied())
        .collect::<Vec<_>>();
    let mut evaluation_commit_cursors = HashMap::new();
    if let CommittedOutcomeProjection::Completed(report) = terminal {
        for (index, item) in report.iterations.iter().enumerate() {
            let key = format!("outcome/{outcome_id}/evaluation/{}", item.iteration);
            let cursor = commands
                .iter()
                .find_map(|(command, cursor)| (command.key.0 == key).then_some(*cursor))
                .or_else(|| {
                    (index + 1 == report.iterations.len() && item.result == "interrupted")
                        .then_some(terminal_commit_cursor)
                })?;
            evaluation_commit_cursors.insert(item.iteration, cursor);
        }
    }
    Some(OutcomeAnchor {
        terminal_commit_cursor,
        evaluation_commit_cursors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_execution_contract::{OutcomeFailure, OutcomeIteration, OutcomeReport};

    #[test]
    fn outcome_projection_requires_a_terminal_fence_and_anchors_synthetic_interrupts() {
        // Cause/effect graph: C1 a terminal state command has no same-commit active
        // pointer removal; C2 an ordinary evaluation has its immutable command; C3
        // cancellation synthesizes a final interrupted cycle with no Grade command;
        // C4 a non-interrupted cycle lacks its evaluation command. Effects: E1 C1
        // remains invisible; E2 C2 uses the evaluation cursor; E3 C3 uses the exact
        // terminal cursor; E4 C4 fails closed. Decision table: R1=C1=>E1;
        // R2=terminal+C2+C3=>E2+E3; R3=terminal+C4=>E4.
        // Invariants: admission and ordering share one anchored value; no JSON state
        // payload is decoded, and a prior non-terminal state update cannot anchor a
        // newer terminal observation read through a different port.
        use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};

        let outcome_id = "outcome-anchor";
        let set = |key: String| {
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                key,
                serde_json::json!({"opaque": true}),
            )
        };
        let terminal_commands = || {
            vec![
                (set(format!("outcome/{outcome_id}/evaluation/0")), 10),
                (set(format!("outcome/{outcome_id}/state")), 20),
                (
                    Command::remove(Scope::Thread, MergePolicy::Disjoint, "outcome/active"),
                    20,
                ),
            ]
        };
        let snapshot = |entries: Vec<(Command, u64)>| {
            let (state, state_commit_cursors) = entries.into_iter().unzip();
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: awaken_agent_contract::agent::thread::Id("outcome-root".into()),
                claimed_run_id: awaken_agent_contract::agent::run::Id("outcome-state".into()),
                runs: Vec::new(),
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state,
                state_commit_cursors,
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 20,
                next_commit_ordinal: 0,
            }
        };
        let projection = |iterations: &[(u32, &str)]| {
            CommittedOutcomeProjection::Completed(
                awaken_session_execution_contract::OutcomeReport {
                    iterations: iterations
                        .iter()
                        .map(|(iteration, result)| {
                            awaken_session_execution_contract::OutcomeIteration {
                                messages: Vec::new(),
                                outcome_id: outcome_id.into(),
                                description: "ship".into(),
                                iteration: *iteration,
                                result: (*result).into(),
                                explanation: "test".into(),
                                usage: None,
                            }
                        })
                        .collect(),
                },
            )
        };
        let select = |terminal, snapshot| {
            CommittedOutcome::select(outcome_id.into(), terminal, Some(snapshot)).unwrap()
        };

        let without_terminal = snapshot(vec![(set(format!("outcome/{outcome_id}/state")), 10)]);
        assert!(
            select(projection(&[(0, "satisfied")]), &without_terminal).is_none(),
            "R1/E1"
        );

        let terminal = snapshot(terminal_commands());
        let anchored = select(
            projection(&[(0, "needs_revision"), (1, "interrupted")]),
            &terminal,
        )
        .expect("R2 terminal projection")
        .anchor;
        assert_eq!(anchored.evaluation_commit_cursors.get(&0), Some(&10));
        assert_eq!(anchored.evaluation_commit_cursors.get(&1), Some(&20));
        assert_eq!(anchored.terminal_commit_cursor, 20, "R2/E2+E3");

        assert!(
            select(
                projection(&[(0, "needs_revision"), (1, "satisfied")]),
                &terminal
            )
            .is_none(),
            "R3/E4"
        );
    }

    #[test]
    fn outcome_facts_are_one_trio_per_iteration_or_one_error() {
        // Causes: C1 a Completed report with iterations 0 and 2 (graded with
        // and without usage); C2 an Errored terminal; C3 a report whose
        // iteration names another Outcome. Effects: E1 C1 publishes
        // start/ongoing/end per iteration in report order on the primary
        // Thread, each end naming its own start and carrying its verdict and
        // usage; E2 C2 publishes exactly one error with its code and message
        // and no evaluation; E3 C3 is refused before any fact exists.
        // Rules: R1=C1=>E1; R2=C2=>E2; R3=C3=>E3.
        let item = |iteration, usage| OutcomeIteration {
            messages: Vec::new(),
            outcome_id: "outcome".into(),
            description: "ship".into(),
            iteration,
            result: "satisfied".into(),
            explanation: format!("round {iteration}"),
            usage,
        };
        let usage = awaken_agent_contract::audit::model_request::TokenUsage {
            prompt_tokens: 3,
            ..Default::default()
        };
        let anchor = OutcomeAnchor {
            terminal_commit_cursor: 30,
            evaluation_commit_cursors: HashMap::from([(0, 10), (2, 20)]),
        };
        let completed = CommittedOutcome {
            outcome_id: "outcome".into(),
            terminal: CommittedOutcomeProjection::Completed(OutcomeReport {
                iterations: vec![item(0, Some(usage)), item(2, None)],
            }),
            anchor: anchor.clone(),
        };
        let facts = completed.facts("session");
        assert_eq!(facts.len(), 6, "R1/E1 one trio per iteration");
        for (trio, (iteration, expected_usage)) in
            facts.chunks(3).zip([(0, Some(usage)), (2, None)])
        {
            let start = &trio[0].key;
            assert!(
                trio.iter().all(|fact| fact.key.thread == "session"),
                "R1/E1 owner"
            );
            assert_eq!(
                trio.iter().map(|fact| fact.key.role).collect::<Vec<_>>(),
                [
                    ManagedEventType::OutcomeEvaluationStart,
                    ManagedEventType::OutcomeEvaluationOngoing,
                    ManagedEventType::OutcomeEvaluationEnd,
                ],
                "R1/E1 trio order"
            );
            assert_eq!(
                trio[2].kind,
                SessionEventKind::OutcomeEvaluationEnd(Box::new(OutcomeVerdict {
                    outcome_id: "outcome".into(),
                    iteration,
                    start: start.clone(),
                    result: "satisfied".into(),
                    explanation: format!("round {iteration}"),
                    usage: expected_usage,
                })),
                "R1/E1 end of iteration {iteration}"
            );
        }

        let errored = CommittedOutcome {
            outcome_id: "outcome".into(),
            terminal: CommittedOutcomeProjection::Errored(OutcomeFailure {
                code: "outcome_failed".into(),
                message: "grader crashed".into(),
                source_run_id: None,
            }),
            anchor,
        };
        assert_eq!(
            errored.facts("session"),
            vec![DerivedSessionEvent::new(
                EventIdentity::new(
                    "session",
                    ManagedEventType::OutcomeError,
                    FactProvenance::Outcome {
                        outcome_id: "outcome".into()
                    },
                ),
                SessionEventKind::OutcomeError {
                    code: "outcome_failed".into(),
                    message: "grader crashed".into(),
                },
            )],
            "R2/E2"
        );

        let mut foreign = item(0, None);
        foreign.outcome_id = "other".into();
        assert!(
            CommittedOutcome::select(
                "outcome".into(),
                CommittedOutcomeProjection::Completed(OutcomeReport {
                    iterations: vec![foreign],
                }),
                None,
            )
            .is_err(),
            "R3/E3"
        );
    }
}
