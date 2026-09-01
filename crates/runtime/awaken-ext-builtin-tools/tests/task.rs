//! Task tools over fake service ports — the tool surface in isolation from any
//! runtime backing.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_ext_builtin_tools::{TaskCanceller, task_tools};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError};

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".to_string(),
        tool_id: id.to_string(),
        arguments: args,
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

fn task_set(canceller: Arc<RecordingCanceller>) -> Vec<Arc<dyn RawTool>> {
    task_tools(canceller)
}

fn find(tools: &[Arc<dyn RawTool>], id: &str) -> Arc<dyn RawTool> {
    tools.iter().find(|t| t.id() == id).expect("tool").clone()
}

#[tokio::test]
async fn cancel_task_forwards_the_task_id() {
    let canceller = Arc::new(RecordingCanceller::default());
    let tools = task_set(canceller.clone());
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
    let tools = task_set(canceller);
    let err = find(&tools, "cancel_task")
        .invoke(call(
            "cancel_task",
            serde_json::json!({ "task_id": "nope" }),
        ))
        .await
        .expect_err("unknown task");
    assert!(matches!(err, ToolError::Execution(_)));
}
