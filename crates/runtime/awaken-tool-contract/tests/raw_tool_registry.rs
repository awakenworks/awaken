use std::sync::Arc;

use async_trait::async_trait;
use awaken_tool_contract::tool::{
    DurableTaskTool, RawTool, RawToolRegistry, ToolCall, ToolError, ToolExecutor, ToolOutput,
    ToolRecoveryCapability, ToolTaskContinuation, ToolTaskHandle, ToolTaskPoll, ToolTaskStart,
    start_executor_task, start_tool_task,
};

struct RegistryTool {
    id: &'static str,
    capability: ToolRecoveryCapability,
}

#[async_trait]
impl RawTool for RegistryTool {
    fn id(&self) -> &str {
        self.id
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.capability
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, self.id))
    }
}

fn call(tool_id: &str) -> ToolCall {
    ToolCall {
        call_id: "call-1".into(),
        tool_id: tool_id.into(),
        arguments: serde_json::json!({}),
    }
}

#[tokio::test]
async fn registry_resolution_decision_table_is_fail_closed() {
    // Cause/effect graph: C1 id is registered, C2 id is duplicated, C3 the
    // implementation advertises recovery. Constraint: C2 implies C1.
    // Decision table: R1 !C1 -> Unknown + NonRecoverable; R2 C1,!C2,C3 ->
    // execute exactly that tool + propagate capability; R3 C1,C2 -> no
    // implementation selected + typed ambiguity + NonRecoverable.
    let unique = RawToolRegistry::new([Arc::new(RegistryTool {
        id: "unique",
        capability: ToolRecoveryCapability::Idempotent,
    }) as Arc<dyn RawTool>]);
    let output = unique.invoke(&call("unique")).await.expect("unique tool");
    assert_eq!(output.text(), "unique");
    assert_eq!(
        unique.recovery_capability("unique"),
        ToolRecoveryCapability::Idempotent
    );

    assert!(matches!(
        unique.invoke(&call("missing")).await,
        Err(ToolError::UnknownTool(id)) if id == "missing"
    ));
    assert_eq!(
        unique.recovery_capability("missing"),
        ToolRecoveryCapability::NonRecoverable
    );

    let duplicate = RawToolRegistry::new([
        Arc::new(RegistryTool {
            id: "same",
            capability: ToolRecoveryCapability::ReplaySafe,
        }) as Arc<dyn RawTool>,
        Arc::new(RegistryTool {
            id: "same",
            capability: ToolRecoveryCapability::Idempotent,
        }) as Arc<dyn RawTool>,
    ]);
    let error = duplicate
        .invoke(&call("same"))
        .await
        .expect_err("duplicate ids are ambiguous");
    // The display is the model-visible text and the text a remote Hand relays.
    assert_eq!(
        error.to_string(),
        "tool execution failed: tool id `same` is ambiguous"
    );
    assert!(matches!(error, ToolError::AmbiguousTool(id) if id == "same"));
    assert_eq!(
        duplicate.recovery_capability("same"),
        ToolRecoveryCapability::NonRecoverable
    );
}

struct DurableTool;

fn handle() -> ToolTaskHandle {
    ToolTaskHandle {
        owner: "fixture".into(),
        binding: "remote".into(),
        task_id: "task-1".into(),
        poll_interval_ms: Some(5),
    }
}

#[async_trait]
impl RawTool for DurableTool {
    fn id(&self) -> &str {
        "durable"
    }

    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        panic!("a durable-task start never falls back to foreground invoke")
    }

    fn durable_tasks(&self) -> Option<&dyn DurableTaskTool> {
        Some(self)
    }
}

#[async_trait]
impl DurableTaskTool for DurableTool {
    async fn start_task(&self, _call: ToolCall) -> Result<ToolTaskStart, ToolError> {
        Ok(ToolTaskStart::Pending(handle()))
    }

    async fn poll_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> Result<ToolTaskPoll, ToolError> {
        assert_eq!(task, &handle());
        Ok(ToolTaskPoll::Completed(ToolOutput::ok(
            &call.call_id,
            "done",
        )))
    }

