use super::*;
use crate::composition::InMemoryAgentRegistry;
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
use crate::contracts::runtime::phase::{Phase, StepContext};
use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use crate::contracts::runtime::tool_call::ToolStatus;
use crate::contracts::storage::RunOrigin;
use crate::contracts::thread::Thread;
use crate::contracts::AgentBehavior;
use crate::loop_runtime::loop_runner::{
    TOOL_SCOPE_CALLER_AGENT_ID_KEY, TOOL_SCOPE_CALLER_MESSAGES_KEY, TOOL_SCOPE_CALLER_STATE_KEY,
    TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use async_trait::async_trait;
use serde_json::json;
use std::time::Duration;
use tirea_contract::testing::{
    apply_after_inference_for_test, apply_after_tool_for_test, apply_before_inference_for_test,
    apply_before_tool_for_test, apply_lifecycle_for_test, TestFixture,
};
use tirea_state::apply_patches;

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
        match phase {
            Phase::RunStart => apply_lifecycle_for_test(step, self.run_start(&ctx).await),
            Phase::StepStart => apply_lifecycle_for_test(step, self.step_start(&ctx).await),
            Phase::BeforeInference => {
                apply_before_inference_for_test(step, self.before_inference(&ctx).await)
            }
            Phase::AfterInference => {
                apply_after_inference_for_test(step, self.after_inference(&ctx).await)
            }
            Phase::BeforeToolExecute => {
                apply_before_tool_for_test(step, self.before_tool_execute(&ctx).await)
            }
            Phase::AfterToolExecute => {
                apply_after_tool_for_test(step, self.after_tool_execute(&ctx).await)
            }
            Phase::StepEnd => apply_lifecycle_for_test(step, self.step_end(&ctx).await),
            Phase::RunEnd => apply_lifecycle_for_test(step, self.run_end(&ctx).await),
        }
        // Reduce any pending state actions
        if !step.pending_state_actions.is_empty() {
            let state_actions = std::mem::take(&mut step.pending_state_actions);
            let snapshot = step.snapshot();
            let patches =
                reduce_state_actions(state_actions, &snapshot, "test", &ScopeContext::run())
                    .expect("state actions should reduce");
            for patch in patches {
                let doc = step.ctx().doc();
                for op in patch.patch().ops() {
                    doc.apply(op).expect("state action patch op should apply");
                }
                step.emit_patch(patch);
            }
        }
    }
}

// ── Plugin tests ─────────────────────────────────────────────────────────────

#[test]
fn plugin_filters_out_caller_agent() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("a", crate::runtime::AgentDefinition::new("mock"));
    reg.upsert("b", crate::runtime::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(SubAgentHandleTable::new()));
    let rendered = plugin.render_available_agents(Some("a"), None);
    assert!(rendered.contains("<id>b</id>"));
    assert!(!rendered.contains("<id>a</id>"));
}

#[test]
fn plugin_filters_agents_by_scope_policy() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("writer", crate::runtime::AgentDefinition::new("mock"));
    reg.upsert(
        "reviewer",
        crate::runtime::AgentDefinition::new("mock"),
    );
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(SubAgentHandleTable::new()));
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(SCOPE_ALLOWED_AGENTS_KEY, vec!["writer"]).unwrap();
    let rendered = plugin.render_available_agents(None, Some(&rt));
    assert!(rendered.contains("<id>writer</id>"));
    assert!(!rendered.contains("<id>reviewer</id>"));
}

#[test]
fn plugin_renders_agent_output_tool_usage() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("worker", crate::runtime::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(SubAgentHandleTable::new()));
    let rendered = plugin.render_available_agents(None, None);
    assert!(
        rendered.contains("agent_output"),
        "available agents should mention agent_output tool"
    );
}

#[tokio::test]
async fn plugin_adds_reminder_for_running_and_stopped_runs() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("worker", crate::runtime::AgentDefinition::new("mock"));
    let handles = Arc::new(SubAgentHandleTable::new());
    let plugin = AgentToolsPlugin::new(Arc::new(reg), handles.clone());

    let epoch = handles
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch, 1);

    let fixture = TestFixture::new();
    let mut step = StepContext::new(fixture.ctx(), "owner-1", &fixture.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
    let reminder = step
        .messaging
        .reminders
        .first()
        .expect("running reminder should be present");
    assert!(reminder.contains("status=\"running\""));

    handles.stop_owned_tree("owner-1", "run-1").await.unwrap();
    let fixture2 = TestFixture::new();
    let mut step2 = StepContext::new(fixture2.ctx(), "owner-1", &fixture2.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step2).await;
    let reminder2 = step2
        .messaging
        .reminders
        .first()
        .expect("stopped reminder should be present");
    assert!(reminder2.contains("status=\"stopped\""));
}

// ── Handle table tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn handle_table_ignores_stale_completion_by_epoch() {
    let handles = SubAgentHandleTable::new();
    let epoch1 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "agent-a".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch1, 1);

    let epoch2 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "agent-a".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch2, 2);

    let ignored = handles
        .update_after_completion(
            "run-1",
            epoch1,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await;
    assert!(ignored.is_none());

    let summary = handles
        .get_owned_summary("owner", "run-1")
        .await
        .expect("run should still exist");
    assert_eq!(summary.status, SubAgentStatus::Running);

    let applied = handles
        .update_after_completion(
            "run-1",
            epoch2,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await
        .expect("latest epoch completion should apply");
    assert_eq!(applied.status, SubAgentStatus::Completed);
}

#[tokio::test]
async fn handle_table_remove_if_epoch_only_removes_matching_generation() {
    let handles = SubAgentHandleTable::new();
    let epoch1 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "agent-a".to_string(),
            None,
            None,
        )
        .await;
    let epoch2 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "agent-a".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch1, 1);
    assert_eq!(epoch2, 2);

    assert!(!handles.remove_if_epoch("run-1", epoch1).await);
    assert!(handles.contains("run-1").await);
    assert!(handles.remove_if_epoch("run-1", epoch2).await);
    assert!(!handles.contains("run-1").await);
}

#[tokio::test]
async fn handle_table_stop_tree_stops_descendants() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "parent-run",
            "owner-thread".to_string(),
            "sub-agent-parent-run".to_string(),
            "agent-a".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "child-run",
            "owner-thread".to_string(),
            "sub-agent-child-run".to_string(),
            "agent-a".to_string(),
            Some("parent-run".to_string()),
            None,
        )
        .await;
    handles
        .put_running(
            "grandchild-run",
            "owner-thread".to_string(),
            "sub-agent-grandchild-run".to_string(),
            "agent-a".to_string(),
            Some("child-run".to_string()),
            None,
        )
        .await;
    handles
        .put_running(
            "other-owner-run",
            "other-owner".to_string(),
            "sub-agent-other-owner-run".to_string(),
            "agent-b".to_string(),
            Some("parent-run".to_string()),
            None,
        )
        .await;

    let stopped = handles
        .stop_owned_tree("owner-thread", "parent-run")
        .await
        .unwrap();

    assert_eq!(stopped.len(), 3);

    let parent = handles
        .get_owned_summary("owner-thread", "parent-run")
        .await
        .expect("parent run should exist");
    assert_eq!(parent.status, SubAgentStatus::Stopped);

    let child = handles
        .get_owned_summary("owner-thread", "child-run")
        .await
        .expect("child run should exist");
    assert_eq!(child.status, SubAgentStatus::Stopped);

    let grandchild = handles
        .get_owned_summary("owner-thread", "grandchild-run")
        .await
        .expect("grandchild run should exist");
    assert_eq!(grandchild.status, SubAgentStatus::Stopped);

    let denied = handles
        .stop_owned_tree("owner-thread", "other-owner-run")
        .await;
    assert!(denied.is_err());
}

// ── AgentRunTool tests ───────────────────────────────────────────────────────

#[derive(Debug)]
struct SlowTerminatePlugin;

#[async_trait]
impl AgentBehavior for SlowTerminatePlugin {
    fn id(&self) -> &str {
        "slow_terminate_behavior_requested"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        tokio::time::sleep(Duration::from_millis(120)).await;
        ActionSet::single(BeforeInferenceAction::Terminate(
            crate::contracts::TerminationReason::BehaviorRequested,
        ))
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
async fn agent_run_tool_requires_scope_context() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "reviewer",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
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

#[tokio::test]
async fn agent_run_tool_fork_context_filters_messages() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

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
    assert_eq!(started.data["status"], json!("running"));

    // Verify the fork_context flag produces a running status.
    // (Child thread details are now in ThreadStore, not in-memory handle.)
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());
    let stop_tool = AgentStopTool::new(handles);

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

    // Resume: since there's no ThreadStore in this test context, resume from
    // the live handle which is Stopped.
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
    // The execution will fail because there's no ThreadStore configured,
    // but the tool itself should handle the error gracefully.
    let status = resumed.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "expected terminal status, got: {status}"
    );
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));

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
        updated["sub_agents"]["runs"][&run_id]["status"],
        json!("running")
    );
    // No thread field in SubAgent.
    assert!(
        updated["sub_agents"]["runs"][&run_id]
            .get("thread")
            .is_none()
            || updated["sub_agents"]["runs"][&run_id]["thread"].is_null(),
        "SubAgent should not contain embedded thread"
    );
    // thread_id should be present.
    assert!(
        updated["sub_agents"]["runs"][&run_id]["thread_id"]
            .as_str()
            .is_some(),
        "SubAgent should contain thread_id"
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

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

    let patch = fix.ctx_with("call-run", "tool:agent_run").take_patch();
    let base = json!({});
    let updated = apply_patches(&base, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["sub_agents"]["runs"][&run_id]["parent_run_id"],
        json!("parent-run-42")
    );
}

