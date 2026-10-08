// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Validates a terminal Outcome against its retained Session provenance and pairs it
// @formal reason: with the root commits that make its facts public, moved unchanged from the Managed
// @formal reason: event projector. Pure; a terminal without both anchors is withheld. The Outcome
// @formal reason: anchor regression and warm/cold Outcome projection tests cover it. Reducer TCB.
//! Terminal Outcomes paired with the root commits that make them public.

use std::collections::HashMap;

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_execution_contract::{CommittedOutcomeProjection, outcome_terminal_commit_cursor};

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
