// @formal surface requirement=runtime.outcomes.outcome_lifecycle
// @formal reason: Static owner: the Outcome classification of a terminal Worker or acknowledgment Run
// @formal reason: from its committed state and durable settlement; an exhaustive decision table test
// @formal reason: covers every row. This is a product-requirement classification, not a proof claim.
//! How an Outcome classifies one terminal Worker or acknowledgment Run.
//!
//! The committed Run state is the terminal authority for the Run itself. One
//! fact it cannot carry is a user interrupt that reached a Run whose
//! cancellation claim found an unconfirmed predecessor: that Run commits
//! `Error`/`Indeterminate`, and only its durable delivery settlement retains
//! the cancel request. The Outcome reads that settlement through
//! [`OutcomeRuns`] and classifies here, in one place.

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_runtime_contract::execution::RunExecutor;

/// The durable delivery settlement of one Run, as the Outcome needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSettlement {
    /// Delivery has not settled yet. A fault classification waits for it, so
    /// a cancel request that is still on its way to the settlement is never
    /// read as a Worker failure.
    Unsettled,
    /// The cancel fact is durable: read from the settled completion, or from
    /// the still-live dispatch row when the Run fault committed before settle.
    Settled { cancellation_requested: bool },
}

/// The Run port an Outcome drives its Worker and acknowledgment Runs through:
/// execution, plus the settlement read the committed Run state cannot answer.
#[async_trait::async_trait]
pub trait OutcomeRuns: RunExecutor {
    /// Read the settlement of `run_id`, which has a committed terminal state.
    /// Executors without durable delivery report `Settled` with no request.
    async fn settlement(
        &self,
        run_id: &RunId,
    ) -> awaken_runtime_contract::execution::Result<RunSettlement>;
}

/// The Outcome transition one terminal Worker or acknowledgment Run selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunVerdict {
    Completed,
    Interrupted,
    Awaiting,
    Failed,
}

/// Why a terminal Run has no verdict yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Undecided {
    /// Read the settlement, then classify again.
    NeedsSettlement,
    /// The settlement is not durable yet; retry the drive later.
    Unsettled,
}

/// Classify one Worker or acknowledgment Run (user decision B1, 2026-10-31).
/// `settlement` is `None` until the controller has read it; only a fault
/// asks for it. A Run fault is published by the Run lifecycle from the
/// committed Run itself, so an interrupted Outcome never adds or withdraws one.
///
/// | Rule | committed Run state             | settlement cancel | verdict                          |
/// |------|---------------------------------|-------------------|----------------------------------|
/// | R1   | Ended(NaturalEnd)               | not read          | Completed                        |
/// | R2   | Ended(Cancelled)                | true              | Interrupted                      |
/// | R3   | Ended(Indeterminate)            | true              | Interrupted; Run fault once      |
/// | R4   | Ended(Error)                    | true              | Interrupted; Run fault once      |
/// | R5   | Ended(Cancelled)                | false             | Interrupted                      |
/// | R6   | Ended(Error / Indeterminate)    | false             | Failed (WorkerFailed)            |
/// | R7   | Awaiting; Ended(MaxSteps / Stopped); Running | not read | unchanged: Awaiting / Failed |
/// | F1   | Ended(Error / Indeterminate)    | not read yet      | Undecided: NeedsSettlement       |
/// | F2   | Ended(Error / Indeterminate)    | Unsettled         | Undecided: Unsettled (fail closed) |
///
/// R2 and R5 need no read: a committed `Cancelled` is already the interrupt.
pub(crate) fn classify_run(
    state: &RunState,
    settlement: Option<RunSettlement>,
) -> Result<RunVerdict, Undecided> {
    match state {
        RunState::Ended(EndCause::NaturalEnd) => Ok(RunVerdict::Completed),
        RunState::Ended(EndCause::Cancelled) => Ok(RunVerdict::Interrupted),
        RunState::Ended(EndCause::Error(_) | EndCause::Indeterminate) => match settlement {
            None => Err(Undecided::NeedsSettlement),
            Some(RunSettlement::Unsettled) => Err(Undecided::Unsettled),
            Some(RunSettlement::Settled {
                cancellation_requested: true,
            }) => Ok(RunVerdict::Interrupted),
            Some(RunSettlement::Settled {
                cancellation_requested: false,
            }) => Ok(RunVerdict::Failed),
        },
        RunState::Awaiting => Ok(RunVerdict::Awaiting),
        _ => Ok(RunVerdict::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Failure;

    const CANCELLED: Option<RunSettlement> = Some(RunSettlement::Settled {
        cancellation_requested: true,
    });
    const PLAIN: Option<RunSettlement> = Some(RunSettlement::Settled {
        cancellation_requested: false,
    });

    /// Rule label, committed Run state, settlement read, expected result.
    type Row = (
        &'static str,
        RunState,
        Option<RunSettlement>,
        Result<RunVerdict, Undecided>,
    );

    fn error() -> RunState {
        RunState::Ended(EndCause::Error(Failure::CapabilityBound))
    }

    #[test]
    fn worker_run_classification_follows_the_settled_cancel_fact() {
        // Every row of the table on `classify_run`; each label names its rule.
        let rows: Vec<Row> = vec![
            (
                "R1",
                RunState::Ended(EndCause::NaturalEnd),
                None,
                Ok(RunVerdict::Completed),
            ),
            (
                "R1 cancel after natural end",
                RunState::Ended(EndCause::NaturalEnd),
                CANCELLED,
                Ok(RunVerdict::Completed),
            ),
            (
                "R2",
                RunState::Ended(EndCause::Cancelled),
                CANCELLED,
                Ok(RunVerdict::Interrupted),
            ),
            (
                "R3",
                RunState::Ended(EndCause::Indeterminate),
                CANCELLED,
                Ok(RunVerdict::Interrupted),
            ),
            ("R4", error(), CANCELLED, Ok(RunVerdict::Interrupted)),
            (
                "R5",
                RunState::Ended(EndCause::Cancelled),
                PLAIN,
                Ok(RunVerdict::Interrupted),
            ),
            (
                "R5 unread",
                RunState::Ended(EndCause::Cancelled),
                None,
                Ok(RunVerdict::Interrupted),
            ),
            ("R6 error", error(), PLAIN, Ok(RunVerdict::Failed)),
            (
                "R6 indeterminate",
                RunState::Ended(EndCause::Indeterminate),
                PLAIN,
                Ok(RunVerdict::Failed),
            ),
            (
                "R7 awaiting",
                RunState::Awaiting,
                CANCELLED,
                Ok(RunVerdict::Awaiting),
            ),
            (
                "R7 max steps",
                RunState::Ended(EndCause::MaxSteps),
                CANCELLED,
                Ok(RunVerdict::Failed),
            ),
            (
                "R7 stopped",
                RunState::Ended(EndCause::Stopped("budget".into())),
                CANCELLED,
                Ok(RunVerdict::Failed),
            ),
            (
                "R7 running",
                RunState::Running,
                None,
                Ok(RunVerdict::Failed),
            ),
            ("F1 error", error(), None, Err(Undecided::NeedsSettlement)),
            (
                "F1 indeterminate",
                RunState::Ended(EndCause::Indeterminate),
                None,
                Err(Undecided::NeedsSettlement),
            ),
            (
                "F2",
                RunState::Ended(EndCause::Indeterminate),
                Some(RunSettlement::Unsettled),
                Err(Undecided::Unsettled),
            ),
        ];
        for (rule, state, settlement, verdict) in rows {
            assert_eq!(classify_run(&state, settlement), verdict, "{rule}");
        }
    }
}