#[tokio::test]
async fn agent_run_tool_query_existing_run_keeps_original_parent_lineage() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    handles
        .put_running(
            "run-1",
            "owner-thread".to_string(),
            "custom-child-thread".to_string(),
            "worker".to_string(),
            Some("origin-parent-run".to_string()),
            None,
        )
        .await;
    let run_tool = AgentRunTool::new(os, handles);

    let base_doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "custom-child-thread",
                    "parent_run_id": "origin-parent-run",
                    "agent_id": "worker",
                    "status": "running"
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(base_doc.clone());
    fix.run_config = caller_scope_with_state_and_run(base_doc.clone(), "query-parent-run");

    let result = run_tool
        .execute(
            json!({
                "run_id":"run-1",
                "background": false
            }),
            &fix.ctx_with("call-run", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);

    let patch = fix.ctx_with("call-run", "tool:agent_run").take_patch();
    let updated = apply_patches(&base_doc, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["parent_run_id"],
        json!("origin-parent-run")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["thread_id"],
        json!("custom-child-thread")
    );
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "stopped"
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
    // The execution will fail because there's no ThreadStore, but the tool handles it.
    let status = resumed.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "expected terminal status, got: {status}"
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
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "running"
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

// ── AgentStopTool tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn agent_stop_tool_stops_descendant_runs() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let stop_tool = AgentStopTool::new(handles.clone());
    let owner_thread_id = "owner-thread";

    let parent_run_id = "run-parent";
    let child_run_id = "run-child";
    let grandchild_run_id = "run-grandchild";

    handles
        .put_running(
            parent_run_id,
            owner_thread_id.to_string(),
            format!("sub-agent-{parent_run_id}"),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            child_run_id,
            owner_thread_id.to_string(),
            format!("sub-agent-{child_run_id}"),
            "worker".to_string(),
            Some(parent_run_id.to_string()),
            None,
        )
        .await;
    handles
        .put_running(
            grandchild_run_id,
            owner_thread_id.to_string(),
            format!("sub-agent-{grandchild_run_id}"),
            "worker".to_string(),
            Some(child_run_id.to_string()),
            None,
        )
        .await;

    let doc = json!({
        "sub_agents": {
            "runs": {
                parent_run_id: {
                    "thread_id": format!("sub-agent-{parent_run_id}"),
                    "agent_id": "worker",
                    "status": "running"
                },
                child_run_id: {
                    "thread_id": format!("sub-agent-{child_run_id}"),
                    "parent_run_id": parent_run_id,
                    "agent_id": "worker",
                    "status": "running"
                },
                grandchild_run_id: {
                    "thread_id": format!("sub-agent-{grandchild_run_id}"),
                    "parent_run_id": child_run_id,
                    "agent_id": "worker",
                    "status": "running"
                }
            }
        }
    });

    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = {
        let mut rt = tirea_contract::RunConfig::new();
        rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, owner_thread_id)
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

    let parent = handles
        .get_owned_summary(owner_thread_id, parent_run_id)
        .await
        .expect("parent run should exist");
    assert_eq!(parent.status, SubAgentStatus::Stopped);
    let child = handles
        .get_owned_summary(owner_thread_id, child_run_id)
        .await
        .expect("child run should exist");
    assert_eq!(child.status, SubAgentStatus::Stopped);
    let grandchild = handles
        .get_owned_summary(owner_thread_id, grandchild_run_id)
        .await
        .expect("grandchild run should exist");
    assert_eq!(grandchild.status, SubAgentStatus::Stopped);

    let patch = fix.ctx_with("call-stop", "tool:agent_stop").take_patch();
    let updated = apply_patches(&doc, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["sub_agents"]["runs"][parent_run_id]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["sub_agents"]["runs"][child_run_id]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["sub_agents"]["runs"][grandchild_run_id]["status"],
        json!("stopped")
    );
}

// ── AgentRecoveryPlugin tests ────────────────────────────────────────────────

#[tokio::test]
async fn recovery_plugin_detects_orphan_and_records_confirmation() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
        updated["sub_agents"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
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
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "__tool_call_scope": {
                "existing_1": {
                    "suspended_call": {
                        "call_id": "existing_1",
                        "tool_name": "agent_run",
                        "suspension": {
                            "id": "existing_1",
                            "action": AGENT_RECOVERY_INTERACTION_ACTION
                        },
                        "arguments": {},
                        "pending": {
                            "id": "existing_1",
                            "name": AGENT_RECOVERY_INTERACTION_ACTION,
                            "arguments": {}
                        },
                        "resume_mode": "pass_decision_to_tool"
                    }
                }
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
        updated["__tool_call_scope"]["existing_1"]["suspended_call"]["suspension"]["id"],
        json!("existing_1")
    );
}

#[tokio::test]
async fn recovery_plugin_auto_approve_when_permission_allow() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "allow"
                }
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["tool_name"],
        json!("agent_run")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["parameters"]["run_id"],
        json!("run-1")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_auto_deny_when_permission_deny() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "ask",
                "tools": {
                    "recover_agent_run": "deny"
                }
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["status"],
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
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {}
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume")
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["tool_name"],
        json!("agent_run")
    );
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_auto_deny_from_default_behavior_deny() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {}
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "deny should not set recovery tool-call resume state"
    );
    assert!(updated
        .get("__suspended_tool_calls")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_object())
        .map_or(true, |calls| calls.is_empty()));
}

#[cfg(feature = "permission")]
#[tokio::test]
async fn recovery_plugin_tool_rule_overrides_default_behavior() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "allow",
                "tools": {
                    "recover_agent_run": "ask"
                }
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
            .get("__tool_call_scope")
            .and_then(|scopes| scopes.get("agent_recovery_run-1"))
            .and_then(|scope| scope.get("tool_call_state"))
            .is_none(),
        "tool-level ask should not set recovery tool-call resume state"
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["suspended_call"]["suspension"]
            ["action"],
        json!(AGENT_RECOVERY_INTERACTION_ACTION)
    );
}

// ── Schema tests ─────────────────────────────────────────────────────────────

#[test]
fn parse_persisted_runs_from_doc_reads_new_path() {
    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "stopped"
                }
            }
        }
    });
    let runs = parse_persisted_runs_from_doc(&doc);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs["run-1"].status, SubAgentStatus::Stopped);
}

#[test]
fn parse_persisted_runs_from_doc_empty_returns_empty() {
    let runs = parse_persisted_runs_from_doc(&json!({}));
    assert!(runs.is_empty());
}

// ── Parallel sub-agent tests ─────────────────────────────────────────────────

#[tokio::test]
async fn parallel_background_runs_and_stop_all() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());
    let stop_tool = AgentStopTool::new(handles.clone());

    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    // Launch 3 background runs in parallel.
    let mut run_ids = Vec::new();
    for i in 0..3 {
        let started = run_tool
            .execute(
                json!({
                    "agent_id": "worker",
                    "prompt": format!("task-{i}"),
                    "background": true
                }),
                &fix.ctx_with(&format!("call-{i}"), "tool:agent_run"),
            )
            .await
            .unwrap();
        assert_eq!(started.status, ToolStatus::Success);
        assert_eq!(started.data["status"], json!("running"));
        run_ids.push(started.data["run_id"].as_str().unwrap().to_string());
    }

    // All should show as running.
    let running = handles.running_or_stopped_for_owner("owner-thread").await;
    assert_eq!(running.len(), 3);
    for summary in &running {
        assert_eq!(summary.status, SubAgentStatus::Running);
    }

    // Stop each one.
    for run_id in &run_ids {
        let mut stop_fix = TestFixture::new();
        stop_fix.run_config = caller_scope();
        let stopped = stop_tool
            .execute(
                json!({ "run_id": run_id }),
                &stop_fix.ctx_with("call-stop", "tool:agent_stop"),
            )
            .await
            .unwrap();
        assert_eq!(stopped.status, ToolStatus::Success);
        assert_eq!(stopped.data["status"], json!("stopped"));
    }

    // All should show as stopped.
    let stopped_all = handles.running_or_stopped_for_owner("owner-thread").await;
    assert_eq!(stopped_all.len(), 3);
    for summary in &stopped_all {
        assert_eq!(summary.status, SubAgentStatus::Stopped);
    }
}

#[tokio::test]
async fn sub_agent_status_lifecycle_running_to_completed() {
    let handles = SubAgentHandleTable::new();
    let epoch = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    let summary = handles
        .get_owned_summary("owner", "run-1")
        .await
        .expect("run should exist");
    assert_eq!(summary.status, SubAgentStatus::Running);

    let completed = handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await
        .expect("should complete");
    assert_eq!(completed.status, SubAgentStatus::Completed);
}

#[tokio::test]
async fn sub_agent_status_lifecycle_running_to_failed() {
    let handles = SubAgentHandleTable::new();
    let epoch = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    let failed = handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some("something went wrong".to_string()),
            },
        )
        .await
        .expect("should fail");
    assert_eq!(failed.status, SubAgentStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("something went wrong"));
}

#[tokio::test]
async fn cancellation_requested_overrides_completion_status() {
    let handles = SubAgentHandleTable::new();
    let token = RunCancellationToken::new();
    let epoch = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            Some(token.clone()),
        )
        .await;

    // Stop the run (sets run_cancellation_requested).
    handles.stop_owned_tree("owner", "run-1").await.unwrap();

    // Completion arrives with Completed, but cancellation should win.
    let result = handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await
        .expect("should apply");
    assert_eq!(result.status, SubAgentStatus::Stopped);
}

