#![allow(missing_docs)]

use serde_json::json;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tirea_agent_loop::engine::tool_execution::execute_single_tool_with_run_policy_and_behavior;
use tirea_agentos::contracts::runtime::tool_call::ToolDescriptor;
use tirea_agentos::contracts::thread::{Thread as ConversationAgentState, ToolCall};
use tirea_agentos::contracts::AgentEvent;
use tirea_agentos::extensions::permission::PermissionPlugin;
use tirea_agentos::extensions::skills::{
    FsSkill, InMemorySkillRegistry, SkillRegistry, SkillSubsystem,
};
use tirea_agentos::runtime::compose_behaviors;
use tirea_contract::testing::TestFixture;
use tirea_protocol_ag_ui::{AgUiEventContext, Event};

fn make_agui_ctx(thread_id: &str, run_id: &str) -> AgUiEventContext {
    let mut ctx = AgUiEventContext::new();
    ctx.on_agent_event(&AgentEvent::RunStart {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        parent_run_id: None,
    });
    ctx
}

#[tokio::test]
async fn test_skill_tool_result_is_emitted_as_agui_tool_call_result() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("docx")).unwrap();
    fs::write(
        root.join("docx").join("SKILL.md"),
        "---\nname: docx\ndescription: docx\n---\nUse docx-js.\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(
        FsSkill::into_arc_skills(result.skills),
    ));
    let skills = SkillSubsystem::new(registry);
    let tools = skills.tools();
    let tool = tools.get("skill").expect("skill tool registered");

    let thread = ConversationAgentState::with_initial_state("s", json!({}));
    let state = thread.rebuild_state().unwrap();
    let call = ToolCall::new("call_1", "skill", json!({"skill": "docx"}));

    let behavior = compose_behaviors("skills_test_router", vec![Arc::new(PermissionPlugin)]);
    let exec = execute_single_tool_with_run_policy_and_behavior(
        Some(tool.as_ref()),
        &call,
        &state,
        None,
        Some(behavior.as_ref()),
    )
    .await;
    assert!(exec.result.is_success());

    // Simulate tool call lifecycle events being converted to AG-UI.
    let mut agui_ctx = make_agui_ctx("thread_1", "run_1");
    let start = AgentEvent::ToolCallStart {
        id: "call_1".to_string(),
        name: "skill".to_string(),
    };
    let ready = AgentEvent::ToolCallReady {
        id: "call_1".to_string(),
        name: "skill".to_string(),
        arguments: json!({"skill": "docx"}),
    };
    let delta = AgentEvent::ToolCallDelta {
        id: "call_1".to_string(),
        args_delta: "{\"skill\":\"docx\"}".to_string(),
    };
    let done = AgentEvent::ToolCallDone {
        id: "call_1".to_string(),
        result: exec.result.clone(),
        patch: exec.patch.clone(),
        message_id: String::new(),
    };

    let mut events = Vec::new();
    events.extend(agui_ctx.on_agent_event(&start));
    events.extend(agui_ctx.on_agent_event(&delta));
    events.extend(agui_ctx.on_agent_event(&ready));
    events.extend(agui_ctx.on_agent_event(&done));

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallStart { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallArgs { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::ToolCallEnd { .. })));

    let args_event = events
        .iter()
        .find_map(|e| {
            if let Event::ToolCallArgs {
                tool_call_id,
                delta,
                ..
            } = e
            {
                Some((tool_call_id.clone(), delta.clone()))
            } else {
                None
            }
        })
        .expect("ToolCallArgs emitted");
    assert_eq!(args_event.0, "call_1");
    assert!(args_event.1.contains("\"skill\""));
    assert!(args_event.1.contains("\"docx\""));

    let result_event = events
        .iter()
        .find_map(|e| {
            if let Event::ToolCallResult {
                tool_call_id,
                content,
                ..
            } = e
            {
                Some((tool_call_id.clone(), content.clone()))
            } else {
                None
            }
        })
        .expect("ToolCallResult emitted");

    assert_eq!(result_event.0, "call_1");
    let parsed: serde_json::Value = serde_json::from_str(&result_event.1).expect("json content");
    assert_eq!(parsed["tool_name"], "skill");
    assert_eq!(parsed["status"], "success");
    assert_eq!(parsed["data"]["activated"], true);
    assert_eq!(parsed["data"]["skill_id"], "docx");
}

#[tokio::test]
async fn test_skills_plugin_injection_is_in_system_context_before_inference() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("docx")).unwrap();
    fs::write(
        root.join("docx").join("SKILL.md"),
        "---\nname: docx\ndescription: docx\n---\nBody\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(
        FsSkill::into_arc_skills(result.skills),
    ));
    let skills = SkillSubsystem::new(registry);
    let plugin: Arc<dyn tirea_agentos::contracts::runtime::AgentBehavior> =
        Arc::new(skills.discovery_plugin());

    // Even without activation, discovery should inject available_skills.
    let fixture = TestFixture::new();
    let mut step = fixture.step(vec![ToolDescriptor::new("t", "t", "t")]);
    let ctx = tirea_agentos::contracts::runtime::behavior::ReadOnlyContext::new(
        tirea_agentos::contracts::runtime::phase::Phase::BeforeInference,
        step.thread_id(),
        step.messages(),
        step.run_policy(),
        step.ctx().doc(),
    );
    use tirea_agentos::contracts::runtime::phase::BeforeInferenceAction;
    let actions = plugin.before_inference(&ctx).await;
    for action in actions {
        if let BeforeInferenceAction::AddSystemContext(s) = action {
            step.inference.system_context.push(s);
        }
    }
    assert_eq!(step.inference.system_context.len(), 1);
    assert!(step.inference.system_context[0].contains("<available_skills>"));
}