    async fn cancel_task(
        &self,
        _call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> Result<ToolTaskPoll, ToolError> {
        assert_eq!(task, &handle());
        Ok(ToolTaskPoll::Cancelled)
    }
}

/// An executor that routes no durable-task protocol, like a remote Hand.
struct ForegroundExecutor(RawToolRegistry);

#[async_trait]
impl ToolExecutor for ForegroundExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        self.0.invoke(call).await
    }
}

fn refusal(result: Result<ToolTaskPoll, ToolError>) -> String {
    match result {
        Err(ToolError::DefinitiveFailure(message)) => message,
        other => panic!("expected a definitive refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn durable_task_capability_ladder_decision_table() {
    // Cause/effect graph: C1 the tool advertises DurableTaskTool; C2 the
    // executor advertises DurableTaskExecutor; C3 the requested action is
    // start (versus poll/cancel of an existing handle).
    // Decision table:
    // R1 C1,C2,C3 -> the tool's own start (Pending handle), never invoke;
    // R2 C1,C2,!C3 -> poll/cancel reach that same tool;
    // R3 !C1,C3 -> start completes through invoke;
    // R4 !C1,C2,!C3 -> the router refuses naming the tool;
    // R5 !C2,C3 -> start completes through invoke of the executor;
    // R6 !C2,!C3 -> refused naming the executor, before any tool is reached.
    // Constraint: a Pending handle is produced only under C1, so a refused
    // continuation never strands a created request.
    let registry = RawToolRegistry::new([
        Arc::new(DurableTool) as Arc<dyn RawTool>,
        Arc::new(RegistryTool {
            id: "plain",
            capability: ToolRecoveryCapability::NonRecoverable,
        }) as Arc<dyn RawTool>,
    ]);
    let durable = registry
        .durable_tasks()
        .expect("a registry routes durable tasks");

    assert_eq!(
        durable.start_task(&call("durable")).await.unwrap(),
        ToolTaskStart::Pending(handle()),
        "R1"
    );
    assert!(
        matches!(
            ToolTaskContinuation::Poll
                .continue_through(&registry, &call("durable"), &handle())
                .await,
            Ok(ToolTaskPoll::Completed(output)) if output.text() == "done"
        ),
        "R2 poll"
    );
    assert_eq!(
        durable
            .cancel_task(&call("durable"), &handle())
            .await
            .unwrap(),
        ToolTaskPoll::Cancelled,
        "R2 cancel"
    );

    assert_eq!(
        start_executor_task(&registry, &call("plain"))
            .await
            .unwrap(),
        ToolTaskStart::Completed(ToolOutput::ok("call-1", "plain")),
        "R3"
    );
    assert!(
        start_tool_task(registry.get("plain").unwrap().as_ref(), call("plain"))
            .await
            .is_ok_and(|start| matches!(start, ToolTaskStart::Completed(_))),
        "R3 direct"
    );
    assert_eq!(
        refusal(durable.poll_task(&call("plain"), &handle()).await),
        "tool does not support durable task polling",
        "R4 poll"
    );
    assert_eq!(
        refusal(durable.cancel_task(&call("plain"), &handle()).await),
        "tool does not support durable task cancellation",
        "R4 cancel"
    );
    assert!(
        matches!(
            durable.poll_task(&call("missing"), &handle()).await,
            Err(ToolError::UnknownTool(id)) if id == "missing"
        ),
        "R4 resolution precedes capability"
    );

    let foreground = ForegroundExecutor(registry);
    assert!(foreground.durable_tasks().is_none());
    assert_eq!(
        start_executor_task(&foreground, &call("plain"))
            .await
            .unwrap(),
        ToolTaskStart::Completed(ToolOutput::ok("call-1", "plain")),
        "R5"
    );
    assert_eq!(
        refusal(
            ToolTaskContinuation::Poll
                .continue_through(&foreground, &call("durable"), &handle())
                .await
        ),
        "tool executor does not support durable task polling",
        "R6 poll"
    );
    assert_eq!(
        refusal(
            ToolTaskContinuation::Cancel
                .continue_through(&foreground, &call("durable"), &handle())
                .await
        ),
        "tool executor does not support durable task cancellation",
        "R6 cancel"
    );
}
