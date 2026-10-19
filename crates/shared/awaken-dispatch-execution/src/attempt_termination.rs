// @formal entrypoint model_operation.coverage_commit_strictly_precedes_model_entry
// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! One recovery projection over canonical ThreadCommit; never a receipt store.
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_runtime_contract::attempt_termination::{
    ActiveNativeAttemptCoverage, NativeAttemptCoverage,
};
use awaken_runtime_contract::model_operation::ActiveModelOperation;
use awaken_runtime_contract::tool_batch::ActiveToolBatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAttemptTermination {
    Unknown,
    AllExecutionStopped,
    /// Governed effects are closed; a data-only provider may still compute or
    /// bill, and its discarded result remains Unknown. This is not termination.
    ControlledExecutionClosed,
}

const fn coverage_precedes_model(coverage_commit: u64, model_commit: u64) -> bool {
    coverage_commit > 0 && coverage_commit < model_commit
}

#[cfg(kani)]
/// @obligation model_operation.coverage_commit_strictly_precedes_model_entry
/// @kani default-features=false
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
    NativeAttemptFootprint::decode(snapshot).map(|footprint| footprint.classify(&[]))
}

/// A transient view of the canonical commit prefix, not a second receipt store.
/// Consumers validate once, observe its exact process bindings, then classify
/// that same frozen view. Local Worker exit is an independent prerequisite.
pub struct NativeAttemptFootprint {
    profile: NativeAttemptCoverage,
    all_models_stopped: bool,
    all_model_effects_closed: bool,
    models_response_only: bool,
    response_only: bool,
    domains: Vec<awaken_runtime_contract::tool_batch::ToolExecutionDomain>,
}

impl NativeAttemptFootprint {
    /// A local process supervisor may only close the model-only profile.
    /// Journalled tools can ask a host service to create work outside the
    /// Worker's cgroup, so their complete scope needs separate provider proof.
    pub fn model_only_coverage(&self) -> bool {
        self.profile == NativeAttemptCoverage::ModelOnlyV1
    }

    pub fn process_domains(&self) -> &[awaken_runtime_contract::tool_batch::ToolExecutionDomain] {
        &self.domains
    }

    pub(crate) fn response_only(&self) -> bool {
        self.response_only
    }

