// @formal entrypoint session_execution.settled_step_has_no_parallel_observation_authority
// @formal entrypoint session_outcome.awaiting_constructor_is_nonterminal_and_nonfailed
// @formal entrypoint session_outcome.ended_constructor_owns_failure_and_clears_pending_tool
// @formal evidence session.agent_message_requires_exact_committed_tool_source_and_payload
//! Neutral value types and the [`SessionRuns`] interface the adapter drives:
//! pending tools, Run outcomes, capabilities, Session initialization, and Run errors.

mod executor;
pub use executor::{SessionExecutionRoleSelection, SessionExecutionRoles};

mod neutral;
mod placement;
pub use placement::{
    ColocatedRealizationRoles, NO_COLOCATED_WORKER, SessionInputAdmission, SessionPlacementRoles,
};
mod projection_evidence;
mod projection_source;
mod roles;

use awaken_session_contract::SessionUsage;
pub use neutral::NeutralRuntime;
pub use projection_evidence::{
    CommittedEvidenceError, CommittedSessionFacts, ProjectionThreadSnapshots,
    SessionProjectionPrefix, SessionProjectionReadTimings, committed_lifecycle_prefix,
    outcome_terminal_commit_cursor, projection_validation_needs_trailing_root,
    same_thread_projection_prefix,
};
pub use projection_source::{
    CommittedSessionProjection, SessionProjectionSource, select_recovery_snapshot_run,
};
pub use roles::*;
use std::sync::Arc;

#[cfg(test)]
use crate::RunErrorKind;
use crate::{
    AgentCapabilities, DelegatedRun, DelegatedRunSnapshot, Pending, RunError,
    SessionBudgetResumeDelivery, SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
    StepOutcome,
};
use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
#[cfg(test)]
use awaken_agent_contract::agent::run::{EndCause, Failure};
use awaken_session_application_contract::{LiveInboxError, LiveInboxSnapshot};

use awaken_agent_contract::agent::awaiting::PermissionDecision;

/// One evaluation round of a goal: the agent's revision messages committed this
/// round (empty when grading the existing deliverable), and the verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub description: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
    /// What grading this round cost, observed on the Judge Run's own Thread.
    /// `None` is a distinct fact from zero: the round was never graded, or the
    /// backend recorded no token usage for the grading Run.
    pub usage: Option<awaken_agent_contract::audit::model_request::TokenUsage>,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// Neutral infrastructure failure committed by the Outcome aggregate. The
/// optional source Run correlates an ordinary lifecycle fact so adapters can
/// project one public error without parsing identities or duplicating it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeFailure {
    pub code: String,
    pub message: String,
    pub source_run_id: Option<RunId>,
}

/// One terminal Outcome rebuilt from committed Thread truth. A rubric `failed`
/// remains a completed report; only execution/schema/persistence faults use the
/// infrastructure-error branch.
#[derive(Debug, Clone, PartialEq)]
pub enum CommittedOutcomeProjection {
    Completed(OutcomeReport),
    Errored(OutcomeFailure),
}

/// The next durable boundary reached while driving an Outcome. Awaiting keeps
/// the active aggregate in Thread state; completed carries its terminal report.
#[derive(Debug, Clone, PartialEq)]
pub enum OutcomeDrive {
    Awaiting,
    Completed(OutcomeReport),
}

/// Public exact-generation realization port owned by the Session application
/// boundary. Local Host relay/connection code and downstream platform gateways
/// implement this same contract; neither becomes Session desired-state authority.
#[async_trait]
pub trait McpAttachmentRealizer: Send + Sync {
    /// Stage one exact generation without making it tool-visible.
    async fn stage_mcp_attachment(
        &self,
        request: crate::StageMcpAttachment,
    ) -> Result<crate::McpRealizationReceipt, RunError>;

    /// Publish one durably active exact generation at a safe boundary.
    async fn publish_mcp_generation(
        &self,
        generation: crate::McpGenerationRef,
    ) -> Result<(), RunError>;