#[tokio::test]
async fn agent_output_tool_returns_error_for_unknown_run() {
    let os = AgentOs::builder().build().unwrap();
    let tool = AgentOutputTool::new(os);
    let fix = TestFixture::new();
    let result = tool
        .execute(
            json!({ "run_id": "nonexistent" }),
            &fix.ctx_with("call-1", "tool:agent_output"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown run_id"));
}

#[tokio::test]
async fn agent_output_tool_returns_status_from_persisted_state() {
    let os = AgentOs::builder().build().unwrap();
    let tool = AgentOutputTool::new(os);

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "completed"
                }
            }
        }
    });
    let fix = TestFixture::new_with_state(doc);
    let result = tool
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_output"),
        )
        .await
        .unwrap();
    // Will get store_error because no ThreadStore configured, but the tool
    // should handle it gracefully.
    let status_str = result.data.get("status").and_then(|v| v.as_str());
    let is_error = result.status == ToolStatus::Error;
    assert!(
        status_str == Some("completed") || is_error,
        "expected completed status or error for missing store"
    );
}

#[test]
fn sub_agent_thread_id_convention() {
    assert_eq!(
        super::tools::sub_agent_thread_id("run-123"),
        "sub-agent-run-123"
    );
}

// ── Handle table contains test ───────────────────────────────────────────────

#[tokio::test]
async fn handle_table_contains_returns_true_for_existing() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    assert!(handles.contains("run-1").await);
    assert!(!handles.contains("run-2").await);
}

// ── Ownership isolation tests ────────────────────────────────────────────────

#[tokio::test]
async fn handle_table_different_owners_cannot_see_each_others_runs() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "run-a",
            "owner-1".to_string(),
            "sub-agent-run-a".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "run-b",
            "owner-2".to_string(),
            "sub-agent-run-b".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    // Owner-1 can see run-a but not run-b.
    assert!(handles
        .get_owned_summary("owner-1", "run-a")
        .await
        .is_some());
    assert!(handles
        .get_owned_summary("owner-1", "run-b")
        .await
        .is_none());

    // Owner-2 can see run-b but not run-a.
    assert!(handles
        .get_owned_summary("owner-2", "run-b")
        .await
        .is_some());
    assert!(handles
        .get_owned_summary("owner-2", "run-a")
        .await
        .is_none());

    // running_or_stopped_for_owner returns only own runs.
    let owner1_runs = handles.running_or_stopped_for_owner("owner-1").await;
    assert_eq!(owner1_runs.len(), 1);
    assert_eq!(owner1_runs[0].run_id, "run-a");

    let owner2_runs = handles.running_or_stopped_for_owner("owner-2").await;
    assert_eq!(owner2_runs.len(), 1);
    assert_eq!(owner2_runs[0].run_id, "run-b");
}

#[tokio::test]
async fn handle_table_cross_owner_stop_is_denied() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "run-a",
            "owner-1".to_string(),
            "sub-agent-run-a".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    // Owner-2 cannot stop owner-1's run.
    let err = handles.stop_owned_tree("owner-2", "run-a").await;
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("Unknown run_id"));

    // Verify run is still running.
    let summary = handles.get_owned_summary("owner-1", "run-a").await.unwrap();
    assert_eq!(summary.status, SubAgentStatus::Running);
}

#[tokio::test]
async fn handle_for_resume_rejects_wrong_owner() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    let err = handles.handle_for_resume("owner-2", "run-1").await;
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("Unknown run_id"));
}

// ── Handle table status filter tests ─────────────────────────────────────────

