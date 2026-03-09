use super::*;

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
    reg.upsert("reviewer", crate::runtime::AgentDefinition::new("mock"));
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
