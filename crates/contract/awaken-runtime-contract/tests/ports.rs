//! The tool and permission ports are dyn-safe, async, and their data values are
//! plain serializable types.

use std::sync::Arc;

use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::permission::{
    GateOutcome, PermissionContext, PermissionDecision, PermissionPolicy, ToolGateHook,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolExecutor, ToolOutput};

struct EchoTool;

#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, call.arguments.to_string()))
    }
}

struct AllowAll;

#[async_trait::async_trait]
impl PermissionPolicy for AllowAll {
    async fn decide(&self, _ctx: &PermissionContext) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// A gate that consults a policy and maps the decision to an outcome — the
/// canonical relationship between the two ports.
struct PolicyGate(Arc<dyn PermissionPolicy>);

#[async_trait::async_trait]
impl ToolGateHook for PolicyGate {
    async fn gate(&self, ctx: &PermissionContext) -> GateOutcome {
        match self.0.decide(ctx).await {
            PermissionDecision::Allow => GateOutcome::Allow,
            PermissionDecision::Deny { reason } => GateOutcome::Block { reason },
            PermissionDecision::Ask { ticket_id } => GateOutcome::Suspend { ticket_id },
        }
    }
}

fn call() -> ToolCall {
    ToolCall {
        call_id: "c1".to_string(),
        tool_id: "echo".to_string(),
        arguments: serde_json::json!({"x": 1}),
    }
}

#[tokio::test]
async fn raw_tool_is_dyn_dispatchable() {
    let tool: Arc<dyn RawTool> = Arc::new(EchoTool);
    let out = tool.invoke(call()).await.expect("invoke");
    assert!(!out.is_error);
    assert_eq!(out.call_id, "c1");
}

#[tokio::test]
async fn policy_backed_gate_allows() {
    let gate = PolicyGate(Arc::new(AllowAll));
    let ctx = PermissionContext {
        tool_id: "echo".to_string(),
        call_id: "c1".to_string(),
        arguments: serde_json::json!({}),
    };
    assert_eq!(gate.gate(&ctx).await, GateOutcome::Allow);
}

#[test]
fn permission_decision_round_trips() {
    let decision = PermissionDecision::Deny {
        reason: "nope".to_string(),
    };
    let json = serde_json::to_string(&decision).expect("serialize");
    let back: PermissionDecision = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decision, back);
}

/// A trivial registry-backed executor proves the loop-facing port is usable.
struct OneToolExecutor(EchoTool);

#[async_trait::async_trait]
impl ToolExecutor for OneToolExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if call.tool_id == self.0.id() {
            self.0.invoke(call.clone()).await
        } else {
            Err(ToolError::Unknown(call.tool_id.clone()))
        }
    }
}

#[tokio::test]
async fn tool_executor_runs_known_tool_and_rejects_unknown() {
    let exec = OneToolExecutor(EchoTool);
    assert!(exec.invoke(&call()).await.is_ok());
    let mut unknown = call();
    unknown.tool_id = "nope".to_string();
    assert!(matches!(
        exec.invoke(&unknown).await,
        Err(ToolError::Unknown(_))
    ));
}
