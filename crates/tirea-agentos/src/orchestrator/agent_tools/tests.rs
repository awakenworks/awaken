use super::*;
use crate::contracts::reduce_state_actions;
use crate::contracts::runtime::plugin::agent::ReadOnlyContext;
use crate::contracts::runtime::plugin::phase::effect::{PhaseEffect, PhaseOutput};
use crate::contracts::runtime::plugin::phase::RunAction;
use crate::contracts::runtime::plugin::phase::{Phase, StepContext};
use crate::contracts::runtime::tool_call::ToolStatus;
use crate::contracts::thread::Thread;
use crate::contracts::AgentBehavior;
use crate::orchestrator::InMemoryAgentRegistry;
use crate::runtime::loop_runner::{
    TOOL_SCOPE_CALLER_AGENT_ID_KEY, TOOL_SCOPE_CALLER_MESSAGES_KEY, TOOL_SCOPE_CALLER_STATE_KEY,
    TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use async_trait::async_trait;
use serde_json::json;
use std::time::Duration;
use tirea_contract::testing::TestFixture;
use tirea_state::apply_patches;

/// Apply a [`PhaseOutput`] to the mutable [`StepContext`] for testing.
///
/// Replicates the effect-application logic from the agent-loop's
/// `effect_applicator` module, which is not publicly exported.
fn apply_phase_output_for_test(step: &mut StepContext<'_>, output: PhaseOutput) {
    for effect in output.effects {
        match effect {
            PhaseEffect::SystemContext(s) => step.system(s),
            PhaseEffect::SessionContext(s) => step.thread(s),
            PhaseEffect::SystemReminder(s) => step.reminder(s),
            PhaseEffect::ExcludeTool(id) => step.exclude(&id),
            PhaseEffect::IncludeOnlyTools(ids) => {
                let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
                step.include_only(&refs);
            }
            PhaseEffect::BlockTool(reason) => step.block(reason),
            PhaseEffect::AllowTool => step.allow(),
            PhaseEffect::SuspendTool(ticket) => step.suspend(ticket),
            PhaseEffect::OverrideToolResult(result) => step.set_tool_result(result),
            PhaseEffect::RequestTermination(reason) => {
                step.set_run_action(RunAction::Terminate(reason));
            }
        }
    }
    let tracked = reduce_state_actions(output.state_actions, &step.snapshot(), "agent")
        .expect("state action reduce should succeed");
    for patch in tracked {
        {
            let doc = step.ctx().doc();
            for op in patch.patch().ops() {
                let _ = doc.apply(op);
            }
        }
        step.emit_patch(patch);
    }
}

#[async_trait]
trait AgentBehaviorTestDispatch {
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>);
}

#[async_trait]
impl<T> AgentBehaviorTestDispatch for T
where
    T: AgentBehavior + ?Sized,
{
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>) {
        let ctx = ReadOnlyContext::new(
            phase,
            step.thread_id(),
            step.messages(),
            step.run_config(),
            step.ctx().doc(),
        );
        let output = match phase {
            Phase::RunStart => self.run_start(&ctx).await,
            Phase::StepStart => self.step_start(&ctx).await,
            Phase::BeforeInference => self.before_inference(&ctx).await,
            Phase::AfterInference => self.after_inference(&ctx).await,
            Phase::BeforeToolExecute => self.before_tool_execute(&ctx).await,
            Phase::AfterToolExecute => self.after_tool_execute(&ctx).await,
            Phase::StepEnd => self.step_end(&ctx).await,
            Phase::RunEnd => self.run_end(&ctx).await,
        };
        apply_phase_output_for_test(step, output);
    }
}

#[test]
fn plugin_filters_out_caller_agent() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("a", crate::orchestrator::AgentDefinition::new("mock"));
    reg.upsert("b", crate::orchestrator::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(AgentRunManager::new()));
    let rendered = plugin.render_available_agents(Some("a"), None);
    assert!(rendered.contains("<id>b</id>"));
    assert!(!rendered.contains("<id>a</id>"));
}