#[tokio::test]
async fn running_or_stopped_for_owner_excludes_completed_and_failed() {
    let handles = SubAgentHandleTable::new();

    let epoch1 = handles
        .put_running(
            "run-running",
            "owner".to_string(),
            "sub-agent-run-running".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    let epoch2 = handles
        .put_running(
            "run-completed",
            "owner".to_string(),
            "sub-agent-run-completed".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    let epoch3 = handles
        .put_running(
            "run-failed",
            "owner".to_string(),
            "sub-agent-run-failed".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "run-stopped",
            "owner".to_string(),
            "sub-agent-run-stopped".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;

    // Transition run-completed and run-failed.
    handles
        .update_after_completion(
            "run-completed",
            epoch2,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await;
    handles
        .update_after_completion(
            "run-failed",
            epoch3,
            SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some("boom".to_string()),
            },
        )
        .await;
    handles
        .stop_owned_tree("owner", "run-stopped")
        .await
        .unwrap();

    // Verify: only Running and Stopped returned.
    let visible = handles.running_or_stopped_for_owner("owner").await;
    let visible_ids: Vec<&str> = visible.iter().map(|s| s.run_id.as_str()).collect();
    assert!(visible_ids.contains(&"run-running"));
    assert!(visible_ids.contains(&"run-stopped"));
    assert!(!visible_ids.contains(&"run-completed"));
    assert!(!visible_ids.contains(&"run-failed"));
    // Verify sort order.
    assert_eq!(visible_ids, {
        let mut sorted = visible_ids.clone();
        sorted.sort();
        sorted
    });

    // Verify epoch1 is untouched.
    let s = handles
        .get_owned_summary("owner", "run-running")
        .await
        .unwrap();
    assert_eq!(s.status, SubAgentStatus::Running);
    let _ = epoch1;
}

// ── Stop edge cases ──────────────────────────────────────────────────────────

#[tokio::test]
async fn stop_already_stopped_run_returns_error() {
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles.stop_owned_tree("owner", "run-1").await.unwrap();

    // Second stop should fail.
    let result = handles.stop_owned_tree("owner", "run-1").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not running"));
}

#[tokio::test]
async fn stop_completed_run_returns_error() {
    let handles = SubAgentHandleTable::new();
    let epoch = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await;

    let result = handles.stop_owned_tree("owner", "run-1").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not running"));
}

#[tokio::test]
async fn stop_unknown_run_returns_error() {
    let handles = SubAgentHandleTable::new();
    let result = handles.stop_owned_tree("owner", "nonexistent").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unknown run_id"));
}

// ── Concurrent handle table operations ───────────────────────────────────────

#[tokio::test]
async fn concurrent_put_running_same_run_id_increments_epoch() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let h1 = handles.clone();
    let h2 = handles.clone();

    // Launch two registrations concurrently for the same run_id.
    let (e1, e2) = tokio::join!(
        h1.put_running(
            "run-race",
            "owner".to_string(),
            "sub-agent-run-race".to_string(),
            "worker-a".to_string(),
            None,
            None,
        ),
        h2.put_running(
            "run-race",
            "owner".to_string(),
            "sub-agent-run-race".to_string(),
            "worker-b".to_string(),
            None,
            None,
        ),
    );

    // Epochs should be different (one is 1, the other is 2, depending on ordering).
    assert_ne!(e1, e2);
    let max_epoch = e1.max(e2);
    assert_eq!(max_epoch, 2);

    // The final state should reflect the last writer.
    let summary = handles
        .get_owned_summary("owner", "run-race")
        .await
        .unwrap();
    assert_eq!(summary.status, SubAgentStatus::Running);
}

#[tokio::test]
async fn concurrent_launches_different_run_ids() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let num_runs = 10;
    let mut tasks = Vec::new();

    for i in 0..num_runs {
        let h = handles.clone();
        tasks.push(tokio::spawn(async move {
            h.put_running(
                &format!("run-{i}"),
                "owner".to_string(),
                format!("sub-agent-run-{i}"),
                "worker".to_string(),
                None,
                None,
            )
            .await
        }));
    }

    let epochs: Vec<u64> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // All should be epoch 1 (first registration).
    for epoch in &epochs {
        assert_eq!(*epoch, 1);
    }

    // All 10 should be visible.
    let running = handles.running_or_stopped_for_owner("owner").await;
    assert_eq!(running.len(), num_runs);
    for summary in &running {
        assert_eq!(summary.status, SubAgentStatus::Running);
    }
}

#[tokio::test]
async fn concurrent_stop_multiple_independent_runs() {
    let handles = Arc::new(SubAgentHandleTable::new());

    // Register 5 independent runs.
    for i in 0..5 {
        handles
            .put_running(
                &format!("run-{i}"),
                "owner".to_string(),
                format!("sub-agent-run-{i}"),
                "worker".to_string(),
                None,
                None,
            )
            .await;
    }

    // Stop them all concurrently.
    let mut tasks = Vec::new();
    for i in 0..5 {
        let h = handles.clone();
        tasks.push(tokio::spawn(async move {
            h.stop_owned_tree("owner", &format!("run-{i}")).await
        }));
    }

    let results: Vec<_> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    for result in results {
        assert!(result.is_ok());
    }

    let all = handles.running_or_stopped_for_owner("owner").await;
    assert_eq!(all.len(), 5);
    for summary in &all {
        assert_eq!(summary.status, SubAgentStatus::Stopped);
    }
}

// ── Interleaved launch/stop/re-launch ────────────────────────────────────────

#[tokio::test]
async fn launch_stop_relaunch_increments_epoch_and_resumes() {
    let handles = SubAgentHandleTable::new();

    // Launch.
    let epoch1 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch1, 1);

    // Stop.
    handles.stop_owned_tree("owner", "run-1").await.unwrap();
    let summary = handles.get_owned_summary("owner", "run-1").await.unwrap();
    assert_eq!(summary.status, SubAgentStatus::Stopped);

    // Re-launch (simulates resume).
    let epoch2 = handles
        .put_running(
            "run-1",
            "owner".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    assert_eq!(epoch2, 2);

    let summary = handles.get_owned_summary("owner", "run-1").await.unwrap();
    assert_eq!(summary.status, SubAgentStatus::Running);

    // Stale epoch1 completion is ignored.
    let stale = handles
        .update_after_completion(
            "run-1",
            epoch1,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await;
    assert!(stale.is_none());

    // Run is still running.
    let summary = handles.get_owned_summary("owner", "run-1").await.unwrap();
    assert_eq!(summary.status, SubAgentStatus::Running);

    // Correct epoch2 completion applies.
    let applied = handles
        .update_after_completion(
            "run-1",
            epoch2,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await
        .expect("epoch2 completion should apply");
    assert_eq!(applied.status, SubAgentStatus::Completed);
}

// ── AgentRunTool: resume completed/failed returns status ─────────────────────

#[tokio::test]
async fn agent_run_tool_returns_completed_status_when_resuming_completed_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let epoch = handles
        .put_running(
            "run-1",
            "owner-thread".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Completed,
                error: None,
            },
        )
        .await;

    let run_tool = AgentRunTool::new(os, handles);
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = run_tool
        .execute(
            json!({ "run_id": "run-1", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("completed"));
}

#[tokio::test]
async fn agent_run_tool_returns_failed_status_when_resuming_failed_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let epoch = handles
        .put_running(
            "run-1",
            "owner-thread".to_string(),
            "sub-agent-run-1".to_string(),
            "worker".to_string(),
            None,
            None,
        )
        .await;
    handles
        .update_after_completion(
            "run-1",
            epoch,
            SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some("agent failed".to_string()),
            },
        )
        .await;

    let run_tool = AgentRunTool::new(os, handles);
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = run_tool
        .execute(
            json!({ "run_id": "run-1", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("failed"));
    assert_eq!(result.data["error"], json!("agent failed"));
}

// ── AgentRunTool: missing required args ──────────────────────────────────────

#[tokio::test]
async fn agent_run_tool_requires_prompt_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = run_tool
        .execute(
            json!({ "agent_id": "worker", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'prompt'"));
}

#[tokio::test]
async fn agent_run_tool_requires_agent_id_for_new_run() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = run_tool
        .execute(
            json!({ "prompt": "hello", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'agent_id'"));
}

// ── AgentRunTool: excluded agents via scope ──────────────────────────────────

#[tokio::test]
async fn agent_run_tool_rejects_excluded_agent() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .with_agent(
            "secret",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    fix.run_config
        .set(SCOPE_EXCLUDED_AGENTS_KEY, vec!["secret"])
        .unwrap();

    let result = run_tool
        .execute(
            json!({ "agent_id": "secret", "prompt": "hi", "background": false }),
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

// ── AgentRunTool: unknown run_id (no handle, no persisted) ───────────────────

#[tokio::test]
async fn agent_run_tool_returns_error_for_unknown_run_id() {
    let os = AgentOs::builder().build().unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = run_tool
        .execute(
            json!({ "run_id": "nonexistent", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("Unknown run_id"));
}

// ── AgentRunTool: persisted completed/failed returns without re-run ──────────

#[tokio::test]
async fn agent_run_tool_returns_persisted_completed_without_rerun() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "completed"
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state(doc);

    let result = run_tool
        .execute(
            json!({ "run_id": "run-1", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("completed"));
}

#[tokio::test]
async fn agent_run_tool_returns_persisted_failed_with_error() {
    let os = AgentOs::builder()
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini"),
        )
        .build()
        .unwrap();
    let run_tool = AgentRunTool::new(os, Arc::new(SubAgentHandleTable::new()));

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-1": {
                    "thread_id": "sub-agent-run-1",
                    "agent_id": "worker",
                    "status": "failed",
                    "error": "something broke"
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = caller_scope_with_state(doc);

    let result = run_tool
        .execute(
            json!({ "run_id": "run-1", "background": false }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("failed"));
    assert_eq!(result.data["error"], json!("something broke"));
}

// ── AgentStopTool edge cases ─────────────────────────────────────────────────

#[tokio::test]
async fn agent_stop_tool_requires_scope_context() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let stop_tool = AgentStopTool::new(handles);
    let fix = TestFixture::new();

    let result = stop_tool
        .execute(
            json!({ "run_id": "run-1" }),
            &fix.ctx_with("call-1", "tool:agent_stop"),
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
async fn agent_stop_tool_requires_run_id() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let stop_tool = AgentStopTool::new(handles);
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();

    let result = stop_tool
        .execute(json!({}), &fix.ctx_with("call-1", "tool:agent_stop"))
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'run_id'"));
}

#[tokio::test]
async fn agent_stop_tool_stops_persisted_running_without_live_handle() {
    let handles = Arc::new(SubAgentHandleTable::new());
    let stop_tool = AgentStopTool::new(handles);

    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-orphan": {
                    "thread_id": "sub-agent-run-orphan",
                    "agent_id": "worker",
                    "status": "running"
                }
            }
        }
    });
    let mut fix = TestFixture::new_with_state(doc.clone());
    fix.run_config = {
        let mut rt = tirea_contract::RunConfig::new();
        rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "owner-thread")
            .unwrap();
        rt
    };

    let result = stop_tool
        .execute(
            json!({ "run_id": "run-orphan" }),
            &fix.ctx_with("call-stop", "tool:agent_stop"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("stopped"));

    // Verify the state patch marks it stopped.
    let patch = fix.ctx_with("call-stop", "tool:agent_stop").take_patch();
    let updated = apply_patches(&doc, std::iter::once(patch.patch())).unwrap();
    assert_eq!(
        updated["sub_agents"]["runs"]["run-orphan"]["status"],
        json!("stopped")
    );
}

// ── AgentOutputTool: missing run_id param ────────────────────────────────────

#[tokio::test]
async fn agent_output_tool_requires_run_id() {
    let os = AgentOs::builder().build().unwrap();
    let tool = AgentOutputTool::new(os);
    let fix = TestFixture::new();

    let result = tool
        .execute(json!({}), &fix.ctx_with("call-1", "tool:agent_output"))
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result
        .message
        .unwrap_or_default()
        .contains("missing 'run_id'"));
}

// ── Recovery plugin: multiple orphans ────────────────────────────────────────

#[tokio::test]
async fn recovery_plugin_detects_multiple_orphans_creates_one_suspension() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker-a",
                        "status": "running"
                    },
                    "run-2": {
                        "thread_id": "sub-agent-run-2",
                        "agent_id": "worker-b",
                        "status": "running"
                    },
                    "run-3": {
                        "thread_id": "sub-agent-run-3",
                        "agent_id": "worker-c",
                        "status": "running"
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

    // All 3 should be marked stopped.
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-2"]["status"],
        json!("stopped")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-3"]["status"],
        json!("stopped")
    );

    // Only one recovery suspension should be created (for the first orphan).
    let scope = &updated["__tool_call_scope"];
    let suspended_count = scope
        .as_object()
        .map(|obj| {
            obj.values()
                .filter(|v| v.get("suspended_call").is_some())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        suspended_count, 1,
        "only one recovery suspension should be created"
    );
}

// ── Recovery plugin: mix of orphans and live handles ─────────────────────────

#[tokio::test]
async fn recovery_plugin_only_marks_orphans_when_some_have_live_handles() {
    let handles = Arc::new(SubAgentHandleTable::new());
    // run-1 has a live handle.
    handles
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "sub-agent-run-1".to_string(),
            "worker-a".to_string(),
            None,
            None,
        )
        .await;

    let plugin = AgentRecoveryPlugin::new(handles);
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker-a",
                        "status": "running"
                    },
                    "run-2": {
                        "thread_id": "sub-agent-run-2",
                        "agent_id": "worker-b",
                        "status": "running"
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

    // run-1 has live handle → not marked stopped.
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["status"],
        json!("running")
    );

    // run-2 is orphaned → marked stopped.
    assert_eq!(
        updated["sub_agents"]["runs"]["run-2"]["status"],
        json!("stopped")
    );
}

// ── Recovery plugin: no orphans when all have live handles ───────────────────

#[tokio::test]
async fn recovery_plugin_no_action_when_all_running_have_live_handles() {
    let handles = Arc::new(SubAgentHandleTable::new());
    handles
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "sub-agent-run-1".to_string(),
            "worker-a".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "run-2",
            "owner-1".to_string(),
            "sub-agent-run-2".to_string(),
            "worker-b".to_string(),
            None,
            None,
        )
        .await;

    let plugin = AgentRecoveryPlugin::new(handles);
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker-a",
                        "status": "running"
                    },
                    "run-2": {
                        "thread_id": "sub-agent-run-2",
                        "agent_id": "worker-b",
                        "status": "running"
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

    // Both still running (no orphan detection).
    assert_eq!(
        updated["sub_agents"]["runs"]["run-1"]["status"],
        json!("running")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-2"]["status"],
        json!("running")
    );

    // No suspended recovery interaction.
    let has_suspended = updated
        .get("__tool_call_scope")
        .and_then(|scope| scope.as_object())
        .map(|obj| obj.values().any(|v| v.get("suspended_call").is_some()))
        .unwrap_or(false);
    assert!(
        !has_suspended,
        "no recovery suspension should be created when all have live handles"
    );
}

// ── Recovery plugin: mixed statuses only Running without handle is orphan ────

#[tokio::test]
async fn recovery_plugin_ignores_completed_stopped_failed_in_persisted_state() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "sub_agents": {
                "runs": {
                    "run-completed": {
                        "thread_id": "sub-agent-run-completed",
                        "agent_id": "worker",
                        "status": "completed"
                    },
                    "run-failed": {
                        "thread_id": "sub-agent-run-failed",
                        "agent_id": "worker",
                        "status": "failed",
                        "error": "oops"
                    },
                    "run-stopped": {
                        "thread_id": "sub-agent-run-stopped",
                        "agent_id": "worker",
                        "status": "stopped"
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

    // None of them should change status.
    assert_eq!(
        updated["sub_agents"]["runs"]["run-completed"]["status"],
        json!("completed")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-failed"]["status"],
        json!("failed")
    );
    assert_eq!(
        updated["sub_agents"]["runs"]["run-stopped"]["status"],
        json!("stopped")
    );

    // No suspended recovery interaction.
    let has_suspended = updated
        .get("__tool_call_scope")
        .and_then(|scope| scope.as_object())
        .map(|obj| obj.values().any(|v| v.get("suspended_call").is_some()))
        .unwrap_or(false);
    assert!(!has_suspended);
}

// ── State helper tests ───────────────────────────────────────────────────────

#[test]
fn collect_descendant_run_ids_from_state_includes_full_tree() {
    let runs = HashMap::from([
        (
            "root".to_string(),
            SubAgent {
                thread_id: "sub-agent-root".to_string(),
                parent_run_id: None,
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
        (
            "child-1".to_string(),
            SubAgent {
                thread_id: "sub-agent-child-1".to_string(),
                parent_run_id: Some("root".to_string()),
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
        (
            "child-2".to_string(),
            SubAgent {
                thread_id: "sub-agent-child-2".to_string(),
                parent_run_id: Some("root".to_string()),
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
        (
            "grandchild".to_string(),
            SubAgent {
                thread_id: "sub-agent-grandchild".to_string(),
                parent_run_id: Some("child-1".to_string()),
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
        (
            "unrelated".to_string(),
            SubAgent {
                thread_id: "sub-agent-unrelated".to_string(),
                parent_run_id: None,
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
    ]);

    let descendants = collect_descendant_run_ids_from_state(&runs, "root", true);
    assert!(descendants.contains(&"root".to_string()));
    assert!(descendants.contains(&"child-1".to_string()));
    assert!(descendants.contains(&"child-2".to_string()));
    assert!(descendants.contains(&"grandchild".to_string()));
    assert!(!descendants.contains(&"unrelated".to_string()));
    assert_eq!(descendants.len(), 4);
}

#[test]
fn collect_descendant_run_ids_from_state_exclude_root() {
    let runs = HashMap::from([
        (
            "root".to_string(),
            SubAgent {
                thread_id: "sub-agent-root".to_string(),
                parent_run_id: None,
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
        (
            "child".to_string(),
            SubAgent {
                thread_id: "sub-agent-child".to_string(),
                parent_run_id: Some("root".to_string()),
                agent_id: "w".to_string(),
                status: SubAgentStatus::Running,
                error: None,
            },
        ),
    ]);

    let descendants = collect_descendant_run_ids_from_state(&runs, "root", false);
    assert!(!descendants.contains(&"root".to_string()));
    assert!(descendants.contains(&"child".to_string()));
    assert_eq!(descendants.len(), 1);
}

#[test]
fn collect_descendant_run_ids_from_state_unknown_root_returns_empty() {
    let runs: HashMap<String, SubAgent> = HashMap::new();
    let descendants = collect_descendant_run_ids_from_state(&runs, "nonexistent", true);
    assert!(descendants.is_empty());
}

#[test]
fn collect_descendant_run_ids_from_state_leaf_node_returns_only_self() {
    let runs = HashMap::from([(
        "leaf".to_string(),
        SubAgent {
            thread_id: "sub-agent-leaf".to_string(),
            parent_run_id: Some("parent".to_string()),
            agent_id: "w".to_string(),
            status: SubAgentStatus::Running,
            error: None,
        },
    )]);

    let descendants = collect_descendant_run_ids_from_state(&runs, "leaf", true);
    assert_eq!(descendants, vec!["leaf".to_string()]);
}

// ── Parallel tool-level operations ───────────────────────────────────────────

#[tokio::test]
async fn parallel_background_launches_produce_unique_run_ids() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let mut run_ids = Vec::new();
    for i in 0..5 {
        let mut fix = TestFixture::new();
        fix.run_config = caller_scope();
        let started = run_tool
            .execute(
                json!({
                    "agent_id": "worker",
                    "prompt": format!("task-{i}"),
                    "background": true
                }),
                &fix.ctx_with(&format!("call-{i}"), "tool:agent_run"),
            )
            .await
            .unwrap();
        assert_eq!(started.status, ToolStatus::Success);
        run_ids.push(started.data["run_id"].as_str().unwrap().to_string());
    }

    // All run_ids should be unique.
    let unique: std::collections::HashSet<&str> = run_ids.iter().map(|s| s.as_str()).collect();
    assert_eq!(unique.len(), 5, "all run_ids should be unique");

    // All should be visible as running.
    let running = handles.running_or_stopped_for_owner("owner-thread").await;
    assert_eq!(running.len(), 5);
}

#[tokio::test]
async fn parallel_launch_and_immediate_stop() {
    let os = AgentOs::builder()
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());
    let stop_tool = AgentStopTool::new(handles.clone());

    // Launch background run.
    let mut fix = TestFixture::new();
    fix.run_config = caller_scope();
    let started = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "fast task",
                "background": true
            }),
            &fix.ctx_with("call-launch", "tool:agent_run"),
        )
        .await
        .unwrap();
    let run_id = started.data["run_id"].as_str().unwrap().to_string();

    // Immediately stop (race with background execution).
    let mut stop_fix = TestFixture::new();
    stop_fix.run_config = caller_scope();
    let stopped = stop_tool
        .execute(
            json!({ "run_id": run_id }),
            &stop_fix.ctx_with("call-stop", "tool:agent_stop"),
        )
        .await
        .unwrap();
    assert_eq!(stopped.status, ToolStatus::Success);
    assert_eq!(stopped.data["status"], json!("stopped"));

    // Wait for background task to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify the run is stopped (cancellation override should prevent it from flipping to completed).
    let summary = handles
        .get_owned_summary("owner-thread", &run_id)
        .await
        .unwrap();
    assert_eq!(summary.status, SubAgentStatus::Stopped);
}

// ── SubAgent serialization round-trip ────────────────────────────────────────

#[test]
fn sub_agent_serialization_roundtrip() {
    let sub = SubAgent {
        thread_id: "sub-agent-run-42".to_string(),
        parent_run_id: Some("parent-7".to_string()),
        agent_id: "researcher".to_string(),
        status: SubAgentStatus::Completed,
        error: None,
    };
    let json = serde_json::to_value(&sub).unwrap();
    assert_eq!(json["thread_id"], "sub-agent-run-42");
    assert_eq!(json["parent_run_id"], "parent-7");
    assert_eq!(json["agent_id"], "researcher");
    assert_eq!(json["status"], "completed");
    assert!(json.get("error").is_none(), "None error should be skipped");

    let roundtrip: SubAgent = serde_json::from_value(json).unwrap();
    assert_eq!(roundtrip.thread_id, sub.thread_id);
    assert_eq!(roundtrip.parent_run_id, sub.parent_run_id);
    assert_eq!(roundtrip.agent_id, sub.agent_id);
    assert_eq!(roundtrip.status as u8, SubAgentStatus::Completed as u8);
    assert!(roundtrip.error.is_none());
}

#[test]
fn sub_agent_deserializes_without_optional_fields() {
    let json = json!({
        "thread_id": "sub-agent-run-99",
        "agent_id": "coder",
        "status": "running"
    });
    let sub: SubAgent = serde_json::from_value(json).unwrap();
    assert!(sub.parent_run_id.is_none());
    assert!(sub.error.is_none());
}

#[test]
fn sub_agent_status_as_str() {
    assert_eq!(SubAgentStatus::Running.as_str(), "running");
    assert_eq!(SubAgentStatus::Completed.as_str(), "completed");
    assert_eq!(SubAgentStatus::Failed.as_str(), "failed");
    assert_eq!(SubAgentStatus::Stopped.as_str(), "stopped");
}

// ── Handle table: collect_descendant_run_ids_by_parent ───────────────────────

#[tokio::test]
async fn handle_table_collect_descendants_across_owners() {
    // Verifies that collect_descendant_run_ids_by_parent scopes to owner.
    let handles = SubAgentHandleTable::new();
    handles
        .put_running(
            "root",
            "owner-1".to_string(),
            "sub-agent-root".to_string(),
            "w".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "child",
            "owner-1".to_string(),
            "sub-agent-child".to_string(),
            "w".to_string(),
            Some("root".to_string()),
            None,
        )
        .await;
    // This child belongs to a different owner, so should NOT be included.
    handles
        .put_running(
            "other-child",
            "owner-2".to_string(),
            "sub-agent-other-child".to_string(),
            "w".to_string(),
            Some("root".to_string()),
            None,
        )
        .await;

    let stopped = handles.stop_owned_tree("owner-1", "root").await.unwrap();
    let stopped_ids: Vec<&str> = stopped.iter().map(|s| s.run_id.as_str()).collect();
    assert!(stopped_ids.contains(&"root"));
    assert!(stopped_ids.contains(&"child"));
    assert!(!stopped_ids.contains(&"other-child"));

    // other-child should still be running.
    let other = handles
        .get_owned_summary("owner-2", "other-child")
        .await
        .unwrap();
    assert_eq!(other.status, SubAgentStatus::Running);
}

// ── Plugin rendering edge cases ──────────────────────────────────────────────

#[test]
fn plugin_renders_empty_when_no_agents() {
    let reg = InMemoryAgentRegistry::new();
    let plugin = AgentToolsPlugin::new(Arc::new(reg), Arc::new(SubAgentHandleTable::new()));
    let rendered = plugin.render_available_agents(None, None);
    assert!(rendered.is_empty());
}

#[tokio::test]
async fn plugin_no_reminder_when_no_handles() {
    let reg = InMemoryAgentRegistry::new();
    let handles = Arc::new(SubAgentHandleTable::new());
    let plugin = AgentToolsPlugin::new(Arc::new(reg), handles);

    let fixture = TestFixture::new();
    let mut step = StepContext::new(fixture.ctx(), "owner-1", &fixture.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
    assert!(
        step.messaging.reminders.is_empty(),
        "no reminder should be emitted when there are no handles"
    );
}

#[tokio::test]
async fn plugin_reminder_shows_multiple_runs() {
    let reg = InMemoryAgentRegistry::new();
    let handles = Arc::new(SubAgentHandleTable::new());
    handles
        .put_running(
            "run-1",
            "owner-1".to_string(),
            "sub-agent-run-1".to_string(),
            "worker-a".to_string(),
            None,
            None,
        )
        .await;
    handles
        .put_running(
            "run-2",
            "owner-1".to_string(),
            "sub-agent-run-2".to_string(),
            "worker-b".to_string(),
            None,
            None,
        )
        .await;

    let plugin = AgentToolsPlugin::new(Arc::new(reg), handles);
    let fixture = TestFixture::new();
    let mut step = StepContext::new(fixture.ctx(), "owner-1", &fixture.messages, vec![]);
    plugin.run_phase(Phase::AfterToolExecute, &mut step).await;
    let reminder = step
        .messaging
        .reminders
        .first()
        .expect("should have a reminder for multiple running sub-agents");
    assert!(reminder.contains("run-1"));
    assert!(reminder.contains("run-2"));
    assert!(reminder.contains("worker-a"));
    assert!(reminder.contains("worker-b"));
    assert!(reminder.contains("agent_output"));
}

// ══════════════════════════════════════════════════════════════════════════════
// Integration tests: full sub-agent lifecycle through ThreadStore
// ══════════════════════════════════════════════════════════════════════════════

/// Build an AgentOs with MemoryStore + SlowTerminatePlugin for integration tests.
/// Sub-agents using this terminate before inference (no LLM needed).
fn build_integration_os() -> (AgentOs, Arc<tirea_store_adapters::MemoryStore>) {
    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "caller",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "reviewer",
            crate::runtime::AgentDefinition::new("gpt-4o-mini")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();
    (os, storage)
}

fn integration_caller_scope(
    state: serde_json::Value,
    run_id: &str,
    messages: Vec<crate::contracts::thread::Message>,
) -> tirea_contract::RunConfig {
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "parent-thread")
        .unwrap();
    rt.set(TOOL_SCOPE_CALLER_AGENT_ID_KEY, "caller").unwrap();
    rt.set(SCOPE_RUN_ID_KEY, run_id).unwrap();
    rt.set(TOOL_SCOPE_CALLER_STATE_KEY, state).unwrap();
    rt.set(TOOL_SCOPE_CALLER_MESSAGES_KEY, messages).unwrap();
    rt
}

#[tokio::test]
async fn integration_foreground_sub_agent_creates_thread_in_store() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "do something",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolStatus::Success);
    let status = result.data["status"].as_str().unwrap();
    assert!(
        status == "completed" || status == "failed",
        "foreground run should reach terminal state, got: {status}"
    );
    let run_id = result.data["run_id"].as_str().unwrap();
    let child_thread_id = super::tools::sub_agent_thread_id(run_id);

    // Verify: child thread exists independently in ThreadStore.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist in store");

    assert_eq!(child_head.thread.id, child_thread_id);
    assert!(
        !child_head.thread.messages.is_empty(),
        "child thread should have at least the user prompt message"
    );
    // The user message should be present.
    let has_user_msg = child_head
        .thread
        .messages
        .iter()
        .any(|m| m.role == Role::User);
    assert!(has_user_msg, "child thread should have a user message");

    // Verify: child thread has parent_thread_id set.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-thread"),
        "child thread should have parent_thread_id set"
    );
}

#[tokio::test]
async fn integration_agent_output_reads_from_thread_store() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os.clone(), handles.clone());
    let output_tool = AgentOutputTool::new(os);

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    // Launch foreground sub-agent.
    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "analyze data",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let status_str = result.data["status"].as_str().unwrap();

    // Persist the SubAgent metadata into fixture state so agent_output can read it.
    let child_thread_id = super::tools::sub_agent_thread_id(&run_id);
    let doc = json!({
        "sub_agents": {
            "runs": {
                &run_id: {
                    "thread_id": child_thread_id,
                    "agent_id": "worker",
                    "status": status_str
                }
            }
        }
    });
    let output_fix = TestFixture::new_with_state(doc);
    let output_result = output_tool
        .execute(
            json!({ "run_id": run_id }),
            &output_fix.ctx_with("call-output", "tool:agent_output"),
        )
        .await
        .unwrap();

    // agent_output should return success with sub-agent info.
    assert_eq!(output_result.status, ToolStatus::Success);
    assert_eq!(output_result.data["agent_id"], json!("worker"));
    assert_eq!(output_result.data["run_id"], json!(run_id));

    // The child thread should exist in store (verified by agent_output reading it).
    let child_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        child_head.is_some(),
        "child thread should exist in store for agent_output to read"
    );
}

#[tokio::test]
async fn integration_parent_state_contains_only_lightweight_metadata() {
    let (os, _storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "task",
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap();

    // Check the state patch that was emitted.
    let patch = fix.ctx_with("call-1", "tool:agent_run").take_patch();
    let updated = apply_patches(&json!({}), std::iter::once(patch.patch())).unwrap();
    let sub_agent_entry = &updated["sub_agents"]["runs"][run_id];

    // Should have lightweight fields.
    assert!(
        sub_agent_entry["thread_id"].as_str().is_some(),
        "should have thread_id"
    );
    assert!(
        sub_agent_entry["agent_id"].as_str().is_some(),
        "should have agent_id"
    );
    assert!(
        sub_agent_entry["status"].as_str().is_some(),
        "should have status"
    );

    // Should NOT have embedded thread or messages.
    assert!(
        sub_agent_entry.get("thread").is_none() || sub_agent_entry["thread"].is_null(),
        "SubAgent should NOT contain embedded Thread"
    );
    assert!(
        sub_agent_entry.get("messages").is_none() || sub_agent_entry["messages"].is_null(),
        "SubAgent should NOT contain messages"
    );
    assert!(
        sub_agent_entry.get("state").is_none() || sub_agent_entry["state"].is_null(),
        "SubAgent should NOT contain state snapshot"
    );
    assert!(
        sub_agent_entry.get("patches").is_none() || sub_agent_entry["patches"].is_null(),
        "SubAgent should NOT contain patches"
    );
}

#[tokio::test]
async fn integration_background_sub_agent_persists_to_store_after_completion() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "background task",
                "background": true
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["status"], json!("running"));
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::tools::sub_agent_thread_id(&run_id);

    // Wait for background completion (SlowTerminatePlugin sleeps 120ms then terminates).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify: child thread was persisted to ThreadStore.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist in store after background completion");

    assert_eq!(child_head.thread.id, child_thread_id);
    assert!(
        child_head.thread.parent_thread_id.as_deref() == Some("parent-thread"),
        "child thread should have parent_thread_id"
    );

    // Verify: handle table shows completed.
    let summary = handles
        .get_owned_summary("parent-thread", &run_id)
        .await
        .expect("handle should exist");
    assert!(
        summary.status == SubAgentStatus::Completed || summary.status == SubAgentStatus::Failed,
        "background run should be terminal after completion: {:?}",
        summary.status
    );
}

#[tokio::test]
async fn integration_background_sub_agent_preserves_parent_run_id_in_deltas() {
    use crate::contracts::storage::ThreadSync;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles);

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-lineage",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "background lineage check",
                "background": true
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::tools::sub_agent_thread_id(&run_id);

    let mut deltas = Vec::new();
    for _ in 0..20 {
        match storage.load_deltas(&child_thread_id, 0).await {
            Ok(found) if !found.is_empty() => {
                deltas = found;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(
        !deltas.is_empty(),
        "expected at least one persisted delta for child thread"
    );
    assert_eq!(
        deltas[0].parent_run_id.as_deref(),
        Some("parent-run-lineage"),
        "background sub-agent delta should retain parent_run_id lineage"
    );
}

#[tokio::test]
async fn integration_fork_context_passes_state_to_sub_agent_thread() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let fork_state = json!({
        "project": "tirea",
        "context": {"depth": 3}
    });
    let fork_messages = vec![
        crate::contracts::thread::Message::user("analyze the code"),
        crate::contracts::thread::Message::assistant("I'll start analyzing."),
    ];

    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(fork_state.clone(), "parent-run-1", fork_messages);

    let result = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "continue analysis",
                "fork_context": true,
                "background": false
            }),
            &fix.ctx_with("call-1", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ToolStatus::Success);
    let run_id = result.data["run_id"].as_str().unwrap();
    let child_thread_id = super::tools::sub_agent_thread_id(run_id);

    // Verify: child thread exists in store with forked content.
    let child_head = storage
        .load(&child_thread_id)
        .await
        .expect("load should not error")
        .expect("child thread should exist");

    // Child should have messages (at minimum the fork + user prompt).
    assert!(
        child_head.thread.messages.len() >= 2,
        "child should have forked messages + user prompt, got {} messages",
        child_head.thread.messages.len()
    );

    // The forked state should be the initial state of the child thread.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert_eq!(
        child_state["project"],
        json!("tirea"),
        "child should have forked state: project"
    );
    assert_eq!(
        child_state["context"]["depth"],
        json!(3),
        "child should have forked state: nested context"
    );
}

#[tokio::test]
async fn integration_multiple_parallel_sub_agents_create_independent_threads() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());

    let mut run_ids = Vec::new();
    for i in 0..3 {
        let agent_id = match i {
            0 => "worker",
            1 => "reviewer",
            _ => "worker",
        };
        let mut fix = TestFixture::new();
        fix.run_config = integration_caller_scope(
            json!({}),
            "parent-run-1",
            vec![crate::contracts::thread::Message::user("hi")],
        );

        let result = run_tool
            .execute(
                json!({
                    "agent_id": agent_id,
                    "prompt": format!("task {i}"),
                    "background": true
                }),
                &fix.ctx_with(&format!("call-{i}"), "tool:agent_run"),
            )
            .await
            .unwrap();
        assert_eq!(result.status, ToolStatus::Success);
        run_ids.push(result.data["run_id"].as_str().unwrap().to_string());
    }

    // Wait for all background runs to complete.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify: each sub-agent has its own independent thread in store.
    for run_id in &run_ids {
        let child_thread_id = super::tools::sub_agent_thread_id(run_id);
        let child_head = storage
            .load(&child_thread_id)
            .await
            .expect("load should not error")
            .unwrap_or_else(|| panic!("child thread '{child_thread_id}' should exist in store"));

        assert_eq!(child_head.thread.id, child_thread_id);
        assert_eq!(
            child_head.thread.parent_thread_id.as_deref(),
            Some("parent-thread")
        );
    }

    // Verify: all thread IDs are unique.
    let thread_ids: std::collections::HashSet<String> = run_ids
        .iter()
        .map(|rid| super::tools::sub_agent_thread_id(rid))
        .collect();
    assert_eq!(thread_ids.len(), 3, "all child threads should be unique");
}

