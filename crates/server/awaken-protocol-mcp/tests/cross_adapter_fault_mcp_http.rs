//! T33-C03 (cross-adapter consistency), MCP row over the actual transport: the
//! shared injected upstream fault, in the tool vocabulary, crosses the
//! production MCP chain end to end: an exported tool fails inside
//! `McpToolService`, the axum Streamable HTTP `router` serves it on a real
//! local port, and the in-repo MCP client (`awaken-ext-mcp` `HttpTransport` +
//! `McpRawTool`) maps the reply back to a runtime `ToolError`. (The sibling
//! `cross_adapter_fault_mcp.rs` covers the service-level JSON-RPC answer.)
//!
//! | fault | origin `ToolError`          | authority                     | uncertain | retry hint      |
//! | F1    | `UnavailableBeforeDispatch` | `unavailable_before_dispatch` | no        | retry-safe      |
//! | F2    | `Indeterminate`             | `indeterminate`               | yes       | never repeated  |
//! | F3    | `DefinitiveFailure`         | `definitive_failure`          | no        | not retryable   |
//!
//! Oracles: O0 authority/uncertainty/retry hint preserved; O1 never success.
//! Holds: O1 for every fault, and O0 for F2 (uncertainty survives). Gap: the
//! service maps F1/F2/F3 alike to JSON-RPC -32603 with no classification
//! (`McpToolService` `call_tool`), so the client must treat every one as
//! Indeterminate; F1 loses its retry-safe hint and F3 its certainty. The two
//! ignored reproducers assert the correct classification on the client and
//! on the wire.
//! Negative control (executed): a server-side adapter mutant that swallows the
//! fault into a successful result is served through the same HTTP chain; the
//! same checker must flag the client's observation as success.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_mcp::transport::McpToolTransport;
use awaken_ext_mcp::{Credential, HttpTransport, McpRawTool};
use awaken_protocol_mcp::{McpExportedTool, McpHttpConfig, McpToolService, StaticExports, router};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectedFault {
    Retryable,
    Unknown,
    Definitive,
}

impl InjectedFault {
    const ALL: [Self; 3] = [Self::Retryable, Self::Unknown, Self::Definitive];