#[test]
fn plugin_filters_agents_by_scope_policy() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("writer", crate::orchestrator::AgentDefinition::new("mock"));
    reg.upsert(
        "reviewer",
        crate::orchestrator::AgentDefinition::new("mock"),
    );
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(AgentRunManager::new()));
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(SCOPE_ALLOWED_AGENTS_KEY, vec!["writer"]).unwrap();
    let rendered = plugin.render_available_agents(None, Some(&rt));
    assert!(rendered.contains("<id>writer</id>"));
    assert!(!rendered.contains("<id>reviewer</id>"));
}

#[tokio::test]
async fn plugin_adds_reminder_for_running_and_stopped_runs() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("worker", crate::orchestrator::AgentDefinition::new("mock"));
    let manager = Arc::new(AgentRunManager::new());
    let plugin = AgentToolsPlugin::new(Arc::new(reg), manager.clone());

    let epoch = manager
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "worker".to_string(),
            None,
            Thread::new("child-1"),
            None,
        )
        .await;
    assert_eq!(epoch, 1);

    let fixture = TestFixture::new();
    let mut step = StepContext::new(fixture.ctx(), "owner-1", &fixture.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
    let reminder = step
        .system_reminders
        .first()
        .expect("running reminder should be present");
    assert!(reminder.contains("status=\"running\""));

    manager.stop_owned_tree("owner-1", "run-1").await.unwrap();
    let fixture2 = TestFixture::new();
    let mut step2 = StepContext::new(fixture2.ctx(), "owner-1", &fixture2.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step2).await;
    let reminder2 = step2
        .system_reminders
        .first()
        .expect("stopped reminder should be present");
    assert!(reminder2.contains("status=\"stopped\""));
}

#[tokio::test]
async fn manager_ignores_stale_completion_by_epoch() {
    let manager = AgentRunManager::new();
    let epoch1 = manager
        .put_running(
            "run-1",
            "owner".to_string(),
            "agent-a".to_string(),
            None,
            Thread::new("s-1"),
            None,
        )
        .await;
    assert_eq!(epoch1, 1);

    let epoch2 = manager
        .put_running(
            "run-1",
            "owner".to_string(),
            "agent-a".to_string(),
            None,
            Thread::new("s-2"),
            None,
        )
        .await;
    assert_eq!(epoch2, 2);

    let ignored = manager
        .update_after_completion(
            "run-1",
            epoch1,
            AgentRunCompletion {
                thread: Thread::new("old"),
                status: DelegationStatus::Completed,
                assistant: Some("old".to_string()),
                error: None,
            },
        )
        .await;
    assert!(ignored.is_none());

    let summary = manager
        .get_owned_summary("owner", "run-1")
        .await
        .expect("run should still exist");
    assert_eq!(summary.status, DelegationStatus::Running);

    let applied = manager
        .update_after_completion(
            "run-1",
            epoch2,
            AgentRunCompletion {
                thread: Thread::new("new"),
                status: DelegationStatus::Completed,
                assistant: Some("new".to_string()),
                error: None,
            },
        )
        .await
        .expect("latest epoch completion should apply");
    assert_eq!(applied.status, DelegationStatus::Completed);
    assert_eq!(applied.assistant.as_deref(), Some("new"));
}