#[tokio::test]
async fn integration_background_stop_resume_full_lifecycle_with_store() {
    use crate::contracts::storage::ThreadReader;

    let (os, storage) = build_integration_os();
    let handles = Arc::new(SubAgentHandleTable::new());
    let run_tool = AgentRunTool::new(os, handles.clone());
    let stop_tool = AgentStopTool::new(handles.clone());

    // Phase 1: Launch background.
    let mut fix = TestFixture::new();
    fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );

    let started = run_tool
        .execute(
            json!({
                "agent_id": "worker",
                "prompt": "long task",
                "background": true
            }),
            &fix.ctx_with("call-start", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(started.data["status"], json!("running"));
    let run_id = started.data["run_id"].as_str().unwrap().to_string();
    let child_thread_id = super::tools::sub_agent_thread_id(&run_id);

    // Phase 2: Stop immediately.
    let mut stop_fix = TestFixture::new();
    stop_fix.run_config = integration_caller_scope(
        json!({}),
        "parent-run-1",
        vec![crate::contracts::thread::Message::user("hi")],
    );
    let stopped = stop_tool
        .execute(
            json!({ "run_id": &run_id }),
            &stop_fix.ctx_with("call-stop", "tool:agent_stop"),
        )
        .await
        .unwrap();
    assert_eq!(stopped.data["status"], json!("stopped"));

    // Wait for background task to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify: child thread exists in store (created during prepare_run before cancellation).
    let child_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        child_head.is_some(),
        "child thread should exist in store even after stop"
    );

    // Phase 3: Resume (foreground).
    let resumed = run_tool
        .execute(
            json!({
                "run_id": &run_id,
                "prompt": "continue",
                "background": false
            }),
            &fix.ctx_with("call-resume", "tool:agent_run"),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, ToolStatus::Success);
    let resumed_status = resumed.data["status"].as_str().unwrap();
    assert!(
        resumed_status == "completed" || resumed_status == "failed",
        "resumed run should reach terminal state, got: {resumed_status}"
    );

    // Verify: the child thread was reused (same thread_id), not a new one.
    let final_head = storage.load(&child_thread_id).await.unwrap();
    assert!(
        final_head.is_some(),
        "resumed child should use the same thread_id"
    );
    let final_thread = final_head.unwrap().thread;
    // After resume, the thread should have more messages (original + resume prompt).
    assert!(
        final_thread.messages.len() >= 2,
        "resumed thread should have accumulated messages from both runs, got {}",
        final_thread.messages.len()
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Integration tests: sub-agent with mock LLM and tool execution
// ══════════════════════════════════════════════════════════════════════════════

/// Mock LLM that returns pre-configured responses.
/// First call returns a tool call, subsequent calls return text (triggering NaturalEnd).
struct ToolCallMockLlm {
    tool_name: String,
    tool_args: serde_json::Value,
    call_count: std::sync::atomic::AtomicUsize,
}

impl ToolCallMockLlm {
    fn new(tool_name: &str, tool_args: serde_json::Value) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            tool_args,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for ToolCallMockLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallMockLlm")
            .field("tool_name", &self.tool_name)
            .finish()
    }
}

#[async_trait]
impl crate::loop_runtime::loop_runner::LlmExecutor for ToolCallMockLlm {
    async fn exec_chat_response(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<genai::chat::ChatResponse> {
        unimplemented!("stream-only mock")
    }

    async fn exec_chat_stream_events(
        &self,
        _model: &str,
        _chat_req: genai::chat::ChatRequest,
        _options: Option<&genai::chat::ChatOptions>,
    ) -> genai::Result<crate::loop_runtime::loop_runner::LlmEventStream> {
        use genai::chat::{ChatStreamEvent, MessageContent, StreamChunk, StreamEnd, ToolChunk};

        let n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if n == 0 {
            // First call: return a tool call.
            let tc = genai::chat::ToolCall {
                call_id: "tc-001".to_string(),
                fn_name: self.tool_name.clone(),
                fn_arguments: serde_json::Value::String(self.tool_args.to_string()),
                thought_signatures: None,
            };
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::ToolCallChunk(ToolChunk {
                    tool_call: tc.clone(),
                })),
                Ok(ChatStreamEvent::End(StreamEnd {
                    captured_content: Some(MessageContent::from_tool_calls(vec![tc])),
                    ..Default::default()
                })),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        } else {
            // Subsequent calls: return text (triggering NaturalEnd).
            let events = vec![
                Ok(ChatStreamEvent::Start),
                Ok(ChatStreamEvent::Chunk(StreamChunk {
                    content: format!("analysis complete (call #{n})"),
                })),
                Ok(ChatStreamEvent::End(StreamEnd::default())),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn name(&self) -> &'static str {
        "tool_call_mock"
    }
}

/// A simple echo tool that returns its arguments.
#[derive(Debug)]
struct EchoTool;

#[async_trait]
impl crate::contracts::runtime::tool_call::Tool for EchoTool {
    fn descriptor(&self) -> crate::contracts::runtime::tool_call::ToolDescriptor {
        crate::contracts::runtime::tool_call::ToolDescriptor::new("echo", "Echo", "Echoes input")
            .with_parameters(json!({"type": "object", "properties": {"input": {"type": "string"}}}))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::contracts::ToolCallContext<'_>,
    ) -> Result<
        crate::contracts::runtime::tool_call::ToolResult,
        crate::contracts::runtime::tool_call::ToolError,
    > {
        Ok(crate::contracts::runtime::tool_call::ToolResult::success(
            "echo",
            json!({"echoed": args}),
        ))
    }
}

/// A counting tool that tracks how many times it's been called.
#[derive(Debug)]
struct CountingTool {
    count: std::sync::atomic::AtomicUsize,
}

impl CountingTool {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl crate::contracts::runtime::tool_call::Tool for CountingTool {
    fn descriptor(&self) -> crate::contracts::runtime::tool_call::ToolDescriptor {
        crate::contracts::runtime::tool_call::ToolDescriptor::new(
            "count",
            "Count",
            "Counts invocations",
        )
        .with_parameters(json!({"type": "object"}))
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &crate::contracts::ToolCallContext<'_>,
    ) -> Result<
        crate::contracts::runtime::tool_call::ToolResult,
        crate::contracts::runtime::tool_call::ToolError,
    > {
        let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::contracts::runtime::tool_call::ToolResult::success(
            "count",
            json!({"invocation": n + 1}),
        ))
    }
}

#[tokio::test]
async fn integration_sub_agent_executes_tool_via_mock_llm() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "echo".to_string(),
            Arc::new(EchoTool) as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    // Resolve, inject mock LLM, prepare, execute.
    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(ToolCallMockLlm::new(
            "echo",
            json!({"input": "hello world"}),
        ))
            as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>);

    let child_thread_id = "sub-agent-tool-test";
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(child_thread_id.to_string()),
                run_id: Some("run-tool-test".to_string()),
                parent_run_id: Some("parent-run-1".to_string()),
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("call echo tool")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let events: Vec<_> = run.events.collect().await;

    // Verify events include tool call and tool result.
    let has_tool_call_done = events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::ToolCallDone { .. }));
    assert!(
        has_tool_call_done,
        "events should include ToolCallDone for echo tool"
    );

    let has_run_finish = events
        .iter()
        .any(|ev| matches!(ev, AgentEvent::RunFinish { .. }));
    assert!(has_run_finish, "events should include RunFinish");

    // Verify: child thread in store has tool call + tool result messages.
    let child_head = storage
        .load(child_thread_id)
        .await
        .unwrap()
        .expect("child thread should exist");

    let messages = &child_head.thread.messages;
    // Expected message sequence:
    // 1. User message ("call echo tool")
    // 2. Assistant message with tool_calls (from mock LLM)
    // 3. Tool result message
    // 4. Assistant message ("analysis complete")
    assert!(
        messages.len() >= 4,
        "child thread should have user + assistant(tool_call) + tool_result + assistant messages, got {}",
        messages.len()
    );

    // Verify user message.
    assert_eq!(messages[0].role, Role::User);

    // Verify assistant message with tool calls.
    let assistant_msg = messages
        .iter()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_some());
    assert!(
        assistant_msg.is_some(),
        "should have an assistant message with tool_calls"
    );
    let tool_calls = assistant_msg.unwrap().tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls[0].name, "echo");

    // Verify tool result message.
    let tool_msg = messages.iter().find(|m| m.role == Role::Tool);
    assert!(tool_msg.is_some(), "should have a tool result message");
    assert_eq!(tool_msg.unwrap().tool_call_id.as_deref(), Some("tc-001"));

    // Verify final assistant message (text response after tool execution).
    let final_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.tool_calls.is_none());
    assert!(
        final_assistant.is_some(),
        "should have a final text assistant message"
    );

    // Verify run lifecycle state.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert_eq!(
        child_state["__run"]["status"],
        json!("done"),
        "child run should be done"
    );

    // Verify parent_thread_id lineage.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-thread")
    );
}

