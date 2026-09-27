//! Durable evidence used to decide whether a crashed Native attempt has any
//! execution participant left after its Worker process is independently gone.
//!
//! This is a projection over the existing ThreadCommit log. It is not a second
//! receipt store and grants no Dispatch authority.

use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey, Store};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use serde::{Deserialize, Serialize};

use crate::model_operation::ActiveModelOperation;
use crate::tool_batch::{ActiveToolBatch, store_from_run_commands};

/// Closed execution surface whose every possible external entry is journalled
/// by the Native runtime. The first supported profile intentionally contains no
/// model-visible tools or plugin execution callbacks. Model invocations still
/// use `ActiveModelOperation`; an empty model history is meaningful only after
/// this marker was committed before the first possible invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeAttemptCoverage {
    #[default]
    Unspecified,
    ModelOnlyV1,
}

/// Run-scoped coverage marker stored through the canonical ThreadCommit log.
pub struct ActiveNativeAttemptCoverage;

impl StateKey for ActiveNativeAttemptCoverage {
    const KEY: &'static str = "runtime.native_attempt_coverage.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = NativeAttemptCoverage;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAttemptTermination {
    Unknown,
    AllExecutionStopped,
}

const fn coverage_precedes_model(coverage_commit: u64, model_commit: u64) -> bool {
    coverage_commit > 0 && coverage_commit < model_commit
}

#[cfg(kani)]
#[kani::proof]
fn native_coverage_must_be_committed_strictly_before_model_entry() {
    // Actual production chronology predicate, full u64 domain. Same commit,
    // missing coordinate and reversed order never prove pre-effect enrollment.
    let coverage: u64 = kani::any();
    let model: u64 = kani::any();
    let precedes = coverage_precedes_model(coverage, model);
    assert_eq!(precedes, coverage != 0 && model > coverage);
    if coverage == model || coverage == 0 || coverage == u64::MAX {
        assert!(!precedes);
    }
}