#[tokio::test]
async fn agent_run_tool_requires_scope_context() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));
    let fix = TestFixture::new();
    let result = tool
        .execute(
            json!({"agent_id":"worker","prompt":"hi","background":false}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing caller thread context"));
}

#[tokio::test]
async fn agent_run_tool_rejects_disallowed_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "reviewer",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    fix.run_config
        .set(SCOPE_ALLOWED_AGENTS_KEY, vec!["worker"])
        .unwrap();
    let result = tool
        .execute(
            json!({"agent_id":"reviewer","prompt":"hi","background":false}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[tokio::test]
async fn agent_run_tool_rejects_self_target_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "caller",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    let result = tool
        .execute(
            json!({"agent_id":"caller","prompt":"hi","background":false}),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown or unavailable agent_id"));
}

#[derive(Debug)]
struct SlowTerminatePlugin;

#[async_trait]
impl AgentBehavior for SlowTerminatePlugin {
    fn id(&self) -> &str {
        "slow_terminate_behavior_requested"
    }

    async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        tokio::time::sleep(Duration::from_millis(120)).await;
        PhaseOutput::default().terminate_behavior_requested()
    }
}

fn caller_scope_with_state_and_run(
    state: serde_json::Value,
    run_id: &str,
) -> tirea_contract::RunConfig {
    caller_scope_with_state_run_and_messages(
        state,
        run_id,
        vec![crate::contracts::thread::Message::user("seed message")],
    )
}

fn caller_scope_with_state_run_and_messages(
    state: serde_json::Value,
    run_id: &str,
    messages: Vec<crate::contracts::thread::Message>,
) -> tirea_contract::RunConfig {
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "owner-thread")
        .unwrap();
    rt.set(TOOL_SCOPE_CALLER_AGENT_ID_KEY, "caller").unwrap();
    rt.set(SCOPE_RUN_ID_KEY, run_id).unwrap();
    rt.set(TOOL_SCOPE_CALLER_STATE_KEY, state).unwrap();
    rt.set(TOOL_SCOPE_CALLER_MESSAGES_KEY, messages).unwrap();
    rt
}

fn caller_scope_with_state(state: serde_json::Value) -> tirea_contract::RunConfig {
    caller_scope_with_state_and_run(state, "parent-run-default")
}

fn caller_scope() -> tirea_contract::RunConfig {
    caller_scope_with_state(json!({"forked": true}))
}

#[tokio::test]
async fn agent_run_tool_fork_context_passes_non_system_messages_and_filters_unpaired_tool_calls() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());

    let fork_messages = vec![
        crate::contracts::thread::Message::system("parent-system"),
        crate::contracts::thread::Message::internal_system("parent-internal-system"),
        crate::contracts::thread::Message::user("parent-user-1"),
        crate::contracts::thread::Message::assistant_with_tool_calls(
            "parent-assistant-tool-call",
            vec![
                crate::contracts::thread::ToolCall::new(
                    "call-paired",
                    "search",
                    json!({"q":"paired"}),
                ),
                crate::contracts::thread::ToolCall::new(
                    "call-missing",
                    "search",
                    json!({"q":"missing"}),
                ),
            ],
        ),
        crate::contracts::thread::Message::tool("call-paired", "tool paired result"),
        crate::contracts::thread::Message::tool("call-orphan", "tool orphan result"),
        crate::contracts::thread::Message::assistant_with_tool_calls(
            "assistant-unpaired-only",
            vec![crate::contracts::thread::ToolCall::new(
                "call-only-assistant",
                "search",
                json!({"q":"only-assistant"}),
            )],
        ),
        crate::contracts::thread::Message::assistant("parent-assistant-plain"),
    ];

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope_with_state_run_and_messages(
        json!({"forked": true}),
        "parent-run-42",
        fork_messages,
    );

    let started = run_tool
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"child-prompt",
                "background": true,
                "fork_context": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let run_id = started.data["run_id"]
        .as_str()
        .expect("run_id should exist")
        .to_string();

    let child_thread = manager
        .owned_record("owner-thread", &run_id)
        .await
        .expect("child thread should be tracked");

    assert!(
        !child_thread.messages.iter().any(|m| m.role == Role::System),
        "forked child should not contain parent system messages"
    );

    assert!(
        child_thread
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == "parent-user-1"),
        "parent non-system user message should be forked"
    );
    assert!(
        child_thread
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == "child-prompt"),
        "new child prompt should be appended"
    );

    let assistant_tool_msg = child_thread
        .messages
        .iter()
        .find(|m| m.content == "parent-assistant-tool-call")
        .expect("assistant tool call message should be forked");
    let tool_calls = assistant_tool_msg
        .tool_calls
        .as_ref()
        .expect("paired tool call should be preserved");
    assert_eq!(tool_calls.len(), 1, "only paired tool call should be kept");
    assert_eq!(tool_calls[0].id, "call-paired");

    let unpaired_assistant = child_thread
        .messages
        .iter()
        .find(|m| m.content == "assistant-unpaired-only")
        .expect("assistant message without paired tool result should remain");
    assert!(
        unpaired_assistant.tool_calls.is_none(),
        "unpaired assistant tool_calls should be removed"
    );

    assert!(
        child_thread.messages.iter().any(|m| m.role == Role::Tool
            && m.tool_call_id.as_deref() == Some("call-paired")
            && m.content == "tool paired result"),
        "paired tool result should be kept"
    );
    assert!(
        !child_thread.messages.iter().any(|m| {
            m.role == Role::Tool
                && (m.tool_call_id.as_deref() == Some("call-orphan")
                    || m.tool_call_id.as_deref() == Some("call-missing")
                    || m.tool_call_id.as_deref() == Some("call-only-assistant"))
        }),
        "unpaired tool messages should be filtered out"
    );
}