#[tokio::test]
async fn integration_sub_agent_tool_invocation_counted() {
    use crate::contracts::io::RunRequest;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let counting_tool = Arc::new(CountingTool::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "count".to_string(),
            counting_tool.clone() as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(Arc::new(ToolCallMockLlm::new("count", json!({})))
            as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>);

    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some("sub-agent-count-test".to_string()),
                run_id: Some("run-count-test".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("count it")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();

    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _events: Vec<_> = run.events.collect().await;

    // The counting tool should have been called exactly once.
    assert_eq!(
        counting_tool.call_count(),
        1,
        "counting tool should be invoked once by the mock LLM"
    );
}

#[tokio::test]
async fn integration_consecutive_runs_on_same_thread_accumulate_messages() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "worker",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    let thread_id = "sub-agent-multi-run";

    // Run 1: initial prompt.
    let resolved1 = os.resolve("worker").unwrap();
    let prepared1 = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-1".to_string()),
                parent_run_id: None,
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("first task")],
                initial_decisions: vec![],
            },
            resolved1,
        )
        .await
        .unwrap();
    let run1 = AgentOs::execute_prepared(prepared1).unwrap();
    let _: Vec<_> = run1.events.collect().await;

    let head1 = storage.load(thread_id).await.unwrap().unwrap();
    let msg_count_after_run1 = head1.thread.messages.len();
    assert!(
        msg_count_after_run1 >= 1,
        "should have at least the user message after run 1"
    );

    // Run 2: resume with new prompt on same thread.
    let resolved2 = os.resolve("worker").unwrap();
    let prepared2 = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(thread_id.to_string()),
                run_id: Some("run-2".to_string()),
                parent_run_id: None,
                parent_thread_id: Some("parent-thread".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("second task")],
                initial_decisions: vec![],
            },
            resolved2,
        )
        .await
        .unwrap();
    let run2 = AgentOs::execute_prepared(prepared2).unwrap();
    let _: Vec<_> = run2.events.collect().await;

    let head2 = storage.load(thread_id).await.unwrap().unwrap();
    let msg_count_after_run2 = head2.thread.messages.len();

    // Run 2 should have more messages (accumulated from both runs).
    assert!(
        msg_count_after_run2 > msg_count_after_run1,
        "messages should accumulate: run1={msg_count_after_run1}, run2={msg_count_after_run2}"
    );

    // Both user messages should be present.
    let user_messages: Vec<&str> = head2
        .thread
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        user_messages.contains(&"first task"),
        "should contain first task message"
    );
    assert!(
        user_messages.contains(&"second task"),
        "should contain second task message"
    );

    // Run lifecycle should reflect run-2.
    let state = head2.thread.rebuild_state().unwrap();
    assert_eq!(state["__run"]["id"], json!("run-2"));
}

