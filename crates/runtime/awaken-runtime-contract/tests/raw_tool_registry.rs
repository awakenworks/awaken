use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, RawToolRegistry, ToolCall, ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability,
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
        Err(ToolError::Unknown(id)) if id == "missing"
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
    assert!(matches!(error, ToolError::Execution(message) if message.contains("ambiguous")));
    assert_eq!(
        duplicate.recovery_capability("same"),
        ToolRecoveryCapability::NonRecoverable
    );
}