#[tokio::test]
async fn background_stop_then_resume_completes() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());
    let stop_tool = AgentStopTool::new(manager);

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    let started = run_tool
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    assert_eq!(started.data["status"], json!("running"));
    let run_id = started.data["run_id"]
        .as_str()
        .expect("run_id should exist")
        .to_string();

    let mut stop_fix = TestFixture::new();
    stop_fix.run_config = caller_scope();
    let stopped = stop_tool
        .execute(
            json!({ "run_id": run_id.clone() }),
            &stop_fix.ctx_with("call-stop", "tool:agent_stop"),
        )
        .await
        .unwrap();
    assert_eq!(stopped.status, ToolStatus::Success);
    assert_eq!(stopped.data["status"], json!("stopped"));

    // Give cancelled background task a chance to flush stale completion.
    tokio::time::sleep(Duration::from_millis(30)).await;

    let resumed = run_tool
        .execute(
            json!({
                "run_id": run_id,
                "prompt":"resume",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    assert_eq!(resumed.data["status"], json!("completed"));
}

#[tokio::test]
async fn manager_stop_tree_stops_descendants() {
    let manager = AgentRunManager::new();
    manager
        .put_running(
            "parent-run",
            "owner-thread".to_string(),
            "agent-a".to_string(),
            None,
            Thread::new("parent-run-thread"),
            None,
        )
        .await;
    manager
        .put_running(
            "child-run",
            "owner-thread".to_string(),
            "agent-a".to_string(),
            Some("parent-run".to_string()),
            Thread::new("child-run-thread"),
            None,
        )
        .await;
    manager
        .put_running(
            "grandchild-run",
            "owner-thread".to_string(),
            "agent-a".to_string(),
            Some("child-run".to_string()),
            Thread::new("grandchild-run-thread"),
            None,
        )
        .await;
    manager
        .put_running(
            "other-owner-run",
            "other-owner".to_string(),
            "agent-b".to_string(),
            Some("parent-run".to_string()),
            Thread::new("other-owner-thread"),
            None,
        )
        .await;

    let stopped = manager
        .stop_owned_tree("owner-thread", "parent-run")
        .await
        .unwrap();

    assert_eq!(stopped.len(), 3);

    let parent = manager
        .get_owned_summary("owner-thread", "parent-run")
        .await
        .expect("parent run should exist");
    assert_eq!(parent.status, DelegationStatus::Stopped);

    let child = manager
        .get_owned_summary("owner-thread", "child-run")
        .await
        .expect("child run should exist");
    assert_eq!(child.status, DelegationStatus::Stopped);

    let grandchild = manager
        .get_owned_summary("owner-thread", "grandchild-run")
        .await
        .expect("grandchild run should exist");
    assert_eq!(grandchild.status, DelegationStatus::Stopped);

    let denied = manager
        .stop_owned_tree("owner-thread", "other-owner-run")
        .await;
    assert!(denied.is_err());
}

#[tokio::test]
async fn agent_run_tool_persists_run_state_patch() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    let started = run_tool
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let run_id = started.data["run_id"]
        .as_str()
        .expect("run_id should exist")
        .to_string();

    let patch = fix.ctx_with("call-run", "tool:agent_run").take_patch();
    assert!(
        !patch.patch().is_empty(),
        "expected tool to persist run snapshot into state"
    );
    let base = json!({});
    let updated = apply_patches(&base, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["agent_runs"]["runs"][&run_id]["status"],
        json!("running")
    );
}

#[tokio::test]
async fn agent_run_tool_binds_scope_run_id_and_parent_lineage() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope_with_state_and_run(json!({"forked": true}), "parent-run-42");
    let started = run_tool
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"start",
                "background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let run_id = started.data["run_id"]
        .as_str()
        .expect("run_id should exist")
        .to_string();

    let child_thread = manager
        .owned_record("owner-thread", &run_id)
        .await
        .expect("child thread should be tracked");
    assert_eq!(
        child_thread.parent_thread_id.as_deref(),
        Some("owner-thread")
    );

    let patch = fix.ctx_with("call-run", "tool:agent_run").take_patch();
    let base = json!({});
    let updated = apply_patches(&base, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["agent_runs"]["runs"][&run_id]["parent_run_id"],
        json!("parent-run-42")
    );
}