    /// Read model effects without treating a logical ToolBatch clear as either
    /// physical closure or the removal of earlier business operations.
    pub(crate) fn model_effect_scope_for_run(
        snapshot: &RunRecoverySnapshot,
        run: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_runtime_contract::model_operation::ModelEffectScope>,
        NativeAttemptTerminationError,
    > {
        use awaken_runtime_contract::model_operation::ModelEffectScope;
        let footprint = Self::decode_model_footprint_for_run(snapshot, run)?;
        Ok(
            (footprint.profile != NativeAttemptCoverage::Unspecified).then_some(
                if footprint.models_response_only {
                    ModelEffectScope::ResponseOnly
                } else {
                    ModelEffectScope::Opaque
                },
            ),
        )
    }

    pub fn classify(
        &self,
        closed: &[awaken_runtime_contract::tool_batch::ToolExecutionDomain],
    ) -> NativeAttemptTermination {
        // This classifies execution containment only. An origin label does not
        // prove membership, resource exclusivity, closure or target access.
        let tools_closed = self.domains.iter().all(|domain| {
            domain.is_worker_contained()
                || (domain.controlled_process_binding().is_some()
                    && closed
                        .iter()
                        .any(|observed| observed.same_containment_as(domain)))
        });
        let covered = self.profile != NativeAttemptCoverage::Unspecified;
        if termination_from_facts(covered, false, self.all_models_stopped, tools_closed)
            == NativeAttemptTermination::AllExecutionStopped
        {
            NativeAttemptTermination::AllExecutionStopped
        } else if termination_from_facts(
            covered,
            false,
            self.all_model_effects_closed,
            tools_closed,
        ) == NativeAttemptTermination::AllExecutionStopped
        {
            NativeAttemptTermination::ControlledExecutionClosed
        } else {
            NativeAttemptTermination::Unknown
        }
    }

    pub fn decode(snapshot: &RunRecoverySnapshot) -> Result<Self, NativeAttemptTerminationError> {
        Self::decode_for_run(snapshot, &snapshot.claimed_run_id)
    }

    pub(crate) fn decode_for_run(
        snapshot: &RunRecoverySnapshot,
        run: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Self, NativeAttemptTerminationError> {
        let mut footprint = Self::decode_model_footprint_for_run(snapshot, run)?;
        if footprint.profile != NativeAttemptCoverage::Unspecified {
            footprint.domains = ActiveToolBatch::recorded_execution_domains(&snapshot.state, run)
                .map_err(|error| {
                NativeAttemptTerminationError::InvalidToolHistory(error.to_string())
            })?;
        }
        Ok(footprint)
    }

    fn decode_model_footprint_for_run(
        snapshot: &RunRecoverySnapshot,
        run: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Self, NativeAttemptTerminationError> {
        let unsupported = || Self {
            profile: NativeAttemptCoverage::Unspecified,
            all_models_stopped: false,
            all_model_effects_closed: false,
            models_response_only: false,
            response_only: false,
            domains: Vec::new(),
        };
        if snapshot
            .run_record(run)
            .map_err(|_| NativeAttemptTerminationError::RunMismatch)?
            .is_none()
        {
            return Err(NativeAttemptTerminationError::RunMismatch);
        }
        let cursors = &snapshot.state_commit_cursors;
        if !snapshot.state.is_empty() && cursors.is_empty() {
            return Ok(unsupported());
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
        let mut coverage = None;
        for (command, cursor) in snapshot.state.iter().zip(cursors) {
            if command.scope != ActiveNativeAttemptCoverage::SCOPE
                || command.key.0 != ActiveNativeAttemptCoverage::KEY
                || command.run_id.as_ref() != Some(run)
            {
                continue;
            }
            if coverage.is_some() || command.merge != ActiveNativeAttemptCoverage::MERGE {
                return Err(NativeAttemptTerminationError::InvalidCoverage);
            }
            let mut one = Store::new();
            one.apply(command);
            let profile = ActiveNativeAttemptCoverage::load(&one)
                .map_err(|_| NativeAttemptTerminationError::InvalidCoverage)?;
            if profile == NativeAttemptCoverage::Unspecified {
                return Err(NativeAttemptTerminationError::InvalidCoverage);
            }
            coverage = Some((profile, *cursor));
        }
        let Some((profile, coverage_cursor)) = coverage else {
            return Ok(unsupported());
        };
        let has_tool_history = snapshot.state.iter().any(|command| {
            command.scope == ActiveToolBatch::SCOPE
                && command.key.0 == ActiveToolBatch::KEY
                && command.run_id.as_ref() == Some(run)
        });
        let coverage_precedes_every_entry =
            snapshot.state.iter().zip(cursors).all(|(command, cursor)| {
                if command.scope == ActiveModelOperation::SCOPE
                    && [ActiveModelOperation::KEY, ActiveToolBatch::KEY]
                        .contains(&command.key.0.as_str())
                    && command.run_id.as_ref() == Some(run)
                {
                    coverage_precedes_model(coverage_cursor, *cursor)
                } else {
                    true
                }
            });
        if !coverage_precedes_every_entry
            || (profile == NativeAttemptCoverage::ModelOnlyV1 && has_tool_history)
        {
            return Ok(unsupported());
        }
        let models = ActiveModelOperation::recorded(&snapshot.state, run).map_err(|error| {
            NativeAttemptTerminationError::InvalidModelHistory(error.to_string())
        })?;
        let models_response_only = models.iter().all(|model| {
            model.effect_scope
                == awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly
        });
        Ok(Self {
            profile,
            all_models_stopped: models.iter().all(|model| model.execution.permits_reentry()),
            all_model_effects_closed: models.iter().all(|model| {
                model.execution.permits_reentry()
                    || model.effect_scope
                        == awaken_runtime_contract::model_operation::ModelEffectScope::ResponseOnly
            }),
            models_response_only,
            response_only: !has_tool_history && models_response_only,
            domains: Vec::new(),
        })
    }
}

#[cfg(kani)]
/// @obligation sandbox.k8s_process_observation_requires_complete_exit
/// @kani default-features=false
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
    use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::model_operation::{
        ModelExecutionEvidence, ModelOperation, ModelOperationPhase, NoDispatchProof,
        TerminalReceipt,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::tool::ToolRecoveryPolicy;
    use awaken_runtime_contract::tool_batch::ToolBatch;
    use std::num::NonZeroU64;

    #[test]
    fn journalled_closure_requires_every_exact_domain_and_preserves_unknown_results() {
        use awaken_runtime_contract::model_operation::ModelEffectScope;
        use awaken_runtime_contract::tool_batch::ToolExecutionDomain;
        // C1 pre-effect coverage; C2 model is response-only/opaque; C3 exact
        // process closure delivered/missing/wrong; C4 Requested prefix before
        // executor entry. R1=C1+C4+response-only+exact
        // => controlled closure, original Unknown unchanged, no model replay.
        // R2=!C1|opaque|missing|wrong => Unknown; R3=!C4 => Unknown.
        // Worker-local tool containment
        // adds no remote participant; a contained process does, even after its
        // ordinary output was committed. All rules consume real canonical cells.
        let domain = ToolExecutionDomain::ControlledProcess {
            provider_ref: "test-process".into(),
            execution_ref: "old-incarnation".into(),
            resource_origin: None,
        };
        let mut batch = ToolBatch::for_step(
            RunId("run".into()),
            1,
            [(
                ToolCall {
                    call_id: "tool".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::default(),
            )],
        )
        .unwrap();
        let requested = bind(ActiveToolBatch::write(&Some(batch.clone())));
        batch
            .mark_executing_in_domain("tool", Some(domain.clone()))
            .unwrap();
        for covered in [false, true] {
            for scope in [ModelEffectScope::Opaque, ModelEffectScope::ResponseOnly] {
                let mut operation = ModelOperation::next(
                    None,
                    ModelBinding::parse("provider", "model", "genai").unwrap(),
                )
                .unwrap();
                operation.effect_scope = scope;
                let mut commands = vec![
                    bind(ActiveModelOperation::write(&Some(operation.clone()))),
                    requested.clone(),
                    bind(ActiveToolBatch::write(&Some(batch.clone()))),
                ];
                if covered {
                    commands.insert(
                        0,
                        bind(ActiveNativeAttemptCoverage::write(
                            &NativeAttemptCoverage::JournalledV1,
                        )),
                    );
                }
                let complete = snapshot(commands);
                let view = NativeAttemptFootprint::decode(&complete).unwrap();
                assert!(
                    !view.model_only_coverage(),
                    "J1 journalled coverage is not a local process-only proof"
                );
                let wrong = ToolExecutionDomain::ControlledProcess {
                    provider_ref: "test-process".into(),
                    execution_ref: "replacement-incarnation".into(),
                    resource_origin: None,
                };
                assert_eq!(view.classify(&[]), NativeAttemptTermination::Unknown);
                assert_eq!(view.classify(&[wrong]), NativeAttemptTermination::Unknown);
                assert_eq!(
                    view.classify(std::slice::from_ref(&domain)),
                    if covered && scope == ModelEffectScope::ResponseOnly {
                        NativeAttemptTermination::ControlledExecutionClosed
                    } else {
                        NativeAttemptTermination::Unknown
                    }
                );
                assert_eq!(operation.execution, ModelExecutionEvidence::Unknown);
                assert!(!ModelOperation::permits_reentry(Some(&operation), false));
                // R3: the same exact process receipt cannot close an isolated
                // tail. The real SDK commits Requested before entry; losing
                // that prefix retains Unknown even with later complete data.
                let mut tail = complete;
                let position = tail.state.iter().position(|row| row == &requested).unwrap();
                tail.state.remove(position);
                tail.state_commit_cursors.remove(position);
                let tail = NativeAttemptFootprint::decode(&tail).unwrap();
                assert_eq!(
                    tail.classify(std::slice::from_ref(&domain)),
                    NativeAttemptTermination::Unknown,
                    "R3 missing entry prefix is never backfilled"
                );
            }
        }
    }

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
            effect_scope: Default::default(),
            ordinal: NonZeroU64::MIN,
            binding: ModelBinding::parse("provider", "model", "genai").unwrap(),
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
        // Entered/Unknown stays unknown even after a local Worker exit. T14
        // only the model-only marker may be consumed by a local cgroup stop;
        // journalled tools require their own complete provider evidence.
        let coverage = bind(ActiveNativeAttemptCoverage::write(
            &NativeAttemptCoverage::ModelOnlyV1,
        ));
        assert!(
            NativeAttemptFootprint::decode(&snapshot(vec![coverage.clone()]))
                .unwrap()
                .model_only_coverage(),
            "T14"
        );
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
            effect_scope: Default::default(),
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
        // T10 shares the SDK selection law: blank id/Thread, foreign owner,
        // and equal/conflicting duplicates all map to the existing RunMismatch,
        // before any coverage/process classification; no first row or repair.
        for defect in 0..6 {
            let mut malformed = snapshot(Vec::new());
            match defect {
                0 => malformed.claimed_run_id = RunId(" ".into()),
                1 => malformed.thread_id = ThreadId(" ".into()),
                2 => malformed.runs[0].thread_id = ThreadId(" ".into()),
                3 => malformed.runs[0].thread_id = ThreadId("foreign".into()),
                _ => {
                    let mut duplicate = malformed.runs[0].clone();
                    if defect == 5 {
                        duplicate.state = RunState::Awaiting;
                    }
                    malformed.runs.push(duplicate);
                }
            }
            assert!(
                matches!(
                    classify_native_attempt_termination(&malformed),
                    Err(NativeAttemptTerminationError::RunMismatch)
                ),
                "T10 defect={defect}"
            );
        }
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
