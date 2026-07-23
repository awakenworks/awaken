//! Agent-backed Outcome grading over the ordinary Runtime Run boundary.

use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
use awaken_runtime_contract::{
    EndCause, ExecutableAgentSnapshot, Message, MessageId, ModelBinding, Role, RunActivation,
    RunState, RuntimeRunContext, ThreadReader, TranscriptSliceSpec,
};

use crate::outcome::{Grade, Grader, GraderError, GradingInput, parse_grade};
use crate::state::{grader_run_id, grader_thread_id};

/// Backend-neutral Judge instructions. Each Evaluation supplies its Outcome,
/// rubric, transcript range, state, and prepared evidence as request data.
pub const DEFAULT_JUDGE_INSTRUCTIONS: &str = "\
You are a strict evaluator. You are given an outcome, its rubric, and evidence. \
Judge whether the requested outcome is achieved. Reply with ONLY a JSON object \
of the form {\"result\": \"satisfied\" | \"needs_revision\" | \"failed\", \
\"explanation\": \"...\"} and nothing else. Use satisfied only when the rubric is \
fully met. Classify in this order: (1) if decisive evidence establishes an explicit permanent, \
unrecoverable, or prohibited blocker, return failed; (2) otherwise, if the requested outcome \
itself is fully achieved, return satisfied; (3) otherwise return needs_revision when another \
revision could improve it. Labels describe the requested outcome's state, not the accuracy \
of an evidence report: correctly reporting a permanent blocker is still failed, never satisfied. \
When evidence is present, cite its decisive stable token or locator in the explanation.";

/// A default tool-free Judge snapshot. A composition may replace this with any
/// pinned Native or ACP Agent snapshot; the Run restriction remains authoritative.
#[must_use]
pub fn default_judge_agent(
    model_ref: &str,
    agent_id: &str,
    instructions: &str,
) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(agent_id)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(2)
        .build()
}

/// Serialize one complete grading request. The stable instruction/schema prefix
/// stays in the Judge snapshot while Evaluation-specific data is request-local.
pub fn grading_prompt(input: &GradingInput) -> Result<String, GraderError> {
    serde_json::to_string(input)
        .map(|payload| {
            format!(
                "Evaluate this Outcome input against its rubric. Return ONLY the required JSON object.\n{payload}"
            )
        })
        .map_err(|error| GraderError::Execution(error.to_string()))
}

/// Direct Agent-backed Grader. It executes a pinned Judge snapshot on a stable,
/// semantically fresh Thread through the ordinary `RunExecutor`, then parses the
/// complete committed assistant reply. The caller supplies the same context and
/// Thread reader used by any other Run; no Host or backend type is required.
pub struct AgentGrader<'a> {
    executor: &'a dyn RunExecutor,
    reader: &'a dyn ThreadReader,
    context: RuntimeRunContext,
}

impl<'a> AgentGrader<'a> {
    #[must_use]
    pub fn new(
        executor: &'a dyn RunExecutor,
        reader: &'a dyn ThreadReader,
        context: RuntimeRunContext,
    ) -> Self {
        Self {
            executor,
            reader,
            context,
        }
    }
}