    /// Hide and dispose one exact generation idempotently.
    async fn drain_mcp_generation(
        &self,
        generation: crate::McpGenerationRef,
    ) -> Result<(), RunError>;
}

pub async fn publish_mcp_generation_receipt(
    mcp: &dyn McpAttachmentRealizer,
    generation: crate::McpGenerationRef,
) -> Result<crate::McpProjectionReceipt, RunError> {
    mcp.publish_mcp_generation(generation.clone()).await?;
    Ok(crate::McpProjectionReceipt::new(
        generation,
        crate::McpProjectionEffectKind::Publish,
    ))
}

pub async fn drain_mcp_generation_receipt(
    mcp: &dyn McpAttachmentRealizer,
    generation: crate::McpGenerationRef,
) -> Result<crate::McpProjectionReceipt, RunError> {
    mcp.drain_mcp_generation(generation.clone()).await?;
    Ok(crate::McpProjectionReceipt::new(
        generation,
        crate::McpProjectionEffectKind::Drain,
    ))
}

/// One best-effort subscription to a logical Session Thread's neutral live
/// observations. The transport/runtime adapter owns buffering and fan-out; the
/// protocol sees neither Tokio channels nor Host types.
#[async_trait]
pub trait SessionThreadLiveSubscription: Send {
    /// Wait for the next live observation. `None` means the producer has closed;
    /// lag is implementation-defined and never affects committed truth.
    async fn recv(
        &mut self,
    ) -> Result<Option<awaken_agent_contract::stream::event::Observation>, RunError>;
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime. MCP realization is kept on
/// [`McpAttachmentRealizer`] because it has a distinct hot-attachment lifecycle.
#[async_trait]
/// Naming rule: this consumer role has one contract owner. N1 the canonical
/// spelling can name the role without constructing or executing anything;
/// N2 the retired spelling must fail to import, so no compatibility alias
/// or second role survives. Runtime cause/effect tests remain unchanged.
///
/// ```no_run
/// use awaken_session_execution_contract::SessionRuns;
/// fn selected_role(_: &dyn SessionRuns) {}
/// ```
///
/// ```compile_fail
/// use awaken_session_execution_contract::SessionRuntime;
/// ```
pub trait SessionRuns: Send + Sync {
    /// Run one user request on `thread` to its first pause or end. `content` is the
    /// user message's full block list (multimodal): text interleaved with any
    /// image blocks, never flattened to a bare string.
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError>;

    /// Answer a built-in tool the run awaits (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: PermissionDecision,
    ) -> Result<StepOutcome, RunError>;

    /// Deliver a client-executed tool's result to the awaiting run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError>;

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;
}

/// Complete runtime required by the canonical Session application. Partial
/// runtimes remain valid for lower-level adapters and focused tests, but they
/// cannot be installed as the application executor merely because their base
/// methods compile. The marker carries no behavior or parallel state.
pub trait CompleteSessionRuntime:
    SessionRuns
    + RealizationRuntime
    + RealizationEffects
    + ChildRunCoordination
    + SessionCoordinationReads
    + BudgetAccounting
    + StreamingRuns
    + EnvironmentRuntime
    + ThreadReads
    + SessionProjectionSource
    + TerminalCleanup
    + Outcomes
    + LiveInboxAccess
    + CapabilityRuntime
{
}

impl<T> CompleteSessionRuntime for T where
    T: SessionRuns
        + RealizationRuntime
        + RealizationEffects
        + ChildRunCoordination
        + SessionCoordinationReads
        + BudgetAccounting
        + StreamingRuns
        + EnvironmentRuntime
        + ThreadReads
        + SessionProjectionSource
        + TerminalCleanup
        + Outcomes
        + LiveInboxAccess
        + CapabilityRuntime
        + ?Sized
{
}
/// Immediate durability boundary for a newly materialized Session environment.
/// The binding must commit before the runtime exposes that environment to work.
#[async_trait]
pub trait SessionEnvironmentBindingSink: Send + Sync {
    /// Atomically determine whether the durable aggregate owns this effect and
    /// whether the exact effect already committed. Internal runtime threads
    /// deliberately return `Unowned`; callers must not split this check into a
    /// separate existence read and a later authorization decision.
    async fn authorize(
        &self,
        intent: &crate::SessionEnvironmentEffectIntent,
    ) -> Result<crate::SessionEnvironmentEffectAuthorization, RunError>;

