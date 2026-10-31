// @formal entrypoint protocol_adapter.a2a_task_state_projection_is_total_exact_and_non_strengthening
//! Project a committed step (and thread history) into an A2A `Task`.
//!
//! A2A is a snapshot protocol: `message:send` returns the whole `Task` — its
//! lifecycle `status` plus the conversation `history`. This module folds committed
//! neutral `Message`s into A2A `Message`s and derives the `TaskState` from the step
//! outcome (awaiting → input-required; step-budget exhausted → failed; else
//! completed).

use awaken_agent_contract::agent::message::{Message as AgentMessage, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};

use awaken_session_application_contract::{StepOutcome, blocks_text};

use crate::types::{Message, MessageRole, Part, Task, TaskState, TaskStatus};

pub(crate) fn working_task(task_id: &str, thread: &str, timestamp: String) -> Task {
    Task {
        kind: crate::types::TaskKind::Task,
        id: task_id.to_string(),
        context_id: thread.to_string(),
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: Some(timestamp),
        },
        history: Vec::new(),
        artifacts: Vec::new(),
        metadata: None,
    }
}

/// The A2A status-message metadata key that names why a task is terminal.
pub(crate) const REASON_METADATA_KEY: &str = "reason";

/// The reason code of a Run whose physical result is unknown. It is the same
/// neutral classification Managed surfaces as `ExecutionUnknown`: terminal,
/// never automatically repeated.
pub(crate) const EXECUTION_UNKNOWN_REASON: &str = "execution_unknown";

const EXECUTION_UNKNOWN_MESSAGE: &str = "The execution result is unknown. This operation will not be automatically repeated. Do not repeat external actions until their outcome is verified.";

/// Total projection from the authoritative runtime lifecycle to A2A state.
///
/// Decision table (C = committed RunState, E = A2A TaskState):
///
/// | Row | C | E |
/// |---|---|---|
/// | S1 | Running | Working |
/// | S2 | Awaiting | InputRequired |
/// | S3 | Ended(Error/MaxSteps/Stopped) | Failed |
/// | S4 | Ended(Indeterminate) | Failed + `execution_unknown` reason |
/// | S5 | Ended(Cancelled) | Canceled |
/// | S6 | Ended(NaturalEnd) | Completed |
///
/// An indeterminate end is terminal: no later commit will change it, so a
/// non-terminal `Working` would make an orchestrator wait forever. It never
/// strengthens into `Completed` or `Canceled`; `Failed` carries the explicit
/// reason so a caller cannot mistake it for an ordinary classified failure.
#[must_use]
pub(crate) fn task_state_for_run_state(state: &RunState) -> TaskState {
    match state {
        RunState::Running => TaskState::Working,
        RunState::Awaiting => TaskState::InputRequired,
        RunState::Ended(
            EndCause::Error(_)
            | EndCause::MaxSteps
            | EndCause::Stopped(_)
            | EndCause::Indeterminate,
        ) => TaskState::Failed,
        RunState::Ended(EndCause::Cancelled) => TaskState::Canceled,
        RunState::Ended(EndCause::NaturalEnd) => TaskState::Completed,
    }
}

/// Build the `Task` returned for a step on `thread`. `history` is the thread's full
/// committed transcript (post-step); `outcome` classifies the terminal state.
pub fn encode_task(
    thread: &str,
    history: &[AgentMessage],
    outcome: &StepOutcome,
    timestamp: String,
) -> Task {
    let messages: Vec<Message> = history
        .iter()
        .filter_map(|m| to_a2a_message(thread, m))
        .collect();

    let state = task_state_for_run_state(outcome.state());

    // The status message is what the agent last said. A terminal fault carries its
    // message so the task explains why it failed; when awaiting on a tool, it is the
    // prompt for the input the task now requires.
    let status_message = match outcome.state() {
        RunState::Ended(EndCause::Error(failure)) => Some(Message::agent_text(
            format!("{thread}-status"),
            failure.message(),
        )),
        RunState::Ended(EndCause::Indeterminate) => Some(execution_unknown_status(thread)),
        RunState::Awaiting => outcome
            .pending()
            .map(|pending| {
                Message::agent_text(
                    format!("{thread}-status"),
                    format!("input required for tool `{}`", pending.name),
                )
            })
            .or_else(|| {
                messages
                    .iter()
                    .rev()
                    .find(|message| message.role == MessageRole::Agent)
                    .cloned()
            }),
        _ => messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Agent)
            .cloned(),
    };

    Task {
        kind: crate::types::TaskKind::Task,
        id: format!("task-{thread}"),
        context_id: thread.to_string(),
        status: TaskStatus {
            state,
            message: status_message,
            timestamp: Some(timestamp),
        },
        history: messages,
        artifacts: Vec::new(),
        metadata: None,
    }
}