/// Final closed decision after the append-only evidence has been decoded.
/// Keeping this truth table separate from state parsing makes the safety
/// condition directly exhaustible while the public classifier remains its only
/// production caller.
#[must_use]
const fn termination_from_facts(
    coverage_installed: bool,
    model_lacks_prior_coverage: bool,
    all_models_stopped: bool,
    tool_batch_absent: bool,
) -> NativeAttemptTermination {
    if coverage_installed && !model_lacks_prior_coverage && all_models_stopped && tool_batch_absent
    {
        NativeAttemptTermination::AllExecutionStopped
    } else {
        NativeAttemptTermination::Unknown
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeAttemptTerminationError {
    #[error("recovery snapshot does not describe its claimed Run")]
    RunMismatch,
    #[error("Native attempt evidence has unaligned or unordered commit coordinates")]
    InvalidCommitCoordinates,
    #[error("Native attempt coverage history is missing or invalid")]
    InvalidCoverage,
    #[error("Native model execution history is invalid: {0}")]
    InvalidModelHistory(String),
    #[error("Native tool execution history is invalid: {0}")]
    InvalidToolHistory(String),
}

/// Classify only the deliberately closed model-only Native profile.
///
/// Cause/effect decision table (owned by the executable tests below):
/// C1 exact snapshot Run; C2 aligned monotone commit coordinates; C3 coverage
/// marker exists before every model record; C4 ToolBatch absent; C5 every
/// recorded model invocation has positive terminal/not-dispatched evidence.
/// R1=C1+C2+C3+C4+C5 => all execution is stopped once the Worker process exit
/// is independently proven. R2=!C1|!C2|invalid history => error. Every other
/// combination => Unknown. Empty model history is safe only in R1 because C3
/// proves the runtime installed its entry barrier before effects were possible.
pub fn classify_native_attempt_termination(
    snapshot: &RunRecoverySnapshot,
) -> Result<NativeAttemptTermination, NativeAttemptTerminationError> {
    if snapshot.claimed_run_id.0.trim().is_empty()
        || !snapshot
            .runs
            .iter()
            .any(|run| run.id == snapshot.claimed_run_id && run.thread_id == snapshot.thread_id)
    {
        return Err(NativeAttemptTerminationError::RunMismatch);
    }
    let cursors = &snapshot.state_commit_cursors;
    // Older recovery transports legitimately omit commit coordinates. Even a
    // syntactically valid marker in such a prefix cannot establish that it was
    // installed before execution, so retain the physical slot as Unknown.
    if !snapshot.state.is_empty() && cursors.is_empty() {
        return Ok(NativeAttemptTermination::Unknown);
    }
    if cursors.len() != snapshot.state.len()
        || cursors
            .iter()
            .try_fold(0_u64, |previous, cursor| {
                (*cursor > 0 && *cursor >= previous).then_some(*cursor)
            })
            .is_none()
    {
        return Err(NativeAttemptTerminationError::InvalidCommitCoordinates);
    }

    let mut coverage_cursor = None;
    for (command, cursor) in snapshot.state.iter().zip(cursors) {
        if command.scope != ActiveNativeAttemptCoverage::SCOPE
            || command.key.0 != ActiveNativeAttemptCoverage::KEY
            || command.run_id.as_ref() != Some(&snapshot.claimed_run_id)
        {
            continue;
        }
        if coverage_cursor.is_some() || command.merge != ActiveNativeAttemptCoverage::MERGE {
            return Err(NativeAttemptTerminationError::InvalidCoverage);
        }
        let mut one = Store::new();
        one.apply(command);
        let coverage = ActiveNativeAttemptCoverage::load(&one)
            .map_err(|_| NativeAttemptTerminationError::InvalidCoverage)?;
        if coverage != NativeAttemptCoverage::ModelOnlyV1 {
            return Err(NativeAttemptTerminationError::InvalidCoverage);
        }
        coverage_cursor = Some(*cursor);
    }
    let model_lacks_prior_coverage = coverage_cursor.is_some_and(|coverage_cursor| {
        snapshot.state.iter().zip(cursors).any(|(command, cursor)| {
            command.scope == ActiveModelOperation::SCOPE
                && command.key.0 == ActiveModelOperation::KEY
                && command.run_id.as_ref() == Some(&snapshot.claimed_run_id)
                && !coverage_precedes_model(coverage_cursor, *cursor)
        })
    });
    let models = ActiveModelOperation::recorded(&snapshot.state, &snapshot.claimed_run_id)
        .map_err(|error| NativeAttemptTerminationError::InvalidModelHistory(error.to_string()))?;
    let all_models_stopped = models
        .iter()
        .all(|operation| operation.execution.permits_reentry());

    let store = store_from_run_commands(snapshot.state.clone(), &snapshot.claimed_run_id);
    let tool_batch_absent = ActiveToolBatch::load(&store)
        .map_err(|error| NativeAttemptTerminationError::InvalidToolHistory(error.to_string()))?
        .is_none();
    Ok(termination_from_facts(
        coverage_cursor.is_some(),
        model_lacks_prior_coverage,
        all_models_stopped,
        tool_batch_absent,
    ))
}

#[cfg(kani)]
#[kani::proof]
fn native_attempt_termination_requires_every_closed_surface_fact() {
    // Exhaust the production decision for all 16 combinations. A complete
    // result is equivalent to pre-effect coverage, no earlier model entry,
    // positive evidence for every model operation, and no ToolBatch history.
    // Parsing, cursor ordering and backend durability remain executable/DB TCB.
    let coverage_installed: bool = kani::any();
    let model_lacks_prior_coverage: bool = kani::any();
    let all_models_stopped: bool = kani::any();
    let tool_batch_absent: bool = kani::any();
    let result = termination_from_facts(
        coverage_installed,
        model_lacks_prior_coverage,
        all_models_stopped,
        tool_batch_absent,
    );
    assert_eq!(
        result == NativeAttemptTermination::AllExecutionStopped,
        coverage_installed
            && !model_lacks_prior_coverage
            && all_models_stopped
            && tool_batch_absent
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::model_operation::{
        ModelExecutionEvidence, ModelOperation, ModelOperationPhase, NoDispatchProof,
        TerminalReceipt,
    };
    use crate::resolved::ModelBinding;
    use crate::tool::ToolRecoveryPolicy;
    use crate::tool_batch::ToolBatch;
    use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use std::num::NonZeroU64;

    fn snapshot(state: Vec<awaken_agent_contract::agent::state::Command>) -> RunRecoverySnapshot {
        let run = RunId("run".into());
        let thread = ThreadId("thread".into());
        let state_commit_cursors = (1..=state.len() as u64).collect();
        RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: run.clone(),
            runs: vec![RunRecord {
                id: run,
                thread_id: thread,
                state: RunState::Running,
            }],
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state,
            state_commit_cursors,
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 1,
        }
    }

    fn bind(
        mut command: awaken_agent_contract::agent::state::Command,
    ) -> awaken_agent_contract::agent::state::Command {
        command.run_id = Some(RunId("run".into()));
        command
    }

    fn model(execution: ModelExecutionEvidence) -> awaken_agent_contract::agent::state::Command {
        bind(ActiveModelOperation::write(&Some(ModelOperation {
            ordinal: NonZeroU64::MIN,
            binding: ModelBinding::new("provider", "model", "genai"),
            phase: ModelOperationPhase::Returned,
            execution,
        })))
    }

    #[test]
    fn model_only_termination_requires_coverage_before_every_terminal_model_record() {
        // Decision rules from the production table: T1 marker only => stopped;
        // T2 terminal model after marker => stopped; T3 missing marker => unknown;
        // T4 unknown model => unknown; T5 model before marker => unknown;
        // T6 tool batch after marker => unknown; T7 NotDispatched => stopped;
        // T8 legacy missing coordinates => unknown; T9 duplicate marker,
        // T10 missing exact Run and T11 unaligned coordinates => error;
        // T12 a crashed tool-bearing Run whose last model entry remains
        // Entered/Unknown stays unknown even after a local Worker exit.
        let coverage = bind(ActiveNativeAttemptCoverage::write(
            &NativeAttemptCoverage::ModelOnlyV1,
        ));
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![coverage.clone()])).unwrap(),
            NativeAttemptTermination::AllExecutionStopped,
            "T1"
        );
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                coverage.clone(),
                model(ModelExecutionEvidence::Terminated(
                    TerminalReceipt::from_wire_status(Some("completed")).unwrap()
                )),
            ]))
            .unwrap(),
            NativeAttemptTermination::AllExecutionStopped,
            "T2"
        );
        // T13 coverage and a returned model frame share one durable commit.
        // That atomic write cannot prove enrollment preceded the external
        // invocation; only a strictly earlier committed barrier permits closure.
        let mut simultaneous = snapshot(vec![
            coverage.clone(),
            model(ModelExecutionEvidence::Terminated(
                TerminalReceipt::from_wire_status(Some("completed")).unwrap(),
            )),
        ]);
        simultaneous.state_commit_cursors = vec![1, 1];
        assert_eq!(
            classify_native_attempt_termination(&simultaneous).unwrap(),
            NativeAttemptTermination::Unknown,
            "T13 same commit is not pre-effect enrollment"
        );
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![model(
                ModelExecutionEvidence::Terminated(
                    TerminalReceipt::from_wire_status(Some("completed")).unwrap()
                ),
            )]))
            .unwrap(),
            NativeAttemptTermination::Unknown,
            "T3"
        );
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                coverage.clone(),
                model(ModelExecutionEvidence::Unknown),
            ]))
            .unwrap(),
            NativeAttemptTermination::Unknown,
            "T4"
        );
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                model(ModelExecutionEvidence::Terminated(
                    TerminalReceipt::from_wire_status(Some("completed")).unwrap()
                )),
                coverage.clone(),
            ]))
            .unwrap(),
            NativeAttemptTermination::Unknown,
            "T5"
        );
        let batch = ToolBatch::for_step(
            RunId("run".into()),
            0,
            [(
                ToolCall {
                    call_id: "call".into(),
                    tool_id: "tool".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::replay_safe(),
            )],
        )
        .unwrap();
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                coverage.clone(),
                bind(ActiveToolBatch::write(&Some(batch.clone()))),
            ]))
            .unwrap(),
            NativeAttemptTermination::Unknown,
            "T6"
        );
        // T12 combines the two unresolved facts observed in a historical
        // terminal Run. Neither an earlier successful model call nor a local
        // process-exit receipt can fill in this missing remote/tool proof.
        let entered_unknown = bind(ActiveModelOperation::write(&Some(ModelOperation {
            ordinal: NonZeroU64::MIN,
            binding: ModelBinding::parse("provider", "model", "genai").unwrap(),
            phase: ModelOperationPhase::Entered,
            execution: ModelExecutionEvidence::Unknown,
        })));
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                coverage.clone(),
                bind(ActiveToolBatch::write(&Some(batch))),
                entered_unknown,
            ]))
            .unwrap(),
            NativeAttemptTermination::Unknown,
            "T12"
        );
        assert_eq!(
            classify_native_attempt_termination(&snapshot(vec![
                coverage.clone(),
                model(ModelExecutionEvidence::NotDispatched(
                    NoDispatchProof::request_projection()
                )),
            ]))
            .unwrap(),
            NativeAttemptTermination::AllExecutionStopped,
            "T7"
        );
        let mut legacy = snapshot(vec![coverage.clone()]);
        legacy.state_commit_cursors.clear();
        assert_eq!(
            classify_native_attempt_termination(&legacy).unwrap(),
            NativeAttemptTermination::Unknown,
            "T8"
        );
        assert!(
            matches!(
                classify_native_attempt_termination(&snapshot(vec![coverage.clone(), coverage,])),
                Err(NativeAttemptTerminationError::InvalidCoverage)
            ),
            "T9"
        );
        let mut missing_run = snapshot(Vec::new());
        missing_run.runs.clear();
        assert!(
            matches!(
                classify_native_attempt_termination(&missing_run),
                Err(NativeAttemptTerminationError::RunMismatch)
            ),
            "T10"
        );
        let mut invalid = snapshot(Vec::new());
        invalid.state_commit_cursors.push(1);
        assert!(
            matches!(
                classify_native_attempt_termination(&invalid),
                Err(NativeAttemptTerminationError::InvalidCommitCoordinates)
            ),
            "T11"
        );
    }
}
