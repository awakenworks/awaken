//! Task and delegation tools over fake service ports — the tool surface in
//! isolation from any runtime backing.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_ext_builtin_tools::{
    AgentRunner, MessageRecovery, MessageSender, TaskCanceller, delegation_tools, task_tools,
};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError};

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
    async fn send(&self, target_thread: &str, content: &str) -> Result<(), ToolError> {
        if self.fail {
            return Err(ToolError::Execution("ingress down".to_string()));
        }
        self.sent
            .lock()
            .unwrap()
            .push(format!("{target_thread}:{content}"));
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

struct EchoRunner;
#[async_trait::async_trait]
impl AgentRunner for EchoRunner {
    async fn run(&self, agent_id: &str, input: &str) -> Result<String, ToolError> {
        if agent_id == "unknown" {
            return Err(ToolError::Execution("agent not in roster".to_string()));
        }
        Ok(format!("{agent_id} handled: {input}"))
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

#[tokio::test]
async fn send_message_forwards_to_the_sender() {
    let sender = Arc::new(RecordingSender::default());
    let tools = task_set(
        sender.clone(),
        Arc::new(RecordingCanceller::default()),
        Arc::new(FixedRecovery("")),
    );
    let out = find(&tools, "send_message")
        .invoke(call(
            "send_message",
            serde_json::json!({ "target_thread": "thread-9", "content": "hi team" }),
        ))
        .await
        .expect("send");
    assert_eq!(out.content, "message sent to thread-9");
    assert_eq!(
        sender.sent.lock().unwrap().as_slice(),
        &["thread-9:hi team"]
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
    let err = find(&tools, "send_message")
        .invoke(call(
            "send_message",
            serde_json::json!({ "target_thread": "thread-9", "content": "x" }),
        ))
        .await
        .expect_err("ingress down");
    assert!(matches!(err, ToolError::Execution(_)));
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
    assert_eq!(out.content, "cancelled run-9");
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
    assert_eq!(out.content, "recovered 2 messages");
}

#[tokio::test]
async fn agent_run_returns_the_delegate_result() {
    let tools = delegation_tools(Arc::new(EchoRunner));
    let out = find(&tools, "agent_run")
        .invoke(call(
            "agent_run",
            serde_json::json!({ "agent_id": "researcher", "input": "find X" }),
        ))
        .await
        .expect("agent_run");
    assert_eq!(out.content, "researcher handled: find X");
}

#[tokio::test]
async fn agent_run_fails_closed_on_unknown_agent() {
    let tools = delegation_tools(Arc::new(EchoRunner));
    let err = find(&tools, "agent_run")
        .invoke(call(
            "agent_run",
            serde_json::json!({ "agent_id": "unknown", "input": "x" }),
        ))
        .await
        .expect_err("not in roster");
    assert!(matches!(err, ToolError::Execution(_)));
}