#[tokio::test]
async fn agent_run_tool_injects_prompt_into_child_thread() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    let started = run_tool
        .execute(
            json!({
                "agent_id":"worker",
                "prompt":"prompt-injected",
                "background": true
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.status, ToolStatus::Success);
    let run_id = started.data["run_id"]
        .as_str()
        .expect("run_id should exist")
        .to_string();

    let child_thread = manager
        .owned_record("owner-thread", &run_id)
        .await
        .expect("child thread should be tracked");
    let prompt_message = child_thread
        .messages
        .last()
        .expect("child thread should contain prompt message");
    assert_eq!(prompt_message.role, crate::contracts::thread::Role::User);
    assert_eq!(prompt_message.content, "prompt-injected");
}

#[tokio::test]
async fn agent_run_tool_resumes_from_persisted_state_without_live_record() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));

    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let doc = json!({
        "agent_runs": {
            "runs": {
                "run-1": {
                    "run_id": "run-1",
                    "target_agent_id": "worker",
                    "status": "stopped",
                    "thread": serde_json::to_value(&child_thread).unwrap()
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state(doc);
    let resumed = run_tool
        .execute(
            json!({
                "run_id":"run-1",
                "prompt":"resume",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    assert_eq!(resumed.data["status"], json!("completed"));
}

#[tokio::test]
async fn agent_run_tool_resume_injects_prompt_into_child_thread() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());

    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let doc = json!({
        "agent_runs": {
            "runs": {
                "run-1": {
                    "run_id": "run-1",
                    "target_agent_id": "worker",
                    "status": "stopped",
                    "thread": serde_json::to_value(&child_thread).unwrap()
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state(doc);
    let resumed = run_tool
        .execute(
            json!({
                "run_id":"run-1",
                "prompt":"resume-prompt",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    assert_eq!(resumed.data["status"], json!("completed"));

    let resumed_thread = manager
        .owned_record("owner-thread", "run-1")
        .await
        .expect("resumed run should be tracked");
    let prompt_message = resumed_thread
        .messages
        .last()
        .expect("resumed child thread should contain prompt message");
    assert_eq!(prompt_message.role, crate::contracts::thread::Role::User);
    assert_eq!(prompt_message.content, "resume-prompt");
}

#[tokio::test]
async fn agent_run_tool_resume_updates_parent_run_lineage() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let manager = Arc::new(AgentRunManager::new());
    let run_tool = AgentRunTool::new(os, manager.clone());

    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let doc = json!({
        "agent_runs": {
            "runs": {
                "run-1": {
                    "run_id": "run-1",
                    "parent_run_id": "old-parent",
                    "target_agent_id": "worker",
                    "status": "stopped",
                    "thread": serde_json::to_value(&child_thread).unwrap()
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state_and_run(doc.clone(), "new-parent-run");
    let resumed = run_tool
        .execute(
            json!({
                "run_id":"run-1",
                "prompt":"resume",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);

    let child_thread = manager
        .owned_record("owner-thread", "run-1")
        .await
        .expect("resumed run should be tracked");
    assert_eq!(
        child_thread.parent_thread_id.as_deref(),
        Some("owner-thread")
    );

    let patch = fix.ctx_with("call-run", "tool:agent_run").take_patch();
    let updated = apply_patches(&doc, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["agent_runs"]["runs"]["run-1"]["parent_run_id"],
        json!("new-parent-run")
    );
}

#[tokio::test]
async fn agent_run_tool_marks_orphan_running_as_stopped_before_resume() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::orchestrator::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(AgentRunManager::new()));

    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let doc = json!({
        "agent_runs": {
            "runs": {
                "run-1": {
                    "run_id": "run-1",
                    "target_agent_id": "worker",
                    "status": "running",
                    "thread": serde_json::to_value(&child_thread).unwrap()
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state(doc);
    let summary = run_tool
        .execute(
            json!({
                "run_id":"run-1",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(summary.status, ToolStatus::Success);
    assert_eq!(summary.data["status"], json!("stopped"));
}

#[tokio::test]
async fn agent_stop_tool_stops_descendant_runs() {
    let manager = Arc::new(AgentRunManager::new());
    let stop_tool = AgentStopTool::new(manager.clone());
    let os_thread = Thread::new("owner-thread");

    let parent_thread = Thread::new("parent-s");
    let child_thread = Thread::new("child-s");
    let grandchild_thread = Thread::new("grandchild-s");
    let parent_run_id = "run-parent";
    let child_run_id = "run-child";
    let grandchild_run_id = "run-grandchild";

    manager
        .put_running(
            parent_run_id,
            os_thread.id.clone(),
            "worker".to_string(),
            None,
            parent_thread.clone(),
            None,
        )
        .await;
    manager
        .put_running(
            child_run_id,
            os_thread.id.clone(),
            "worker".to_string(),
            Some(parent_run_id.to_string()),
            child_thread.clone(),
            None,
        )
        .await;
    manager
        .put_running(
            grandchild_run_id,
            os_thread.id.clone(),
            "worker".to_string(),
            Some(child_run_id.to_string()),
            grandchild_thread.clone(),
            None,
        )
        .await;

    let doc = json!({
        "agent_runs": {
            "runs": {
                parent_run_id: {
                    "run_id": parent_run_id,
                    "target_agent_id": "worker",
                    "status": "running",
                    "thread": serde_json::to_value(parent_thread).unwrap()
                },
                child_run_id: {
                    "run_id": child_run_id,
                    "parent_run_id": parent_run_id,
                    "target_agent_id": "worker",
                    "status": "running",
                    "thread": serde_json::to_value(child_thread).unwrap()
                },
                grandchild_run_id: {
                    "run_id": grandchild_run_id,
                    "parent_run_id": child_run_id,
                    "target_agent_id": "worker",
                    "status": "running",
                    "thread": serde_json::to_value(grandchild_thread).unwrap()
                }
            }
        }
    });

    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = {
        let mut rt = tirea_contract::RunConfig::new();
        rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, os_thread.id.clone())
            .unwrap();
        rt
    };
    let result = stop_tool
        .execute(
            json!({ "run_id": parent_run_id }),
            &fix.ctx_with("call-stop", "tool:agent_stop"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("stopped"));

    let parent = manager
        .get_owned_summary(&os_thread.id, parent_run_id)
        .await
        .expect("parent run should exist");
    assert_eq!(parent.status, DelegationStatus::Stopped);
    let child = manager
        .get_owned_summary(&os_thread.id, child_run_id)
        .await
        .expect("child run should exist");
    assert_eq!(child.status, DelegationStatus::Stopped);
    let grandchild = manager
        .get_owned_summary(&os_thread.id, grandchild_run_id)
        .await
        .expect("grandchild run should exist");
    assert_eq!(grandchild.status, DelegationStatus::Stopped);

    let patch = fix.ctx_with("call-stop", "tool:agent_stop").take_patch();
    let updated = apply_patches(&doc, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["agent_runs"]["runs"][parent_run_id]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["agent_runs"]["runs"][child_run_id]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["agent_runs"]["runs"][grandchild_run_id]["status"],
        json!("stopped")
    );
}

#[tokio::test]
async fn recovery_plugin_reconciles_orphan_running_and_records_confirmation() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;
    assert!(matches!(
        step.run_action(),
        crate::contracts::RunAction::Continue
    ));

    let updated = fixture.updated_state();
    assert_eq!(
        updated["agent_runs"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["suspension"]["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["suspension"]
            ["parameters"]["run_id"],
        json!("run-1")
    );

    let fixture2 = TestFixture::new_with_state(updated);
    let mut before = fixture2.step(vec![]);
    plugin.run_phase(Phase::BeforeInference, &mut before).await;
    assert!(
        matches!(before.run_action(), crate::contracts::RunAction::Continue),
        "recovery plugin should not control inference flow in BeforeInference"
    );
}

#[tokio::test]
async fn recovery_plugin_does_not_override_existing_suspended_interaction() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "__suspended_tool_calls": {
                "calls": {
                    "existing_1": {
                        "call_id": "existing_1",
                        "tool_name": "confirm",
                        "suspension": {
                            "id": "existing_1",
                            "action": "confirm",
                        },
                        "arguments": {},
                        "pending": {
                            "id": "existing_1",
                            "name": "confirm",
                            "arguments": {}
                        },
                        "resume_mode": "pass_decision_to_tool"
                    }
                }
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );

    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;
    assert!(
        matches!(step.run_action(), crate::contracts::RunAction::Continue),
        "existing suspended interaction should not be replaced"
    );

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["existing_1"]["suspension"]["id"],
        json!("existing_1")
    );
}

#[tokio::test]
async fn recovery_plugin_auto_approve_when_permission_allow() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "allow"
                }
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_states"]["calls"]["agent_recovery_run-1"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_states"]["calls"]["agent_recovery_run-1"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["tool_name"],
        json!("agent_run")
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["suspension"]
            ["parameters"]["run_id"],
        json!("run-1")
    );
    assert_eq!(
        updated["agent_runs"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
}

#[tokio::test]
async fn recovery_plugin_auto_deny_when_permission_deny() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "deny"
                }
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_states")
            .and_then(|v| v.get("calls"))
            .and_then(|calls| calls.get("agent_recovery_run-1"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert_eq!(
        updated["agent_runs"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
    assert!(updated
        .get("__suspended_tool_calls")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_object())
        .map_or(true, |calls| calls.is_empty()));
}

#[tokio::test]
async fn recovery_plugin_auto_approve_from_default_behavior_allow() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {}
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert_eq!(
        updated["__tool_call_states"]["calls"]["agent_recovery_run-1"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_states"]["calls"]["agent_recovery_run-1"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["tool_name"],
        json!("agent_run")
    );
}

#[tokio::test]
async fn recovery_plugin_auto_deny_from_default_behavior_deny() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {}
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_states")
            .and_then(|v| v.get("calls"))
            .and_then(|calls| calls.get("agent_recovery_run-1"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert!(updated
        .get("__suspended_tool_calls")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_object())
        .map_or(true, |calls| calls.is_empty()));
}

#[tokio::test]
async fn recovery_plugin_tool_rule_overrides_default_behavior() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(AgentRunManager::new()));
    let child_thread = crate::contracts::thread::Thread::new("child-run")
        .with_message(crate::contracts::thread::Message::user("seed"));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {
                    "recover_agent_run": "ask"
                }
            },
            "agent_runs": {
                "runs": {
                    "run-1": {
                        "run_id": "run-1",
                        "target_agent_id": "worker",
                        "status": "running",
                        "thread": serde_json::to_value(&child_thread).unwrap()
                    }
                }
            }
        }),
    );
    let doc = thread.rebuild_state().unwrap();
    let fixture = TestFixture::new_with_state(doc);
    let mut step = fixture.step(vec![]);
    plugin.run_phase(Phase::RunStart, &mut step).await;

    let updated = fixture.updated_state();
    assert!(
        updated
            .get("__tool_call_states")
            .and_then(|v| v.get("calls"))
            .and_then(|calls| calls.get("agent_recovery_run-1"))
            .is_none(),
        "tool-level ask should not set recovery tool-call resume state"
    );
    assert_eq!(
        updated["__suspended_tool_calls"]["calls"]["agent_recovery_run-1"]["suspension"]["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
}

// ── Legacy schema migration tests ────────────────────────────────────────────

#[test]
fn parse_persisted_runs_from_doc_reads_new_path() {
    let doc = json!({
        "agent_runs": {
            "runs": {
                "run-1": {
                    "run_id": "run-1",
                    "target_agent_id": "worker",
                    "status": "stopped"
                }
            }
        }
    });
    let runs = parse_persisted_runs_from_doc(&doc);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs["run-1"].status, DelegationStatus::Stopped);
}

#[test]
fn parse_persisted_runs_from_doc_empty_returns_empty() {
    let runs = parse_persisted_runs_from_doc(&json!({}));
    assert!(runs.is_empty());
}
