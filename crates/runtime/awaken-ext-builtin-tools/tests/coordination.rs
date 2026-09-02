//! Asynchronous Agent coordination tools over a fake orchestration port.

use std::sync::{Arc, Mutex};

use awaken_ext_builtin_tools::{
    AgentCoordinator, AgentListRequest, AgentMessageReceipt, AgentMessageRequest,
    AgentMessageTarget, AgentRosterEntry, LIST_AGENTS, SEND_MESSAGE, Toolset, coordination_tools,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolError, ToolOperationContext, ToolRecoveryCapability, ToolRecoveryPolicy,
    with_tool_operation_context,
};
use awaken_runtime_contract::{RunId, ThreadId};

#[derive(Default)]
struct RecordingCoordinator {
    listed: Mutex<Vec<AgentListRequest>>,
    sent: Mutex<Vec<AgentMessageRequest>>,
}

#[async_trait::async_trait]
impl AgentCoordinator for RecordingCoordinator {
    async fn list_agents(
        &self,
        request: AgentListRequest,
    ) -> Result<Vec<AgentRosterEntry>, ToolError> {
        self.listed.lock().unwrap().push(request);
        Ok(vec![AgentRosterEntry {
            agent_id: "researcher".into(),
            name: "Researcher".into(),
            description: Some("Finds evidence".into()),
        }])
    }

    async fn send_message(
        &self,
        request: AgentMessageRequest,
    ) -> Result<AgentMessageReceipt, ToolError> {
        let thread = match &request.target {
            AgentMessageTarget::Spawn { agent_id } => format!("sthr-{agent_id}"),
            AgentMessageTarget::ExistingThread { session_thread_id } => session_thread_id.clone(),
        };
        self.sent.lock().unwrap().push(request);
        Ok(AgentMessageReceipt {
            session_thread_id: thread,
            accepted: true,
        })
    }
}

fn call(tool_id: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "call-1".into(),
        tool_id: tool_id.into(),
        arguments,
    }
}

fn find(tools: &[Arc<dyn RawTool>], id: &str) -> Arc<dyn RawTool> {
    tools
        .iter()
        .find(|tool| tool.id() == id)
        .expect("coordination tool")
        .clone()
}

async fn invoke(
    tool: Arc<dyn RawTool>,
    arguments: serde_json::Value,
) -> Result<awaken_runtime_contract::tool::ToolOutput, ToolError> {
    let id = tool.id().to_string();
    with_tool_operation_context(
        ToolOperationContext {
            run_id: Some(RunId("parent-run".into())),
            thread_id: Some(ThreadId("parent-thread".into())),
            operation_id: "operation-7".into(),
            call_id: Some("call-1".into()),
            execution_scope: None,
        },
        tool.invoke(call(&id, arguments)),
    )
    .await
}

#[tokio::test]
async fn list_agents_uses_the_runtime_owned_source_run() {
    // Cause/effect graph: C1=runtime-owned Run context is present; C2=the
    // coordinator port returns one frozen roster entry. R1 C1+C2 => E1=the port
    // receives the Run id and E2=the exact entry is rendered to the model.
    // Constraint/invariant: source Run and Thread come only from trusted Runtime
    // context; model arguments cannot supply or override either coordinate.
    // Decision rule R1 is the sole valid list partition: trusted context plus a
    // successful roster read yields the exact request and rendered response.
    let service = Arc::new(RecordingCoordinator::default());
    let tools = coordination_tools(service.clone());
    let output = invoke(find(&tools, LIST_AGENTS), serde_json::json!({}))
        .await
        .expect("list roster");
    assert_eq!(
        service.listed.lock().unwrap().as_slice(),
        &[AgentListRequest {
            source_run_id: "parent-run".into(),
            source_thread_id: "parent-thread".into(),
        }],
        "R1/E1"
    );
    let rendered: Vec<AgentRosterEntry> = serde_json::from_str(&output.text()).unwrap();
    assert_eq!(rendered[0].agent_id, "researcher", "R1/E2");
}

#[tokio::test]
async fn send_message_selects_spawn_or_follow_up_without_a_second_route() {
    // Decision table:
    // | Rule | agent_id | thread_id | Effect |
    // | R1   | value    | absent    | Spawn |
    // | R2   | absent   | value     | ExistingThread |
    // Both rules carry the same runtime operation identity to the one port.
    // Causes: exactly one target selector plus a nonblank message. Effects:
    // the sole coordinator port receives Spawn or ExistingThread and returns its
    // receipt. Constraint/invariant: no adapter-local routing registry exists.
    let service = Arc::new(RecordingCoordinator::default());
    let tools = coordination_tools(service.clone());
    let send = find(&tools, SEND_MESSAGE);
    let spawned = invoke(
        send.clone(),
        serde_json::json!({"agent_id":"researcher","message":"investigate"}),
    )
    .await
    .expect("spawn");
    let followed = invoke(
        send,
        serde_json::json!({"session_thread_id":"sthr-9","message":"check again"}),
    )
    .await
    .expect("follow up");
    assert!(spawned.text().contains("sthr-researcher"), "R1");
    assert!(followed.text().contains("sthr-9"), "R2");
    let sent = service.sent.lock().unwrap();
    assert!(
        matches!(sent[0].target, AgentMessageTarget::Spawn { .. }),
        "R1"
    );
    assert!(
        matches!(sent[1].target, AgentMessageTarget::ExistingThread { .. }),
        "R2"
    );
    assert!(sent.iter().all(|request| {
        request.source_run_id == "parent-run"
            && request.source_thread_id == "parent-thread"
            && request.source_call_id == "call-1"
            && request.operation_id == "operation-7"
    }));
}

