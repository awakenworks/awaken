use futures::StreamExt;
use std::sync::Arc;
use tirea_agentos::contracts::{AgentEvent, RunRequest};
use tirea_agentos::orchestrator::AgentDefinition;
use tirea_agentos::orchestrator::{AgentOs, AgentOsBuilder};
use tirea_agentos_server::protocol::ag_ui::apply_agui_extensions;
use tirea_protocol_ag_ui::{Message, RunAgentInput};
use tirea_protocol_ai_sdk_v6::AiSdkV6RunRequest;
use tirea_store_adapters::MemoryStore;

mod common;

use common::TerminatePlugin;

fn make_os() -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_parity".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_parity",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_parity")),
        )
        .with_agent("test", def)
        .with_agent_state_store(Arc::new(MemoryStore::new()))
        .build()
        .unwrap()
}

fn collect_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::RunStart { .. } => "RunStart",
            AgentEvent::RunFinish { .. } => "RunFinish",
            AgentEvent::TextDelta { .. } => "TextDelta",
            AgentEvent::ToolCallStart { .. } => "ToolCallStart",
            AgentEvent::ToolCallDone { .. } => "ToolCallDone",
            AgentEvent::ToolCallReady { .. } => "ToolCallReady",
            AgentEvent::ToolCallDelta { .. } => "ToolCallDelta",
            AgentEvent::ToolCallResumed { .. } => "ToolCallResumed",
            AgentEvent::Error { .. } => "Error",
            AgentEvent::StateDelta { .. } => "StateDelta",
            AgentEvent::StateSnapshot { .. } => "StateSnapshot",
            AgentEvent::MessagesSnapshot { .. } => "MessagesSnapshot",
            AgentEvent::ActivityDelta { .. } => "ActivityDelta",
            AgentEvent::ActivitySnapshot { .. } => "ActivitySnapshot",
            AgentEvent::ReasoningDelta { .. } => "ReasoningDelta",
            AgentEvent::ReasoningEncryptedValue { .. } => "ReasoningEncryptedValue",
            AgentEvent::StepStart { .. } => "StepStart",
            AgentEvent::StepEnd => "StepEnd",
            AgentEvent::InferenceComplete { .. } => "InferenceComplete",
        })
        .collect()
}

fn normalize(run: RunRequest) -> RunRequest {
    RunRequest {
        parent_run_id: None,
        resource_id: None,
        state: None,
        ..run
    }
}

#[test]
fn agui_and_ai_sdk_inputs_map_to_equivalent_run_requests() {
    let agent_id = "test".to_string();
    let agui = RunAgentInput::new("thread_parity", "run_parity")
        .with_message(Message::user("hello parity"));
    let aisdk = AiSdkV6RunRequest::from_thread_input(
        "thread_parity",
        "hello parity",
        Some("run_parity".to_string()),
    );

    let agui_run = normalize(agui.into_runtime_run_request(agent_id.clone()));
    let aisdk_run = normalize(aisdk.into_runtime_run_request(agent_id));

    assert_eq!(agui_run.agent_id, aisdk_run.agent_id);
    assert_eq!(agui_run.thread_id, aisdk_run.thread_id);
    assert_eq!(agui_run.run_id, aisdk_run.run_id);
    assert_eq!(agui_run.messages.len(), 1);
    assert_eq!(aisdk_run.messages.len(), 1);
    assert_eq!(agui_run.messages[0].role, aisdk_run.messages[0].role);
    assert_eq!(agui_run.messages[0].content, aisdk_run.messages[0].content);
}

#[tokio::test]
async fn agui_and_ai_sdk_have_equivalent_runtime_event_shape() {
    let os = make_os();

    let agui_req = RunAgentInput::new("thread_parity_stream", "run_parity_stream")
        .with_message(Message::user("hello parity"));
    let mut agui_resolved = os.resolve("test").unwrap();
    apply_agui_extensions(&mut agui_resolved, &agui_req);
    let agui_run_req = agui_req.into_runtime_run_request("test".to_string());
    let agui_prepared = os.prepare_run(agui_run_req, agui_resolved).await.unwrap();
    let agui_run = AgentOs::execute_prepared(agui_prepared).unwrap();
    let agui_events: Vec<AgentEvent> = agui_run.events.collect().await;

    let aisdk_run_req = AiSdkV6RunRequest::from_thread_input(
        "thread_parity_stream",
        "hello parity",
        Some("run_parity_stream".to_string()),
    )
    .into_runtime_run_request("test".to_string());
    let aisdk_run = os.run_stream(aisdk_run_req).await.unwrap();
    let aisdk_events: Vec<AgentEvent> = aisdk_run.events.collect().await;

    assert_eq!(
        collect_kinds(&agui_events),
        collect_kinds(&aisdk_events),
        "AG-UI and AI-SDK should produce equivalent runtime event shape for the same semantic input"
    );
}