/// The status message of an indeterminate end: the fixed explanation plus the
/// machine-readable `execution_unknown` reason in the message metadata.
fn execution_unknown_status(thread: &str) -> Message {
    let mut message = Message::agent_text(format!("{thread}-status"), EXECUTION_UNKNOWN_MESSAGE);
    message.metadata = Some(std::collections::BTreeMap::from([(
        REASON_METADATA_KEY.to_string(),
        serde_json::Value::String(EXECUTION_UNKNOWN_REASON.to_string()),
    )]));
    message
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use awaken_agent_contract::agent::run::Failure;

    /// @obligation protocol_adapter.a2a_task_state_projection_is_total_exact_and_non_strengthening
    #[kani::proof]
    fn a2a_task_state_projection_is_total_exact_and_non_strengthening() {
        let tag: u8 = kani::any();
        kani::assume(tag <= 7);
        let state = match tag {
            0 => RunState::Running,
            1 => RunState::Awaiting,
            2 => RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
            3 => RunState::Ended(EndCause::MaxSteps),
            4 => RunState::Ended(EndCause::Stopped(String::new())),
            5 => RunState::Ended(EndCause::Cancelled),
            6 => RunState::Ended(EndCause::Indeterminate),
            _ => RunState::Ended(EndCause::NaturalEnd),
        };

        let projected = task_state_for_run_state(&state);
        match projected {
            TaskState::Working => assert_eq!(state, RunState::Running),
            TaskState::InputRequired => assert_eq!(state, RunState::Awaiting),
            TaskState::Failed => assert!(matches!(
                state,
                RunState::Ended(
                    EndCause::Error(_)
                        | EndCause::MaxSteps
                        | EndCause::Stopped(_)
                        | EndCause::Indeterminate
                )
            )),
            TaskState::Canceled => {
                assert_eq!(state, RunState::Ended(EndCause::Cancelled))
            }
            TaskState::Completed => {
                assert_eq!(state, RunState::Ended(EndCause::NaturalEnd))
            }
            TaskState::Submitted
            | TaskState::AuthRequired
            | TaskState::Rejected
            | TaskState::Unknown => unreachable!("runtime projection cannot invent this state"),
        }
    }
}