#[tokio::test]
async fn send_message_rejects_ambiguous_empty_and_unknown_inputs_before_effects() {
    // Decision table:
    // | Rule | agent_id | thread_id | message | Effect |
    // | R1   | absent   | absent    | text    | reject |
    // | R2   | value    | value     | text    | reject |
    // | R3   | value    | absent    | blank   | reject |
    // | R4   | value    | absent    | text + unknown field | reject |
    // Causes are the four invalid partitions above; Effects: every partition
    // returns InvalidArguments and records zero sends. Constraint/invariant:
    // validation completes before the coordinator effect boundary.
    let service = Arc::new(RecordingCoordinator::default());
    let send = find(&coordination_tools(service.clone()), SEND_MESSAGE);
    let cases = [
        serde_json::json!({"message":"x"}),
        serde_json::json!({"agent_id":"a","session_thread_id":"t","message":"x"}),
        serde_json::json!({"agent_id":"a","message":"  "}),
        serde_json::json!({"agent_id":"a","message":"x","other":true}),
    ];
    for (index, arguments) in cases.into_iter().enumerate() {
        let error = invoke(send.clone(), arguments)
            .await
            .expect_err("invalid request must fail closed");
        assert!(
            matches!(error, ToolError::InvalidArguments(_)),
            "R{}",
            index + 1
        );
    }
    assert!(service.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn coordination_effects_require_runtime_context() {
    // Cause C1=no trusted Run/operation context. Decision rule R1 => both tools fail before their
    // service port, so direct adapter invocation cannot invent effect identity.
    // Effects: both calls return Execution and neither coordinator method runs.
    // Constraint/invariant: Runtime context is the sole effect identity source;
    // this one-rule negative partition covers both coordination entry points.
    let service = Arc::new(RecordingCoordinator::default());
    let tools = coordination_tools(service.clone());
    for (id, arguments) in [
        (LIST_AGENTS, serde_json::json!({})),
        (
            SEND_MESSAGE,
            serde_json::json!({"agent_id":"a","message":"x"}),
        ),
    ] {
        let error = find(&tools, id)
            .invoke(call(id, arguments))
            .await
            .expect_err("missing runtime identity");
        assert!(matches!(error, ToolError::Execution(_)), "R1/{id}");
    }
    assert!(service.listed.lock().unwrap().is_empty());
    assert!(service.sent.lock().unwrap().is_empty());
}

#[test]
fn coordination_catalog_and_executables_are_exact_and_recoverable() {
    // Causes: C1=the canonical catalog selects Coordination; C2=the executable factory
    // is built from the same package. R1 => exact two-id equality. C3=send has
    // an idempotent external effect. R2 => descriptor and executable both claim
    // DurableRequest while the read-only list tool does not.
    // Effects: R1 yields exact catalog/factory id equality; R2 yields matching
    // recovery classes. Constraint/invariant: descriptor and executable expose
    // one canonical two-tool catalog, never a parallel recovery declaration.
    // Decision rules R1=C1+C2=>equal id sets; R2=C3=>DurableRequest for send
    // and NonRecoverable for the read-only list operation.
    let descriptors = awaken_ext_builtin_tools::builtin_tools()
        .into_iter()
        .filter(|tool| tool.toolset() == Toolset::Coordination)
        .map(|tool| {
            let descriptor = tool.into_descriptor();
            (descriptor.id, descriptor.recovery_policy)
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let tools = coordination_tools(Arc::new(RecordingCoordinator::default()));
    let executable_ids = tools
        .iter()
        .map(|tool| tool.id().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(descriptors.len(), 2, "R1 exact two-tool catalog");
    assert_eq!(
        descriptors
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        executable_ids,
        "R1"
    );
    assert_eq!(
        descriptors[SEND_MESSAGE],
        ToolRecoveryPolicy::durable_request(),
        "R2/descriptor"
    );
    assert_eq!(
        find(&tools, SEND_MESSAGE).recovery_capability(),
        ToolRecoveryCapability::DurableRequest,
        "R2/executable"
    );
    assert_eq!(
        find(&tools, LIST_AGENTS).recovery_capability(),
        ToolRecoveryCapability::NonRecoverable,
        "R2/read-only"
    );
}