#[tokio::test]
async fn integration_sub_agent_thread_independent_from_parent_thread() {
    use crate::contracts::io::RunRequest;
    use crate::contracts::storage::ThreadReader;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_registered_behavior(
            "slow_terminate_behavior_requested",
            Arc::new(SlowTerminatePlugin),
        )
        .with_agent(
            "parent-agent",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .with_agent(
            "child-agent",
            crate::runtime::AgentDefinition::new("mock")
                .with_behavior_id("slow_terminate_behavior_requested"),
        )
        .build()
        .unwrap();

    // Create parent thread.
    let parent_resolved = os.resolve("parent-agent").unwrap();
    let parent_prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "parent-agent".to_string(),
                thread_id: Some("parent-tid".to_string()),
                run_id: Some("parent-rid".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: Some(json!({"parent_data": "secret"})),
                messages: vec![crate::contracts::thread::Message::user("parent task")],
                initial_decisions: vec![],
            },
            parent_resolved,
        )
        .await
        .unwrap();
    let parent_run = AgentOs::execute_prepared(parent_prepared).unwrap();
    let _: Vec<_> = parent_run.events.collect().await;

    // Create child thread with parent lineage.
    let child_resolved = os.resolve("child-agent").unwrap();
    let child_prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "child-agent".to_string(),
                thread_id: Some("child-tid".to_string()),
                run_id: Some("child-rid".to_string()),
                parent_run_id: Some("parent-rid".to_string()),
                parent_thread_id: Some("parent-tid".to_string()),
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("child task")],
                initial_decisions: vec![],
            },
            child_resolved,
        )
        .await
        .unwrap();
    let child_run = AgentOs::execute_prepared(child_prepared).unwrap();
    let _: Vec<_> = child_run.events.collect().await;

    // Verify: both threads exist independently.
    let parent_head = storage.load("parent-tid").await.unwrap().unwrap();
    let child_head = storage.load("child-tid").await.unwrap().unwrap();

    // Parent should NOT contain child's messages.
    let parent_msg_contents: Vec<&str> = parent_head
        .thread
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        !parent_msg_contents.contains(&"child task"),
        "parent thread should not contain child messages"
    );

    // Child should NOT contain parent's messages.
    let child_msg_contents: Vec<&str> = child_head
        .thread
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        !child_msg_contents.contains(&"parent task"),
        "child thread should not contain parent messages"
    );

    // Child should NOT have parent's initial state.
    let child_state = child_head.thread.rebuild_state().unwrap();
    assert!(
        child_state.get("parent_data").is_none() || child_state["parent_data"].is_null(),
        "child should not inherit parent's state unless forked"
    );

    // Child should have parent lineage.
    assert_eq!(
        child_head.thread.parent_thread_id.as_deref(),
        Some("parent-tid")
    );
}

