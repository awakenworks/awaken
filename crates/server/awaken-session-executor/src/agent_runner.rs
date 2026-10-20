// @formal evidence delegation.independent_child_scheduling_lease
//! Constructing and running an Agent through the ordinary Run lifecycle.
//!
//! Native delegation (`agent_run`) and skill forks use this
//! substrate. A delegated Agent receives a first-class child Run identity, the
//! same durable context and capabilities as a directly initiated Agent, and the
//! same delegation executor, so nested delegation is not a special execution
//! path. Auxiliary Agents may deliberately request transient identity and an
//! isolated context because their work is outside the user-visible Run tree.
//!
//! [`root_execution::run_configured_agent`] is the parameterized form: it resolves the Agent's
//! own `ExecutableAgentSnapshot` (instructions, model, tools) from an
//! [`AgentCatalog`](crate::agent_catalog::AgentCatalog) by id. [`run_agent`] is the thin wrapper that runs the default
//! `assistant` config with a plain string prompt.

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::llm::ThreadUsage;

use crate::config::latest_assistant_text;

mod child_attempt;
mod child_dispatch;
mod child_execution;
mod child_worker;
#[cfg(test)]
mod fixtures;
mod root_execution;
pub(crate) use child_attempt::{
    ChildAcpExecutorFactory, ChildExecutionAdapters, child_attempt_executor,
    configure_child_native_runtime, validate_child_tool_configuration,
};
pub(crate) use child_dispatch::child_dispatch_request;
pub(crate) use child_execution::{
    AbortOnDropTask, ChildRunRequest, RunScheduler, run_configured_agent_until_boundary,
};
pub(crate) use child_worker::{ChildWorkerFactory, ClaimedChildAttempt};
pub(crate) use root_execution::{
    AgentExecution, AgentRunError, AgentRunSandbox, run_agent, run_configured_agent_with_id,
};

/// One ordinary lifecycle boundary reached by an Agent Run.
///
/// Delegated and directly admitted Runs use the same `Runtime::execute` /
/// `Runtime::resume` transitions. The parent only observes whether its child
/// ended or is awaiting; the committed `ResumeTicket` remains the sole authority
/// for what may resume it.
pub(crate) enum AgentRunBoundary {
    Ended { text: String, usage: ThreadUsage },
    Awaiting,
}

/// Project one committed settled state into the shared root/child boundary.
/// This is the sole success/failure classifier for configured Agent Runs.
fn settled_agent_boundary(
    reader: &dyn CommittedThreadView,
    thread_id: &ThreadId,
    state: RunState,
) -> Result<AgentRunBoundary, AgentRunError> {
    match state {
        RunState::Awaiting => Ok(AgentRunBoundary::Awaiting),
        RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
            | awaken_agent_contract::agent::run::EndCause::MaxSteps,
        ) => Ok(AgentRunBoundary::Ended {
            text: latest_assistant_text(&reader.committed_messages(thread_id)),
            usage: ThreadUsage::from_committed_state(&reader.committed_state(thread_id)),
        }),
        RunState::Ended(cause) => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "Agent Run ended unsuccessfully: {cause:?}"
            )),
        )),
        RunState::Running => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(
                "an Agent Run escaped without reaching a settled boundary".to_string(),
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_store_inmem::MemoryCommitCoordinator;

    use super::fixtures::agent;
    use super::{AgentRunBoundary, settled_agent_boundary};
    use crate::config::effective_tool_authorization;

    #[tokio::test]
    async fn every_agent_run_uses_its_frozen_toolset_authorization() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 a frozen Agent publication has an Agent
        // toolset; C2 web_search is enabled+always_allow while web_fetch is
        // enabled+always_ask. E1 search executes without HITL; E2 fetch awaits;
        // C3 no authored policy/toolset retains the strict runtime baseline.
        //
        // Decision table:
        // | C1 policy                         | effective gate    |
        // | web_search enabled/always_allow  | allow (R1/E1)     |
        // | web_fetch enabled/always_ask     | confirm (R2/E2)   |
        // | no explicit policy               | no override (R3)  |
        // FMECA: a root/child/recovery-specific compiler can strand an otherwise
        // autonomous Run at Awaiting or let an initiator silently broaden another
        // Agent. The same effective authorization compiler owns every Run shape.
        use awaken_agent_contract::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        use awaken_runtime_contract::permission::GateOutcome;
        use awaken_tool_contract::tool::ToolCall;

        let mut child = agent("research-child", "research");
        child.resolved_spec.plugin_config.agent.toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: Default::default(),
            overrides: vec![
                ToolPolicyOverride::new(
                    "web_search",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                ),
                ToolPolicyOverride::new(
                    "web_fetch",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                ),
            ],
        }];
        let authorization = effective_tool_authorization(
            &child.resolved_spec.plugin_config,
            &[],
            &child.resolved_spec.plugin_config.agent.toolsets,
        );
        let gate = authorization.gate;
        let state = awaken_agent_contract::agent::state::Store::default();
        let call = |tool_id: &str| ToolCall {
            tool_id: tool_id.into(),
            call_id: format!("call-{tool_id}"),
            arguments: serde_json::json!({}),
        };
        assert_eq!(
            gate.gate(&call("web_search"), &state).await.outcome,
            GateOutcome::Allow,
            "R1/E1"
        );
        assert!(
            matches!(
                gate.gate(&call("web_fetch"), &state).await.outcome,
                GateOutcome::RequireConfirmation { .. }
            ),
            "R2/E2"
        );
        let plain = agent("plain-child", "plain");
        let baseline = effective_tool_authorization(
            &plain.resolved_spec.plugin_config,
            &[],
            &plain.resolved_spec.plugin_config.agent.toolsets,
        );
        let unlisted = baseline.gate.gate(&call("web_search"), &state).await;
        assert!(
            matches!(unlisted.outcome, GateOutcome::RequireConfirmation { .. }),
            "R3"
        );
    }

    #[test]
    fn child_terminal_cause_is_not_collapsed_to_success() {
        // Cause/effect graph: child boundary is Awaiting/NaturalEnd/MaxSteps/
        // Cancelled/Stopped/Error/Indeterminate. Effects are preserve Awaiting,
        // return successful child output, or surface an AgentRunError.
        // Decision table: R1 Awaiting -> awaiting; R2 NaturalEnd|MaxSteps ->
        // success; R3 every other EndCause -> error. This closes the former
        // `Ended(_)` wildcard that reclassified remote failures as empty success.
        let reader = MemoryCommitCoordinator::new(awaken_time::system_clock());
        let thread = ThreadId("child-terminal".into());
        assert!(matches!(
            settled_agent_boundary(&reader, &thread, RunState::Awaiting),
            Ok(AgentRunBoundary::Awaiting)
        ));
        for cause in [EndCause::NaturalEnd, EndCause::MaxSteps] {
            assert!(matches!(
                settled_agent_boundary(&reader, &thread, RunState::Ended(cause)),
                Ok(AgentRunBoundary::Ended { .. })
            ));
        }
        for cause in [
            EndCause::Cancelled,
            EndCause::Stopped("budget".into()),
            EndCause::Indeterminate,
            EndCause::Error(awaken_agent_contract::agent::run::Failure::Inference {
                code: "a2a_task_failed".into(),
                message: "remote failed".into(),
            }),
        ] {
            assert!(
                settled_agent_boundary(&reader, &thread, RunState::Ended(cause)).is_err(),
                "R3"
            );
        }
    }
}
