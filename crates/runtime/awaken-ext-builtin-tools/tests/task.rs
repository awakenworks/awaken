//! Task tools over fake service ports — the tool surface in isolation from any
//! runtime backing.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_ext_builtin_tools::{
    MessageRecovery, MessageSendRequest, MessageSender, TaskCanceller, task_tools,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolError, ToolOperationContext, ToolRecoveryCapability,
    with_tool_operation_context,
};

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".to_string(),
        tool_id: id.to_string(),
        arguments: args,
    }
}

#[derive(Default)]
struct RecordingSender {
    sent: Mutex<Vec<String>>,
    fail: bool,
}
#[async_trait::async_trait]
impl MessageSender for RecordingSender {
    async fn send(&self, request: MessageSendRequest) -> Result<(), ToolError> {
        if self.fail {
            return Err(ToolError::Execution("ingress down".to_string()));
        }
        self.sent.lock().unwrap().push(format!(
            "{}:{}:{:?}:{}:{}",
            request.target_thread,
            request.content,
            request.idempotency_key,
            request.source_run_id,
            request.operation_id
        ));
        Ok(())
    }
}

#[derive(Default)]
struct RecordingCanceller {
    cancelled: Mutex<Vec<String>>,
    fail: bool,
}
#[async_trait::async_trait]
impl TaskCanceller for RecordingCanceller {
    async fn cancel(&self, task_id: &str) -> Result<(), ToolError> {
        if self.fail {
            return Err(ToolError::Execution("unknown task".to_string()));
        }
        self.cancelled.lock().unwrap().push(task_id.to_string());
        Ok(())
    }
}