    /// Commit one authorized receipt and return the exact Store-read
    /// Environment authority. Runtime owner installation must consume this
    /// value rather than reconstructing generation state from local time.
    async fn persist(
        &self,
        receipt: crate::SessionEnvironmentReceipt,
    ) -> Result<crate::SessionEnvironmentState, RunError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

    #[tokio::test]
    async fn read_only_coordination_needs_no_execution_roles() {
        struct ReadOnly;
        crate::neutral_role!(SessionCoordinationReads for ReadOnly);
        let reads: &dyn SessionCoordinationReads = &ReadOnly;
        assert!(reads.delegated_runs("session").await.unwrap().is_empty());
        assert!(
            reads
                .coordinated_threads(&crate::SessionId::from("session"), None)
                .await
                .unwrap()
                .is_empty()
        );
        let error = reads
            .session_run_state(&crate::SessionId::from("session"), &RunId("run".into()))
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunErrorKind::Unavailable);
    }

    /// A minimal double that overrides ONLY [`SessionRuns::run`] (plus the trait's
    /// other *required* methods, implemented minimally). Every fail-closed DEFAULT
    /// method (`live_inbox_*`, `owns_thread`, `committed_messages`, `run_streaming`,
    /// …) is left at the trait's default so the tests below exercise those defaults.
    struct MinimalRuntime;

    /// The distinctive outcome `run` returns — used to prove `run_streaming` delegates
    /// to `run` identically (the default just ignores the sink).
    fn sample_outcome() -> StepOutcome {
        StepOutcome::ended(
            vec![
                Message::text(MessageId("m1".into()), Role::Assistant, "one"),
                Message::text(MessageId("m2".into()), Role::Assistant, "two"),
            ],
            EndCause::Error(Failure::Inference {
                code: "boom".into(),
                message: "it failed".into(),
            }),
        )
    }

    #[async_trait]
    impl SessionRuns for MinimalRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            Ok(sample_outcome())
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: PermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    crate::neutral_role!(RealizationRuntime for MinimalRuntime);

    crate::neutral_role!(RealizationEffects for MinimalRuntime);

    crate::neutral_role!(ChildRunCoordination for MinimalRuntime);

    crate::neutral_role!(SessionCoordinationReads for MinimalRuntime);

    crate::neutral_role!(BudgetAccounting for MinimalRuntime);

    crate::neutral_role!(StreamingRuns for MinimalRuntime);

    crate::neutral_role!(EnvironmentRuntime for MinimalRuntime);

    crate::neutral_role!(ThreadReads for MinimalRuntime);

    crate::neutral_role!(TerminalCleanup for MinimalRuntime);

    crate::neutral_role!(LiveInboxAccess for MinimalRuntime);

    crate::neutral_role!(CapabilityRuntime for MinimalRuntime);

    #[async_trait::async_trait]
    impl crate::Outcomes for MinimalRuntime {
        // Neutral role behaviour: forwarded to `NeutralRuntime` rather than scripted here.
        async fn prepare_outcome(
            &self,
            thread: &str,
            outcome_id: &str,
            description: &str,
            rubric: &str,
            max_iterations: u32,
        ) -> Result<u64, RunError> {
            crate::NeutralRuntime
                .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
                .await
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
            _executor: &dyn crate::PreparedSessionRunAdmission,
        ) -> Result<OutcomeDrive, RunError> {
            unreachable!("not exercised by the default-method tests")
        }

        // Neutral role behaviour: forwarded to `NeutralRuntime` rather than scripted here.
        async fn continue_outcome(
            &self,
            thread: &str,
            executor: &dyn crate::PreparedSessionRunAdmission,
        ) -> Result<Option<OutcomeDrive>, RunError> {
            crate::NeutralRuntime
                .continue_outcome(thread, executor)
                .await
        }
    }

    // Item 1: the fail-closed DEFAULT live-inbox methods all reject with `Inactive`.
    // Item 1: the other fail-closed defaults — ownership false, committed empty.
    /// Cause-effect graph for a Runtime without generation support:
    ///
    /// C1 exact generation command reaches the default SessionRuns port
    ///   -> C2 no concrete generation adapter is installed
    ///   -> E1 reject before any stage/publish/drain effect.
    ///
    /// Decision table (the test cases below are generated one-for-one):
    ///
    /// | Rule | command | C1 | C2 | result |
    /// |---|---|---|---|---|
    /// | R1 | stage | T | T | `mcp_runtime_unsupported` |
    /// | R2 | publish | T | T | `mcp_runtime_unsupported` |
    /// | R3 | drain | T | T | `mcp_runtime_unsupported` |
    #[test]
    fn core_only_runtime_owns_no_role() {
        // Capability ladder (ADR-0081 §3): C1 a runtime that implements only the
        // four core operations. R1 role accessors are absent from the core, so
        // coordination queries, terminal publication, live-inbox edits,
        // thread probes and streaming runs are unreachable at the type level
        // instead of answered by a refusing default; R2 the core surface is
        // still there, which is what an ephemeral in-process host needs.
        struct CoreOnlyRuntime;
        #[async_trait]
        impl SessionRuns for CoreOnlyRuntime {
            async fn run(
                &self,
                agent: &str,
                thread: &str,
                content: Vec<ContentBlock>,
            ) -> Result<StepOutcome, RunError> {
                MinimalRuntime.run(agent, thread, content).await
            }
            async fn resume(
                &self,
                thread: &str,
                tool_use_id: &str,
                decision: PermissionDecision,
            ) -> Result<StepOutcome, RunError> {
                MinimalRuntime.resume(thread, tool_use_id, decision).await
            }
            async fn resume_custom(
                &self,
                thread: &str,
                tool_use_id: &str,
                content: Vec<ContentBlock>,
                is_error: bool,
            ) -> Result<StepOutcome, RunError> {
                MinimalRuntime
                    .resume_custom(thread, tool_use_id, content, is_error)
                    .await
            }
            fn model(&self) -> String {
                MinimalRuntime.model()
            }
        }
        let rt = CoreOnlyRuntime;
        let core: &dyn SessionRuns = &rt;
        assert_eq!(core.model(), "test-model");
    }

    #[tokio::test]
    async fn explicit_unsupported_mcp_fixture_fails_closed_from_decision_table() {
        use crate::{McpGenerationRef, McpTarget, StageMcpAttachment};

        struct UnsupportedRealizer;
        #[async_trait]
        impl McpAttachmentRealizer for UnsupportedRealizer {
            async fn stage_mcp_attachment(
                &self,
                _request: crate::StageMcpAttachment,
            ) -> Result<crate::McpRealizationReceipt, RunError> {
                Err(RunError::classified(
                    "mcp_runtime_unsupported",
                    "fixture does not support generation-fenced MCP realization",
                ))
            }

            async fn publish_mcp_generation(
                &self,
                _generation: crate::McpGenerationRef,
            ) -> Result<(), RunError> {
                Err(RunError::classified(
                    "mcp_runtime_unsupported",
                    "fixture does not support generation-fenced MCP publication",
                ))
            }

            async fn drain_mcp_generation(
                &self,
                _generation: crate::McpGenerationRef,
            ) -> Result<(), RunError> {
                Err(RunError::classified(
                    "mcp_runtime_unsupported",
                    "fixture does not support generation-fenced MCP drain",
                ))
            }
        }
        let runtime = UnsupportedRealizer;
        let generation = McpGenerationRef::from_persisted(
            "session-1".into(),
            crate::mcp_attachment::test_generation("mcp-1", 1, "https://mcp.example.test"),
            "runtime-1".into(),
            7,
            u64::MAX,
        )
        .expect("MCP generation fence epoch is nonzero");
        let stage = StageMcpAttachment {
            workspace_id: "workspace-1".into(),
            generation: generation.clone(),
            realization_id: "realization-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "calculator".into(),
            target: McpTarget::parse_http("https://mcp.example.test").unwrap(),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };

        let stage_error = runtime.stage_mcp_attachment(stage).await.unwrap_err();
        let publish_error = runtime
            .publish_mcp_generation(generation.clone())
            .await
            .unwrap_err();
        let drain_error = runtime.drain_mcp_generation(generation).await.unwrap_err();
        for (rule, error) in [
            ("R1", stage_error),
            ("R2", publish_error),
            ("R3", drain_error),
        ] {
            assert_eq!(error.code, "mcp_runtime_unsupported", "{rule}");
            assert_eq!(error.kind, RunErrorKind::Internal, "{rule}");
        }
    }

    // Item 1: `run_streaming`'s default delegates to `run` — the committed outcome is
    // identical (the sink only mirrors in-flight events, which the default ignores).
    #[test]
    fn committed_resume_ticket_has_one_pending_projection_decision_table() {
        use awaken_agent_contract::agent::awaiting::{
            AwaitTarget, PauseReason, PendingTool as TicketPendingTool, RemoteInputReason,
            ResumeTicket, ToolAwaitReason,
        };

        // Causes: C1 each closed ToolCall reason is Permission, ClientExecution,
        // ScheduledAction, or Delegation; C2 each RemoteInput reason is UserInput
        // or ExternalEvent; C3 each Pause reason is Manual, RateLimit, or
        // BudgetReached. Effects: E1 preserve the exact call/tool and classify a
        // built-in confirmation; E2 preserve the exact call/tool and classify a
        // client execution; E3 synthesize client-executed agent_input with the
        // exact reason token; E4 omit system-owned waits. This is the sole
        // foreground/cold decoder.
        //
        // Decision table:
        // | Rule | Closed target | Effect |
        // |---|---|---|
        // | P1 | ToolCall(Permission) | E1 |
        // | P2 | ToolCall(ClientExecution) | E2 |
        // | P3 | RemoteInput(UserInput) | E3 user_input |
        // | P4 | RemoteInput(ExternalEvent) | E3 external_event |
        // | P5 | ToolCall(ScheduledAction) | E4 |
        // | P6 | ToolCall(Delegation) | E4 |
        // | P7 | Pause(Manual) | E4 |
        // | P8 | Pause(RateLimit) | E4 |
        // | P9 | Pause(BudgetReached) | E4 |
        // Constraints/invariants: K1 ToolCall necessarily carries call id and
        // PendingTool, so projection has no malformed-payload error; K2 internal
        // waits never become required action; K3 exhaustive matching makes a new
        // closed target fail compilation until this authority classifies it.
        let ticket = |target| {
            ResumeTicket::new(
                "correlation",
                RunId("run".into()),
                awaken_agent_contract::agent::thread::Id("thread".into()),
                "snapshot",
                "catalog",
                target,
            )
        };
        let concrete = || TicketPendingTool {
            tool_id: "calculator".into(),
            arguments: serde_json::json!({"value": 7}),
        };

        let tool = |reason| AwaitTarget::ToolCall {
            reason,
            call_id: "call".into(),
            tool: concrete(),
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        };
        let exact_tool = |client_executed| {
            Some(Pending {
                tool_use_id: "call".into(),
                name: "calculator".into(),
                input: serde_json::json!({"value": 7}),
                client_executed,
            })
        };
        let agent_input = |reason| {
            Some(Pending {
                tool_use_id: "call".into(),
                name: "agent_input".into(),
                input: serde_json::json!({"reason": reason}),
                client_executed: true,
            })
        };
        let cases = [
            (
                "P1/E1",
                tool(ToolAwaitReason::Permission),
                exact_tool(false),
            ),
            (
                "P2/E2",
                tool(ToolAwaitReason::ClientExecution),
                exact_tool(true),
            ),
            (
                "P3/E3",
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::UserInput,
                    call_id: "call".into(),
                },
                agent_input("user_input"),
            ),
            (
                "P4/E3",
                AwaitTarget::RemoteInput {
                    reason: RemoteInputReason::ExternalEvent,
                    call_id: "call".into(),
                },
                agent_input("external_event"),
            ),
            ("P5/E4", tool(ToolAwaitReason::ScheduledAction), None),
            ("P6/E4", tool(ToolAwaitReason::Delegation), None),
            ("P7/E4", AwaitTarget::Pause(PauseReason::Manual), None),
            ("P8/E4", AwaitTarget::Pause(PauseReason::RateLimit), None),
            (
                "P9/E4",
                AwaitTarget::Pause(PauseReason::BudgetReached),
                None,
            ),
        ];

        for (rule, target, expected) in cases {
            assert_eq!(
                Pending::from_resume_ticket(&ticket(target)),
                expected,
                "{rule}"
            );
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};

    /// @exhaustive unit(1) = 1
    /// @obligation session_outcome.awaiting_constructor_is_nonterminal_and_nonfailed
    #[test]
    fn awaiting_constructor_cannot_create_a_terminal_or_failed_outcome() {
        // Causes: the Awaiting constructor receives no pending tool. Effects:
        // state is Awaiting and failure is absent. Decision rule K1: any empty
        // message vector+None pending=>Awaiting and no failure authority.
        let outcome = StepOutcome::awaiting(Vec::new(), None);
        assert!(matches!(outcome.state(), RunState::Awaiting));
        assert!(outcome.failure().is_none());
    }

    /// @exhaustive EndCause{Error(CapabilityBound),NaturalEnd}(2) = 2
    /// @obligation session_outcome.ended_constructor_owns_failure_and_clears_pending_tool
    #[test]
    fn ended_constructor_carries_the_only_failure_authority_and_no_pending_tool() {
        // Causes: an Ended constructor receives either a classified error or
        // natural completion. Effects: both are terminal, only the error exposes
        // Failure, and neither has pending work. Decision rule K2:
        // error=>failure; natural=>none; both=>Ended+no pending.
        for cause in [
            EndCause::Error(Failure::CapabilityBound),
            EndCause::NaturalEnd,
        ] {
            let is_error = matches!(&cause, EndCause::Error(_));
            let outcome = StepOutcome::ended(Vec::new(), cause);
            assert!(matches!(outcome.state(), RunState::Ended(_)));
            assert!(outcome.pending().is_none());
            assert_eq!(outcome.failure().is_some(), is_error);
        }
    }

    /// @exhaustive unit(1) = 1
    /// @obligation session_execution.settled_step_has_no_parallel_observation_authority
    #[test]
    fn settled_step_has_no_parallel_observation_authority() {
        // A terminal committed Run cause creates an Ended Step outcome;
        // compaction, retry, and model-request observations remain exclusively
        // in the committed recovery snapshot audit stream.
        let ended = StepOutcome::ended(Vec::new(), EndCause::NaturalEnd);
        assert!(matches!(
            ended.state(),
            RunState::Ended(EndCause::NaturalEnd)
        ));
        assert!(ended.pending().is_none());
    }
}

mod execution_recovery;
pub use execution_recovery::{
    SessionExecutionClosureReference, SessionExecutionEvidenceSelection,
    SessionExecutionOperationFootprint, SessionExecutionOperationInventory,
    SessionExecutionRecoveryGap, SessionExecutionRecoveryObservation,
    SessionExecutionToolOperation, SessionRecoveryPreparationReference,
    SessionSourceDispatchObservation,
};

mod execution_observation;
pub use execution_observation::{
    SessionExecutionObservation, SessionModelRequestObservation, SessionRunExecutionObservation,
    SessionToolUseObservation,
};

pub(crate) mod continuation;