/// Convert a neutral message to an A2A message. User/assistant map to the A2A
/// roles; system and tool messages are internal and dropped from the A2A history.
fn to_a2a_message(thread: &str, message: &AgentMessage) -> Option<Message> {
    let role = match message.role {
        Role::User => MessageRole::User,
        Role::Assistant => MessageRole::Agent,
        Role::System | Role::Tool => return None,
    };
    let text = blocks_text(&message.content);
    if text.trim().is_empty() {
        return None;
    }
    Some(Message {
        kind: crate::types::MessageKind::Message,
        task_id: Some(format!("task-{thread}")),
        context_id: Some(thread.to_string()),
        message_id: message.id.0.clone(),
        role,
        parts: vec![Part::text(text)],
        extensions: Vec::new(),
        metadata: None,
        reference_task_ids: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::run::{EndCause, Failure};
    use awaken_session_application_contract::Pending;

    const TIMESTAMP: &str = "2026-07-19T00:00:00Z";

    fn msg(id: &str, role: Role, text: &str) -> AgentMessage {
        AgentMessage::text(MessageId(id.into()), role, text)
    }

    #[test]
    fn terminal_failure_yields_a_failed_task_with_the_fault_message() {
        // Causes: the fixtures below establish `terminal failure` with the concrete inputs, state,
        // dependencies, and failure triggers used by this case.
        // Effects: the observable result `yields a failed task with the fault message` and every
        // asserted state transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect rule F1: an inference failure in authoritative RunState
        // produces a failed A2A task carrying the same neutral fault message.
        let outcome = StepOutcome::ended(
            Vec::new(),
            EndCause::Error(Failure::Inference {
                code: "inference_failed".into(),
                message: "upstream is down".into(),
            }),
        );
        let task = encode_task("th", &[], &outcome, TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::Failed);
        let text = task
            .status
            .message
            .as_ref()
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(crate::types::Part::text_value)
                    .collect::<String>()
            })
            .unwrap_or_default();
        assert!(
            text.contains("upstream is down"),
            "status carries the fault: {text}"
        );
    }

    fn outcome(awaiting: bool, pending: Option<Pending>) -> StepOutcome {
        if awaiting {
            StepOutcome::awaiting(Vec::new(), pending)
        } else {
            StepOutcome::ended(Vec::new(), EndCause::NaturalEnd)
        }
    }

    #[test]
    fn completed_task_carries_history_and_last_agent_status() {
        // Cause: the router supplied one exact observation timestamp with the
        // committed outcome. Effect: the pure encoder preserves those bytes in
        // the public Task, without sampling another clock (rule T1).
        let history = [
            msg("u1", Role::User, "hi"),
            msg("a1", Role::Assistant, "hello there"),
            msg("t1", Role::Tool, "tool noise"),
        ];
        let task = encode_task("t", &history, &outcome(false, None), TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(task.context_id, "t");
        assert_eq!(task.status.timestamp.as_deref(), Some(TIMESTAMP), "T1");
        // Tool/system messages are not part of the A2A history.
        assert_eq!(task.history.len(), 2);
        assert_eq!(task.status.message.unwrap().text(), "hello there");
    }

    #[test]
    fn awaiting_task_is_input_required_with_a_prompt() {
        let history = [msg("u1", Role::User, "answer please")];
        let pending = Pending {
            tool_use_id: "c1".into(),
            name: "submit_answer".into(),
            input: serde_json::Value::Null,
            client_executed: true,
        };
        let task = encode_task(
            "t",
            &history,
            &outcome(true, Some(pending)),
            TIMESTAMP.into(),
        );
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert!(
            task.status
                .message
                .unwrap()
                .text()
                .contains("submit_answer")
        );
    }

    #[test]
    fn exhausted_step_budget_projects_a_failed_task() {
        // Causes: the fixtures below establish `exhausted step budget` with the concrete inputs,
        // state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `projects a failed task` and every asserted state
        // transition or side effect must hold.
        // Constraints/invariants: this adapter owns only its public wire mapping; the shared
        // neutral Runtime and committed facts remain the single execution authority.
        // Coverage rationale: `exhausted step budget` is one independent branch selecting `projects
        // a failed task`; a multi-row decision table is not applicable, and sibling tests own
        // alternate causes.
        // A run that hit its step ceiling (not awaiting) is a `failed` A2A task, and
        // still carries the transcript + last agent status.
        let history = [
            msg("u1", Role::User, "do a lot"),
            msg("a1", Role::Assistant, "partial progress"),
        ];
        let exhausted = StepOutcome::ended(Vec::new(), EndCause::MaxSteps);
        let task = encode_task("t", &history, &exhausted, TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::Failed);
        assert_eq!(task.status.message.unwrap().text(), "partial progress");
    }

    #[test]
    fn run_state_projection_covers_every_decision_row() {
        // Rows S1-S6 of `task_state_for_run_state`. Expected states are
        // independent constants; S4 (Indeterminate) is terminal, not Working.
        let rows = [
            ("S1", RunState::Running, TaskState::Working),
            ("S2", RunState::Awaiting, TaskState::InputRequired),
            (
                "S3",
                RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
                TaskState::Failed,
            ),
            ("S3", RunState::Ended(EndCause::MaxSteps), TaskState::Failed),
            (
                "S3",
                RunState::Ended(EndCause::Stopped("stop".into())),
                TaskState::Failed,
            ),
            (
                "S4",
                RunState::Ended(EndCause::Indeterminate),
                TaskState::Failed,
            ),
            (
                "S5",
                RunState::Ended(EndCause::Cancelled),
                TaskState::Canceled,
            ),
            (
                "S6",
                RunState::Ended(EndCause::NaturalEnd),
                TaskState::Completed,
            ),
        ];
        for (row, state, expected) in rows {
            assert_eq!(task_state_for_run_state(&state), expected, "{row}");
        }
    }

    #[test]
    fn indeterminate_end_is_a_terminal_failed_task_with_the_execution_unknown_reason() {
        // Row S4. Cause: the committed Run ended Indeterminate (its physical
        // result is unknown). Effects: the Task is terminal Failed (an
        // orchestrator stops waiting), its status message explains that the
        // operation will not be repeated, and the message metadata carries the
        // `execution_unknown` reason code Managed also uses. The last agent
        // text never masks the reason.
        let history = [
            msg("u1", Role::User, "do it"),
            msg("a1", Role::Assistant, "partial progress"),
        ];
        let outcome = StepOutcome::ended(Vec::new(), EndCause::Indeterminate);
        let task = encode_task("t", &history, &outcome, TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::Failed);
        assert!(task.status.state.is_terminal());
        let message = task.status.message.expect("indeterminate status message");
        assert!(
            message
                .text()
                .contains("will not be automatically repeated")
        );
        assert_eq!(
            message
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason")),
            Some(&serde_json::Value::String("execution_unknown".into()))
        );
        // A classified failure carries no execution_unknown reason (row S3).
        let failed = StepOutcome::ended(Vec::new(), EndCause::MaxSteps);
        let failed = encode_task("t", &history, &failed, TIMESTAMP.into());
        assert!(
            failed
                .status
                .message
                .and_then(|message| message.metadata)
                .is_none()
        );
    }

    #[test]
    fn completed_task_with_no_agent_message_has_no_status_message() {
        // History holding only a user turn (agent produced nothing textual) → a
        // completed task whose status message is absent, not a user message.
        let history = [msg("u1", Role::User, "hi")];
        let task = encode_task("t", &history, &outcome(false, None), TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::Completed);
        assert!(task.status.message.is_none());
    }

    #[test]
    fn history_projection_keeps_public_text_and_drops_private_thinking() {
        /* A2A snapshot decision table. Causes: C1 an assistant row contains
         * private Thinking plus public Text; C2 a row contains only Thinking.
         * Effects: E1 C1 produces one Agent message containing only public
         * text; E2 C2 produces no history/status message; E3 no thinking bytes
         * reach the snapshot. Rules: R1 C1=>E1+E3; R2 C2=>E2+E3. A2A remains
         * snapshot-only and reuses the canonical `blocks_text` projection. */
        let history = [
            AgentMessage {
                id: MessageId("a1".into()),
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "private-reasoning".into(),
                        signature: None,
                    },
                    ContentBlock::image_url("https://x/y.png"),
                    ContentBlock::text("described"),
                ],
            },
            AgentMessage {
                id: MessageId("a2".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::Thinking {
                    text: "private-only".into(),
                    signature: None,
                }],
            },
        ];
        let task = encode_task("t", &history, &outcome(false, None), TIMESTAMP.into());
        // Image and Thinking are dropped; only public text survives.
        assert_eq!(task.status.message.as_ref().unwrap().text(), "described");
        assert_eq!(task.history.len(), 1);
        assert_eq!(task.history[0].parts.len(), 1);
        assert!(!format!("{task:?}").contains("private"));
    }

    #[test]
    fn input_required_without_a_pending_falls_back_to_the_last_agent_message() {
        // Cause/effect rule A2: Awaiting + no Pending + prior agent message ->
        // InputRequired + that message. A1 (Pending present) owns the generated
        // tool prompt; A3 (neither source) yields no status message.
        let history = [
            msg("u1", Role::User, "question"),
            msg("a1", Role::Assistant, "still thinking"),
        ];
        let task = encode_task("t", &history, &outcome(true, None), TIMESTAMP.into());
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert_eq!(task.status.message.unwrap().text(), "still thinking");
    }

    #[test]
    fn history_maps_user_and_assistant_to_a2a_roles() {
        let history = [
            msg("u1", Role::User, "hi"),
            msg("a1", Role::Assistant, "hello"),
        ];
        let task = encode_task("t", &history, &outcome(false, None), TIMESTAMP.into());
        assert_eq!(task.history[0].role, MessageRole::User);
        assert_eq!(task.history[1].role, MessageRole::Agent);
    }
}