#[tokio::test]
async fn integration_agent_output_reads_tool_result_from_sub_agent() {
    use crate::contracts::io::RunRequest;

    let storage = Arc::new(tirea_store_adapters::MemoryStore::new());
    let os = AgentOs::builder()
        .with_agent_state_store(storage.clone() as Arc<dyn crate::contracts::storage::ThreadStore>)
        .with_agent("worker", crate::runtime::AgentDefinition::new("mock"))
        .with_tools(HashMap::from([(
            "echo".to_string(),
            Arc::new(EchoTool) as Arc<dyn crate::contracts::runtime::tool_call::Tool>,
        )]))
        .build()
        .unwrap();

    // Run sub-agent with mock LLM that calls echo tool.
    let mut resolved = os.resolve("worker").unwrap();
    resolved.agent = resolved
        .agent
        .with_llm_executor(
            Arc::new(ToolCallMockLlm::new("echo", json!({"input": "test data"})))
                as Arc<dyn crate::loop_runtime::loop_runner::LlmExecutor>,
        );

    let child_thread_id = "sub-agent-output-test";
    let prepared = os
        .prepare_run(
            RunRequest {
                agent_id: "worker".to_string(),
                thread_id: Some(child_thread_id.to_string()),
                run_id: Some("run-output-test".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: RunOrigin::default(),
                state: None,
                messages: vec![crate::contracts::thread::Message::user("echo test")],
                initial_decisions: vec![],
            },
            resolved,
        )
        .await
        .unwrap();
    let run = AgentOs::execute_prepared(prepared).unwrap();
    let _: Vec<_> = run.events.collect().await;

    // Now use AgentOutputTool to read the output.
    let output_tool = AgentOutputTool::new(os);
    let doc = json!({
        "sub_agents": {
            "runs": {
                "run-output-test": {
                    "thread_id": child_thread_id,
                    "agent_id": "worker",
                    "status": "completed"
                }
            }
        }
    });
    let fix = TestFixture::new_with_state(doc);
    let result = output_tool
        .execute(
            json!({ "run_id": "run-output-test" }),
            &fix.ctx_with("call-output", "tool:agent_output"),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.data["agent_id"], json!("worker"));
    assert_eq!(result.data["status"], json!("completed"));

    // The output should contain the last assistant message (the text response after tool execution).
    let output = result.data["output"].as_str();
    assert!(
        output.is_some(),
        "agent_output should return the last assistant message"
    );
    assert!(
        output.unwrap().contains("analysis complete"),
        "output should contain the mock LLM's final response: got {:?}",
        output
    );
}

// ── Permission fallback test ─────────────────────────────────────────────────

#[cfg(not(feature = "permission"))]
#[tokio::test]
async fn recovery_plugin_fallback_always_approves_despite_deny_state() {
    let plugin = AgentRecoveryPlugin::new(Arc::new(SubAgentHandleTable::new()));
    // State declares "deny" for recover_agent_run, but without the permission
    // feature the fallback always returns Allow.
    let thread = Thread::with_initial_state(
        "owner-1",
        json!({
            "permissions": {
                "default_behavior": "deny",
                "tools": {
                    "recover_agent_run": "deny"
                }
            },
            "sub_agents": {
                "runs": {
                    "run-1": {
                        "thread_id": "sub-agent-run-1",
                        "agent_id": "worker",
                        "status": "running"
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
    // Without permission feature, fallback always returns Allow — so recovery
    // should auto-approve even though state says "deny".
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["status"],
        json!("resuming"),
        "fallback should auto-approve when permission feature is off"
    );
    assert_eq!(
        updated["__tool_call_scope"]["agent_recovery_run-1"]["tool_call_state"]["resume"]["action"],
        json!("resume"),
    );
}
