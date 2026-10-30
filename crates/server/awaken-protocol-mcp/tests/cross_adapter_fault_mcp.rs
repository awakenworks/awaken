//! T33-C03 (cross-adapter consistency), MCP row: the shared injected upstream
//! fault (see `awaken-runtime/tests/cross_adapter_fault_local.rs`) raised by an
//! exported tool and projected through the production `McpToolService`
//! `tools/call` JSON-RPC path.
//!
//! | fault | tool error | must surface as |
//! |---|---|---|
//! | F1 | `UnavailableBeforeDispatch` (certain: nothing ran; retry is safe) | a JSON-RPC error, never a result |
//! | F2 | `Indeterminate` (effect outcome unknown; retry is unsafe) | a JSON-RPC error, never a result |
//! | F3 | `DefinitiveFailure` (certain: it ran and failed) | a JSON-RPC error, never a result |
//!
//! Oracles: O0 authority, uncertainty and retry hint are preserved; O1 the
//! fault never becomes a successful (or `isError` tool-result) answer. O1 and
//! the operator message hold today; the reproducer records that F1 and F2
//! reach the wire with the same code, so a client cannot tell a safe retry
//! from an unknown effect.
//! Negative control (executed): the same O1 checker rejects a silent-success
//! mutant of the real wire answer.

use std::sync::Arc;

use awaken_mcp_server_core::NullSink;
use awaken_protocol_mcp::{McpExportedTool, McpToolService, StaticExports};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::{Value, json};

const UPSTREAM_DETAIL: &str = "upstream 529 overloaded_error (retry-after 7s)";

#[derive(Clone, Copy, Debug)]
enum Fault {
    CertainNotDispatched,
    UnknownOutcome,
    CertainFailed,
}

struct FaultingTool(Fault);

#[async_trait::async_trait]
impl RawTool for FaultingTool {
    fn id(&self) -> &str {
        "upstream"
    }

    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        let detail = UPSTREAM_DETAIL.to_string();
        Err(match self.0 {
            Fault::CertainNotDispatched => ToolError::UnavailableBeforeDispatch(detail),
            Fault::UnknownOutcome => ToolError::Indeterminate(detail),
            Fault::CertainFailed => ToolError::DefinitiveFailure(detail),
        })
    }
}

/// One `tools/call` through the production service; `Ok` is the wire result
/// object, `Err` the JSON-RPC error `(code, message)`.
async fn call(fault: Fault) -> Result<Value, (i64, String)> {
    let source = StaticExports::new(vec![McpExportedTool::plain(
        ToolDescriptor::pinned(
            "t33",
            "upstream",
            "the faulting upstream",
            json!({"type": "object"}),
        ),
        Arc::new(FaultingTool(fault)),
    )]);
    let service = McpToolService::new("t33-c03", "0.0.0", Arc::new(source));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        service.handle(
            "tools/call",
            json!({ "name": "upstream", "arguments": {} }),
            Arc::new(NullSink),
        ),
    )
    .await
    .expect("the MCP call settles within its deadline")
    .map_err(|error| (error.code, error.message))
}

/// O1: a fault must never come back as a result (success or `isError`).
fn success_violation(row: &str, answer: &Result<Value, (i64, String)>) -> Option<String> {
    answer
        .as_ref()
        .ok()
        .map(|result| format!("O1 {row}: the fault surfaced as a tool result: {result}"))
}

#[tokio::test]
async fn mcp_projects_every_injected_fault_as_a_protocol_error_with_its_cause() {
    for fault in [
        Fault::CertainNotDispatched,
        Fault::UnknownOutcome,
        Fault::CertainFailed,
    ] {
        let answer = call(fault).await;
        assert_eq!(success_violation(&format!("{fault:?}"), &answer), None);
        let (code, message) = answer.expect_err("a protocol error");
        assert!(code < 0, "{fault:?}: a JSON-RPC error code, got {code}");
        assert!(
            message.contains(UPSTREAM_DETAIL),
            "{fault:?}: the provider's account and hint survive: {message}"
        );
    }
}

#[tokio::test]
async fn mcp_success_checker_rejects_a_silent_success_mutant_of_the_real_answer() {
    let real = call(Fault::UnknownOutcome).await;
    assert_eq!(
        success_violation("F2", &real),
        None,
        "the real answer passes"
    );
    let mutant: Result<Value, (i64, String)> =
        Ok(json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false }));
    assert!(
        success_violation("F2 mutant", &mutant).is_some_and(|v| v.starts_with("O1")),
        "NC: an adapter that reports the fault as a result is detected"
    );
}

#[tokio::test]
#[ignore = "product gap: MCP tools/call maps certain-not-dispatched, certain-failed and unknown-outcome tool errors all to JSON-RPC -32603, so the wire loses the uncertainty and the safe-retry hint"]
async fn mcp_distinguishes_a_safe_retry_from_an_unknown_effect() {
    let (certain_not_dispatched, _) = call(Fault::CertainNotDispatched)
        .await
        .expect_err("F1 is a protocol error");
    let (unknown, _) = call(Fault::UnknownOutcome)
        .await
        .expect_err("F2 is a protocol error");
    let (certain_failed, _) = call(Fault::CertainFailed)
        .await
        .expect_err("F3 is a protocol error");
    assert_ne!(
        certain_not_dispatched, unknown,
        "O0 a safe-to-retry fault and an unknown effect carry different JSON-RPC codes"
    );
    assert_ne!(
        certain_failed, unknown,
        "O0 a certain failure and an unknown effect carry different JSON-RPC codes"
    );
}
