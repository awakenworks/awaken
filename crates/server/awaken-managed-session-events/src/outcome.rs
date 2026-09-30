// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Validates a terminal Outcome against its retained Session provenance and pairs it
// @formal reason: with the root commits that make its facts public, moved unchanged from the Managed
// @formal reason: event projector. Pure; a terminal without both anchors is withheld. The Outcome
// @formal reason: anchor regression and warm/cold Outcome projection tests cover it. Reducer TCB.
//! Terminal Outcomes paired with the root commits that make them public.

use std::collections::HashMap;

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_execution_contract::CommittedOutcomeProjection;

use crate::ProjectionError;

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

/// The terminal state Set and active-pointer Remove must be one committed
/// Thread write. A protocol read may use this key/action/cursor proof without
/// decoding extension-private Outcome JSON or building a second query.
pub fn outcome_terminal_commit_cursor(
    outcome_id: &str,
    snapshot: &RunRecoverySnapshot,
) -> Result<Option<u64>, ProjectionError> {
    if snapshot.state.len() != snapshot.state_commit_cursors.len() {
        return Err(ProjectionError::corrupt(
            "Outcome state recovery omitted commit coordinates",
        ));
    }
    let state_key = format!("outcome/{outcome_id}/state");
    let terminal = snapshot
        .state
        .iter()
        .zip(snapshot.state_commit_cursors.iter().copied())
        .rev()
        .find(|(command, _)| command.key.0 == state_key)
        .and_then(|(command, cursor)| {
            matches!(
                command.action,
                awaken_agent_contract::agent::state::Action::Set(_)
            )
            .then_some(cursor)
        });
    Ok(terminal.filter(|terminal| {
        snapshot
            .state
            .iter()
            .zip(snapshot.state_commit_cursors.iter().copied())
            .any(|(command, cursor)| {
                command.key.0 == "outcome/active"
                    && matches!(
                        command.action,
                        awaken_agent_contract::agent::state::Action::Remove
                    )
                    && cursor == *terminal
            })
    }))
}

impl AnchoredOutcome<'_> {
    /// The root commit of one evaluation iteration's public facts.
    #[must_use]
    pub fn evaluation_commit_cursor(&self, iteration: u32) -> Option<u64> {
        self.evaluation_commit_cursors.get(&iteration).copied()
    }
}

/// Whether an Outcome's recorded failure came from `run_id`, so the Outcome
/// error owns that Run's failure instead of the lifecycle error.
#[must_use]
pub fn outcome_owns_failure_run(
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
pub fn validate_outcome(
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
pub fn anchor_outcome(
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
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    #[test]
    fn outcome_terminal_query_must_match_exact_root_state_and_pointer_commit() {
        // Causes: C1 no Outcome state, active state Set, terminal state Set plus
        // same-commit active Remove, or a torn/wrong-action pair; C2 the typed
        // terminal query is None or Some; C3 state/cursor vectors align or not.
        // Effects: E1 only exact Set+Remove shares a terminal cursor; E2
        // root-terminal/query agreement can publish, a lagging None must retry,
        // and a query ahead of the root is withheld by its missing anchor; E3
        // malformed coordinates fail closed. Rules R1 absent/active+None=>E2;
        // R2 exact terminal+Some=>E1+E2; R3 exact terminal+None=>retry;
        // R4 active+Some=>withhold; R5 torn/wrong-action or a later state
        // Remove=>no terminal;
        // R6 !C3=>E3. No private Outcome
        // JSON or wall time participates in the decision.
        let set = |key| {
            Command::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                key,
                serde_json::json!({}),
            )
        };
        let remove = |key| Command::remove(Scope::Thread, MergePolicy::Disjoint, key);
        let snapshot = |entries: Vec<(Command, u64)>| {
            let (state, state_commit_cursors) = entries.into_iter().unzip();
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: ThreadId("root".into()),
                claimed_run_id: RunId("outcome-run".into()),
                runs: Vec::new(),
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state,
                state_commit_cursors,
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 30,
                next_commit_ordinal: 0,
            }
        };
        let active = snapshot(vec![(set("outcome/id/state"), 10)]);
        let terminal = snapshot(vec![
            (set("outcome/id/state"), 20),
            (remove("outcome/active"), 20),
        ]);
        for (rule, source, terminal_query, expected_cursor, complete) in [
            ("R1 absent", snapshot(Vec::new()), false, None, true),
            ("R1 active", active.clone(), false, None, true),
            ("R2 terminal", terminal.clone(), true, Some(20), true),
            ("R3 stale query", terminal, false, Some(20), false),
            ("R4 ahead query", active, true, None, true),
            (
                "R4 torn",
                snapshot(vec![
                    (set("outcome/id/state"), 20),
                    (remove("outcome/active"), 21),
                ]),
                false,
                None,
                true,
            ),
            (
                "R4 wrong state action",
                snapshot(vec![
                    (remove("outcome/id/state"), 20),
                    (remove("outcome/active"), 20),
                ]),
                false,
                None,
                true,
            ),
            (
                "R5 terminal state removed later",
                snapshot(vec![
                    (set("outcome/id/state"), 20),
                    (remove("outcome/active"), 20),
                    (remove("outcome/id/state"), 21),
                ]),
                false,
                None,
                true,
            ),
        ] {
            let cursor = outcome_terminal_commit_cursor("id", &source).unwrap();
            assert_eq!(cursor, expected_cursor, "{rule} exact source");
            assert_eq!(
                cursor.is_none() || terminal_query,
                complete,
                "{rule} lag coverage"
            );
        }
        let mut corrupt = snapshot(vec![(set("outcome/id/state"), 20)]);
        corrupt.state_commit_cursors.clear();
        assert!(
            outcome_terminal_commit_cursor("id", &corrupt).is_err(),
            "R6/E3"
        );
    }
}
