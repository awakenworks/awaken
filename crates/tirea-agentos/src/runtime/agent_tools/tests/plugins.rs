use super::*;

#[test]
fn plugin_filters_out_caller_agent() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("a", crate::runtime::AgentDefinition::new("mock"));
    reg.upsert("b", crate::runtime::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg));
    let rendered = plugin.render_available_agents(Some("a"), None);
    assert!(rendered.contains("<id>b</id>"));
    assert!(!rendered.contains("<id>a</id>"));
}

#[test]
fn plugin_filters_agents_by_scope_policy() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("writer", crate::runtime::AgentDefinition::new("mock"));
    reg.upsert("reviewer", crate::runtime::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg));
    let mut rt = tirea_contract::RunPolicy::new();
    rt.set_allowed_agents_if_absent(Some(&["writer".to_string()]));
    let rendered = plugin.render_available_agents(None, Some(&rt));
    assert!(rendered.contains("<id>writer</id>"));
    assert!(!rendered.contains("<id>reviewer</id>"));
}

#[test]
fn plugin_renders_task_output_tool_usage() {
    let mut reg = InMemoryAgentRegistry::new();
    reg.upsert("worker", crate::runtime::AgentDefinition::new("mock"));
    let plugin = AgentToolsPlugin::new(Arc::new(reg));
    let rendered = plugin.render_available_agents(None, None);
    assert!(
        rendered.contains("task_output"),
        "available agents should mention task_output tool"
    );
}

#[test]
fn plugin_renders_empty_when_no_agents() {
    let reg = InMemoryAgentRegistry::new();
    let plugin = AgentToolsPlugin::new(Arc::new(reg));
    let rendered = plugin.render_available_agents(None, None);
    assert!(rendered.is_empty());
}