#[async_trait::async_trait]
impl Grader for AgentGrader<'_> {
    async fn grade(
        &self,
        snapshot: &ExecutableAgentSnapshot,
        input: &GradingInput,
    ) -> Result<Grade, GraderError> {
        let slice_spec = TranscriptSliceSpec {
            snapshot: input.transcript_snapshot.clone(),
            ranges: input.transcript_ranges.clone(),
        };
        if let Err(error) = slice_spec.validate() {
            return Err(GraderError::Execution(format!(
                "evaluated message range is invalid: {error}"
            )));
        }
        let legacy_range = u64::try_from(input.message_start)
            .ok()
            .zip(u64::try_from(input.message_end).ok())
            .map(|(start, end)| awaken_runtime_contract::TranscriptRange::new(start, end));
        if input.transcript_ranges.as_slice() != legacy_range.as_slice() {
            return Err(GraderError::Execution(
                "grading range evidence is inconsistent".into(),
            ));
        }
        let selected_len = input
            .transcript_ranges
            .iter()
            .try_fold(0_u64, |total, range| {
                total.checked_add(range.end.saturating_sub(range.start))
            })
            .and_then(|total| usize::try_from(total).ok());
        if selected_len != Some(input.transcript.len()) {
            return Err(GraderError::Execution(
                "materialized grading messages do not match transcript ranges".into(),
            ));
        }
        let thread_id = grader_thread_id(&input.outcome_id, input.iteration);
        let run_id = grader_run_id(&input.outcome_id, input.iteration);
        let state = self.reader.run_state(&run_id);
        let state = if state
            .as_ref()
            .is_some_and(|state| *state != RunState::Running)
        {
            state.expect("checked as present")
        } else {
            let mut activation = RunActivation::new(
                run_id,
                thread_id.clone(),
                snapshot.clone(),
                vec![Message::text(
                    MessageId(format!(
                        "outcome/{}/grader/{}/input",
                        input.outcome_id.0, input.iteration
                    )),
                    Role::User,
                    grading_prompt(input)?,
                )],
            );
            activation.tool_capability_narrowing = ToolCapabilityNarrowing::DenyAll;
            self.executor
                .execute(activation, self.context.clone())
                .await
                .map_err(|error| GraderError::Execution(error.to_string()))?
        };
        if state == RunState::Ended(EndCause::Cancelled) {
            return Err(GraderError::Interrupted);
        }
        if state != RunState::Ended(EndCause::NaturalEnd) {
            return Err(GraderError::Execution(format!(
                "Judge Run ended in {state:?}"
            )));
        }
        let reply = self
            .reader
            .committed_messages(&thread_id)
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text_content)
            .ok_or_else(|| GraderError::InvalidOutput("Judge returned no reply".into()))?;
        parse_grade(&reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{Id, Rubric};
    use awaken_runtime_contract::{
        ThreadId, TranscriptRange, TranscriptSnapshotRef, TranscriptView,
    };

    fn input() -> GradingInput {
        GradingInput {
            outcome_id: Id("grade-1".into()),
            iteration: 2,
            description: "ship".into(),
            rubric: Rubric("all checks pass".into()),
            transcript_snapshot: TranscriptSnapshotRef {
                thread_id: ThreadId("worker".into()),
                view: TranscriptView::RawCommitted,
                version: 1,
                end_seq: 1,
            },
            transcript_ranges: vec![TranscriptRange::new(0, 1)],
            transcript: vec![Message::text(
                MessageId("answer".into()),
                Role::Assistant,
                "done",
            )],
            message_start: 0,
            message_end: 1,
            worker_state: serde_json::json!({"version": 3}),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn default_judge_is_tool_free_and_domain_neutral() {
        let snapshot = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        assert_eq!(snapshot.root_agent_id.0, "judge");
        assert!(snapshot.resolved_spec.tool_descriptors.is_empty());

        let instructions = DEFAULT_JUDGE_INSTRUCTIONS.to_ascii_lowercase();
        for required in [
            "satisfied",
            "needs_revision",
            "failed",
            "rubric",
            "evidence",
            "classify in this order",
            "outcome's state",
            "permanent blocker is still failed",
        ] {
            assert!(instructions.contains(required), "missing `{required}`");
        }
        for domain_term in ["coverage", "commit", "git", "test log", "compiler"] {
            assert!(
                !instructions.contains(domain_term),
                "default Judge prompt leaked domain term `{domain_term}`"
            );
        }
    }

    #[test]
    fn grading_prompt_carries_complete_dynamic_input() {
        let prompt = grading_prompt(&input()).unwrap();
        assert!(prompt.starts_with("Evaluate this Outcome input"));
        assert!(prompt.contains("all checks pass"));
        assert!(prompt.contains("\"iteration\":2"));
        assert!(prompt.contains("\"message_end\":1"));
    }
}