    fn tool(self) -> &'static str {
        match self {
            Self::Retryable => "fault_retryable",
            Self::Unknown => "fault_unknown",
            Self::Definitive => "fault_definitive",
        }
    }

    /// The upstream's own account; each fault has a distinct marker.
    fn detail(self) -> String {
        format!(
            "upstream 529 overloaded_error (retry-after 7s) [{}]",
            self.tool()
        )
    }

    fn origin_error(self) -> ToolError {
        match self {
            Self::Retryable => ToolError::UnavailableBeforeDispatch(self.detail()),
            Self::Unknown => ToolError::Indeterminate(self.detail()),
            Self::Definitive => ToolError::DefinitiveFailure(self.detail()),
        }
    }

    fn expected(self) -> Classification {
        match self {
            Self::Retryable => Classification::failure("unavailable_before_dispatch", false, true),
            Self::Unknown => Classification::failure("indeterminate", true, false),
            Self::Definitive => Classification::failure("definitive_failure", false, false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Classification {
    authority: String,
    uncertain: bool,
    retryable: bool,
    success: bool,
}

impl Classification {
    fn failure(authority: &str, uncertain: bool, retryable: bool) -> Self {
        Self {
            authority: authority.to_string(),
            uncertain,
            retryable,
            success: false,
        }
    }
}

fn classify(result: &Result<ToolOutput, ToolError>) -> Classification {
    match result {
        Ok(output) if output.is_error => Classification::failure("tool_error_result", false, false),
        Ok(_) => Classification {
            authority: "success".to_string(),
            uncertain: false,
            retryable: false,
            success: true,
        },
        Err(ToolError::UnavailableBeforeDispatch(_)) => {
            Classification::failure("unavailable_before_dispatch", false, true)
        }
        Err(ToolError::Indeterminate(_)) => Classification::failure("indeterminate", true, false),
        Err(ToolError::DefinitiveFailure(_)) => {
            Classification::failure("definitive_failure", false, false)
        }
        Err(ToolError::StateInvalidatedAfterDispatch(_)) => {
            Classification::failure("state_invalidated_after_dispatch", false, false)
        }
        Err(other) => Classification::failure(&format!("other: {other}"), false, false),
    }
}

fn check_not_success(adapter: &str, observed: &Classification) -> Result<(), String> {
    if observed.success {
        return Err(format!(
            "O1 {adapter}: the injected fault surfaced as success: {observed:?}"
        ));
    }
    Ok(())
}

fn check(
    adapter: &str,
    expected: &Classification,
    observed: &Classification,
) -> Result<(), String> {
    check_not_success(adapter, observed)?;
    if observed != expected {
        return Err(format!(
            "O0 {adapter}: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

/// The upstream: an exported tool that fails with the injected fault.
struct FaultTool(InjectedFault);

#[async_trait]
impl RawTool for FaultTool {
    fn id(&self) -> &str {
        self.0.tool()
    }

    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        Err(self.0.origin_error())
    }
}

/// Negative-control server adapter: swallows the fault into success.
struct SwallowingTool(Arc<dyn RawTool>);

#[async_trait]
impl RawTool for SwallowingTool {
    fn id(&self) -> &str {
        self.0.id()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let call_id = call.call_id.clone();
        match self.0.invoke(call).await {
            Ok(output) => Ok(output),
            Err(_) => Ok(ToolOutput::ok(call_id, "swallowed".to_string())),
        }
    }
}

fn descriptor(id: &str) -> ToolDescriptor {
    ToolDescriptor::pinned(
        "t33-c03",
        id,
        "injected upstream fault",
        json!({ "type": "object" }),
    )
}

/// Serve the fault tools (optionally behind the swallowing mutant) on a real
/// local port; returns the client transport after a real handshake.
async fn serve(swallow: bool) -> Arc<HttpTransport> {
    let exports = InjectedFault::ALL
        .into_iter()
        .map(|fault| {
            let tool: Arc<dyn RawTool> = Arc::new(FaultTool(fault));
            let tool: Arc<dyn RawTool> = if swallow {
                Arc::new(SwallowingTool(tool))
            } else {
                tool
            };
            McpExportedTool::plain(descriptor(fault.tool()), tool)
        })
        .collect();
    let service = Arc::new(McpToolService::new(
        "t33-c03",
        "0.0.0",
        Arc::new(StaticExports::new(exports)),
    ));
    let app = router(
        service,
        McpHttpConfig {
            path: "/mcp".to_string(),
            bearer_token: None,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server runs");
    });
    Arc::new(
        HttpTransport::connect(url, Credential::None)
            .await
            .expect("MCP handshake over HTTP"),
    )
}

/// The full adapter chain: the client tool the runtime would execute.
async fn invoke_through_mcp(
    transport: &Arc<HttpTransport>,
    fault: InjectedFault,
) -> Result<ToolOutput, ToolError> {
    let client_transport: Arc<dyn McpToolTransport> = transport.clone();
    let tool = McpRawTool::new("t33", fault.tool(), client_transport).expect("client tool");
    tool.invoke(ToolCall {
        call_id: format!("t33-c03-{}", fault.tool()),
        tool_id: tool.id().to_string(),
        arguments: json!({}),
    })
    .await
}

fn error_text(result: &Result<ToolOutput, ToolError>) -> String {
    match result {
        Ok(output) => output.text(),
        Err(error) => error.to_string(),
    }
}

#[tokio::test]
async fn mcp_http_chain_never_reports_an_injected_tool_fault_as_success() {
    let transport = serve(false).await;
    for fault in InjectedFault::ALL {
        let result = invoke_through_mcp(&transport, fault).await;
        let observed = classify(&result);
        check_not_success("mcp-http", &observed)
            .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
        assert!(
            result.is_err(),
            "{fault:?}: a protocol error, never a ToolOutput"
        );
        assert!(
            error_text(&result).contains(&fault.detail()),
            "{fault:?}: the upstream's account crosses the chain: {}",
            error_text(&result)
        );
        // Uncertainty is never dropped: every client observation is at
        // least as uncertain as its origin.
        assert!(
            observed.uncertain || !fault.expected().uncertain,
            "{fault:?}: uncertainty dropped: {observed:?}"
        );
    }
    let unknown = invoke_through_mcp(&transport, InjectedFault::Unknown).await;
    check(
        "mcp-http",
        &InjectedFault::Unknown.expected(),
        &classify(&unknown),
    )
    .unwrap_or_else(|violation| panic!("F2: {violation}"));
}

#[tokio::test]
async fn mcp_http_fault_checker_detects_a_server_adapter_that_swallows_the_fault() {
    let transport = serve(true).await;
    for fault in InjectedFault::ALL {
        let result = invoke_through_mcp(&transport, fault).await;
        let observed = classify(&result);
        assert!(
            observed.success,
            "{fault:?}: the mutant really reported success"
        );
        let violation = check_not_success("mcp-http-mutant", &observed)
            .expect_err("the checker detects the swallowed fault");
        assert!(violation.starts_with("O1"), "{fault:?}: {violation}");
    }
}

#[tokio::test]
#[ignore = "product gap: McpToolService maps UnavailableBeforeDispatch/Indeterminate/DefinitiveFailure all to JSON-RPC -32603, so the MCP client sees certain retry-safe and definitive faults as Indeterminate"]
async fn mcp_http_chain_keeps_the_certain_and_retry_safe_tool_fault_classification() {
    let transport = serve(false).await;
    for fault in [InjectedFault::Retryable, InjectedFault::Definitive] {
        let result = invoke_through_mcp(&transport, fault).await;
        check("mcp-http", &fault.expected(), &classify(&result))
            .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
    }
}

/// The JSON-RPC error object of a raw `tools/call`, without its free text.
async fn wire_class(transport: &Arc<HttpTransport>, fault: InjectedFault) -> serde_json::Value {
    let error = transport
        .call_tool(fault.tool(), json!({}))
        .await
        .expect_err("a failed tool call is a JSON-RPC error")
        .to_string();
    let start = error
        .find('{')
        .unwrap_or_else(|| panic!("{fault:?}: no JSON-RPC error object in {error}"));
    let mut object: serde_json::Value = serde_json::from_str(&error[start..])
        .unwrap_or_else(|parse| panic!("{fault:?}: unparseable JSON-RPC error {error}: {parse}"));
    if let Some(map) = object.as_object_mut() {
        map.remove("message");
    }
    object
}

#[tokio::test]
#[ignore = "product gap: over Streamable HTTP certain and uncertain tool faults reach the client as the same JSON-RPC error (-32603, no classification data)"]
async fn mcp_http_wire_distinguishes_certain_from_uncertain_tool_faults() {
    let transport = serve(false).await;
    let unknown = wire_class(&transport, InjectedFault::Unknown).await;
    for fault in [InjectedFault::Retryable, InjectedFault::Definitive] {
        let certain = wire_class(&transport, fault).await;
        assert_ne!(
            certain, unknown,
            "{fault:?}: a certain fault must not share the uncertain fault's wire classification"
        );
    }
}