struct FixedRecovery(&'static str);
#[async_trait::async_trait]
impl MessageRecovery for FixedRecovery {
    async fn recover(&self) -> Result<String, ToolError> {
        Ok(self.0.to_string())
    }
}

fn task_set(
    sender: Arc<RecordingSender>,
    canceller: Arc<RecordingCanceller>,
    recovery: Arc<FixedRecovery>,
) -> Vec<Arc<dyn RawTool>> {
    task_tools(sender, canceller, recovery)
}

fn find(tools: &[Arc<dyn RawTool>], id: &str) -> Arc<dyn RawTool> {
    tools.iter().find(|t| t.id() == id).expect("tool").clone()
}

async fn invoke_in_run(
    tool: Arc<dyn RawTool>,
    call: ToolCall,
) -> Result<awaken_runtime_contract::tool::ToolOutput, ToolError> {
    with_tool_operation_context(
        ToolOperationContext::for_run("run-1", "op-1"),
        tool.invoke(call),
    )
    .await
}

#[tokio::test]
async fn send_message_forwards_to_the_sender() {
    let sender = Arc::new(RecordingSender::default());
    let tools = task_set(
        sender.clone(),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    let out = invoke_in_run(
        find(&tools, "send_message"),
        call(
            "send_message",
            serde_json::json!({
                "target_thread": "thread-9",
                "content": "hi team",
                "idempotency_key": "logical-message"
            }),
        ),
    )
    .await
    .expect("send");
    assert_eq!(out.text(), "message sent to thread-9");
    assert_eq!(
        sender.sent.lock().unwrap().as_slice(),
        &["thread-9:hi team:Some(\"logical-message\"):run-1:op-1"]
    );
}

#[tokio::test]
async fn send_message_idempotency_key_is_optional_and_blank_means_absent() {
    // CE-SM1..SM3: omitted key and blank key both delegate retry identity to
    // the runtime-owned operation; a non-blank key remains caller-controlled.
    let sender = Arc::new(RecordingSender::default());
    let tools = task_set(
        sender.clone(),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    for arguments in [
        serde_json::json!({ "target_thread": "thread-9", "content": "omitted" }),
        serde_json::json!({
            "target_thread": "thread-9",
            "content": "blank",
            "idempotency_key": "  "
        }),
        serde_json::json!({
            "target_thread": "thread-9",
            "content": "padded",
            "idempotency_key": " caller-key "
        }),
    ] {
        invoke_in_run(
            find(&tools, "send_message"),
            call("send_message", arguments),
        )
        .await
        .expect("optional idempotency key");
    }
    assert_eq!(
        sender.sent.lock().unwrap().as_slice(),
        &[
            "thread-9:omitted:None:run-1:op-1",
            "thread-9:blank:None:run-1:op-1",
            "thread-9:padded:Some(\"caller-key\"):run-1:op-1",
        ]
    );
}

#[tokio::test]
async fn send_message_propagates_a_service_error() {
    let sender = Arc::new(RecordingSender {
        fail: true,
        ..Default::default()
    });
    let tools = task_set(
        sender,
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    let err = invoke_in_run(
        find(&tools, "send_message"),
        call(
            "send_message",
            serde_json::json!({ "target_thread": "thread-9", "content": "x" }),
        ),
    )
    .await
    .expect_err("ingress down");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn send_message_requires_runtime_owned_operation_context() {
    // CE-SM9: no runtime context -> no stable sender identity -> fail closed;
    // the service is not called and no process-local fallback id is invented.
    let sender = Arc::new(RecordingSender::default());
    let tools = task_set(
        sender.clone(),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    let err = find(&tools, "send_message")
        .invoke(call(
            "send_message",
            serde_json::json!({ "target_thread": "thread-9", "content": "x" }),
        ))
        .await
        .expect_err("missing durable identity must fail closed");
    assert!(matches!(err, ToolError::Execution(_)));
    assert!(sender.sent.lock().unwrap().is_empty());
}

#[test]
fn send_message_declares_durable_request_recovery() {
    // CE-SM10: descriptor policy and executable capability must both advertise
    // DurableRequest; either default strands a request after a process crash.
    let tools = task_set(
        Arc::new(RecordingSender::default()),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    assert_eq!(
        find(&tools, "send_message").recovery_capability(),
        ToolRecoveryCapability::DurableRequest
    );
    let descriptor = awaken_ext_builtin_tools::builtin_tools()
        .into_iter()
        .find(|tool| tool.descriptor().id == "send_message")
        .expect("send_message descriptor");
    assert_eq!(
        descriptor.descriptor().recovery_policy,
        awaken_runtime_contract::tool::ToolRecoveryPolicy::durable_request()
    );
}

#[tokio::test]
async fn cancel_task_forwards_the_task_id() {
    let canceller = Arc::new(RecordingCanceller::default());
    let tools = task_set(
        Arc::new(RecordingSender::default()),
        canceller.clone(),
        Arc::new(FixedRecovery("")),
    );
    let out = find(&tools, "cancel_task")
        .invoke(call(
            "cancel_task",
            serde_json::json!({ "task_id": "run-9" }),
        ))
        .await
        .expect("cancel");
    assert_eq!(out.text(), "cancelled run-9");
    assert_eq!(canceller.cancelled.lock().unwrap().as_slice(), &["run-9"]);
}

#[tokio::test]
async fn cancel_task_propagates_a_service_error() {
    let canceller = Arc::new(RecordingCanceller {
        fail: true,
        ..Default::default()
    });
    let tools = task_set(
        Arc::new(RecordingSender::default()),
        canceller,
        Arc::new(FixedRecovery("")),
    );
    let err = find(&tools, "cancel_task")
        .invoke(call(
            "cancel_task",
            serde_json::json!({ "task_id": "nope" }),
        ))
        .await
        .expect_err("unknown task");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn recover_failed_messages_takes_no_args_and_returns_a_summary() {
    let tools = task_set(
        Arc::new(RecordingSender::default()),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("recovered 2 messages")),
    );
    // A null argument payload is accepted (no-arg tool).
    let out = find(&tools, "recover_failed_messages")
        .invoke(call("recover_failed_messages", serde_json::Value::Null))
        .await
        .expect("recover");
    assert_eq!(out.text(), "recovered 2 messages");
}
