//! Neutral Host port for exporting a Session-owned tool to an ACP workload.
//!
//! The Host owns when a tool is projected and how long it lives. A higher
//! protocol adapter owns the concrete MCP server transport.

use std::sync::Arc;

use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

pub struct AcpToolExport {
    pub server: awaken_run_executor_acp::McpServerConfig,
    _lease: Box<dyn Send + Sync>,
}

impl AcpToolExport {
    #[must_use]
    pub fn new(
        server: awaken_run_executor_acp::McpServerConfig,
        lease: impl Send + Sync + 'static,
    ) -> Self {
        Self {
            server,
            _lease: Box::new(lease),
        }
    }
}

#[async_trait::async_trait]
pub trait AcpToolExporter: Send + Sync {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String>;

    async fn export(
        &self,
        server_name: &str,
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_runtime_contract::tool::RawTool>,
    ) -> Result<AcpToolExport, String> {
        self.export_set(server_name, vec![descriptor], vec![tool])
            .await
    }
}

/// Session-local one-shot authority minted only after the ACP permission path
/// returns Allow. It is deliberately volatile: durable truth is the approval
/// ticket; a recovered ACP Step asks again and receives a fresh one-shot grant.
pub(crate) struct AcpToolExecutionGrants {
    exported: std::collections::HashSet<String>,
    pending: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl AcpToolExecutionGrants {
    pub(crate) fn new(ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            exported: ids.into_iter().collect(),
            pending: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn consume(&self, tool_id: &str, arguments: &serde_json::Value) -> bool {
        let mut pending = self.pending.lock().expect("ACP grant lock poisoned");
        let Some(index) = pending
            .iter()
            .position(|(tool, args)| tool == tool_id && args == arguments)
        else {
            return false;
        };
        pending.remove(index);
        true
    }
}

impl awaken_run_executor_acp::PermissionGrantObserver for AcpToolExecutionGrants {
    fn on_allow(&self, call: &ToolCall) {
        if !self.exported.contains(&call.tool_id) {
            return;
        }
        self.pending
            .lock()
            .expect("ACP grant lock poisoned")
            .push((call.tool_id.clone(), call.arguments.clone()));
    }
}

/// Defense-in-depth guard around every Awaken tool exported to ACP. A normal
/// ACP client first calls `session/request_permission`; that Allow mints the
/// one-shot grant consumed here. Always-allow policy remains executable even if
/// a client omits the optional ask. Ask/deny never cross the dispatch boundary.
pub(crate) struct PermissionGuardedAcpTool {
    inner: Arc<dyn RawTool>,
    policy: Arc<dyn ToolPermissionPolicy>,
    grants: Arc<AcpToolExecutionGrants>,
}

impl PermissionGuardedAcpTool {
    pub(crate) fn new(
        inner: Arc<dyn RawTool>,
        policy: Arc<dyn ToolPermissionPolicy>,
        grants: Arc<AcpToolExecutionGrants>,
    ) -> Self {
        Self {
            inner,
            policy,
            grants,
        }
    }
}

#[async_trait::async_trait]
impl RawTool for PermissionGuardedAcpTool {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn execution_target(&self) -> awaken_runtime_contract::tool::ToolExecutionTarget {
        self.inner.execution_target()
    }

    fn recovery_capability(&self) -> awaken_runtime_contract::tool::ToolRecoveryCapability {
        self.inner.recovery_capability()
    }

    fn concurrency(
        &self,
        arguments: &serde_json::Value,
    ) -> awaken_runtime_contract::tool::ToolConcurrency {
        self.inner.concurrency(arguments)
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        if self.grants.consume(&call.tool_id, &call.arguments) {
            return self.inner.invoke(call).await;
        }
        match self.policy.evaluate(&call).await {
            ToolPermissionVerdict::Allow => self.inner.invoke(call).await,
            ToolPermissionVerdict::Deny { reason } => Err(ToolError::Execution(format!(
                "TOOL_PERMISSION_BLOCKED: {reason}"
            ))),
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                Err(ToolError::Execution(format!(
                    "TOOL_APPROVAL_REQUIRED: ACP must request Awaken permission before invoking this tool ({correlation_id})"
                )))
            }
        }
    }
}

/// Export one exact descriptor/executor set through the existing ACP MCP port.
/// The complete set is served by one Session MCP endpoint and one lifetime
/// lease. Native and ACP therefore consume identical contracts without turning
/// each semantic tool into a separate server/process lifecycle.
pub(crate) async fn export_tools(
    exporter: &dyn AcpToolExporter,
    namespace: &str,
    descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    executors: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
) -> Result<
    (
        Vec<awaken_run_executor_acp::SessionMcpServer>,
        Vec<AcpToolExport>,
    ),
    String,
> {
    let executors = executors
        .into_iter()
        .map(|tool| (tool.id().to_string(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    if descriptors.len() != executors.len() {
        return Err("ACP tool descriptor/executor sets differ in size".into());
    }
    let mut paired_descriptors = Vec::with_capacity(descriptors.len());
    let mut paired_executors = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let tool = executors.get(&descriptor.id).cloned().ok_or_else(|| {
            format!(
                "ACP tool descriptor `{}` has no matching executor",
                descriptor.id
            )
        })?;
        paired_descriptors.push(descriptor);
        paired_executors.push(tool);
    }
    let export = exporter
        .export_set(namespace, paired_descriptors, paired_executors)
        .await?;
    let server = match export.server.transport.clone() {
        awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { command, args } => {
            awaken_run_executor_acp::SessionMcpServer {
                name: export.server.name.clone(),
                command: Some(command),
                args,
                url: None,
                auth: None,
            }
        }
        awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } => {
            awaken_run_executor_acp::SessionMcpServer {
                name: export.server.name.clone(),
                command: None,
                args: Vec::new(),
                url: Some(url),
                auth: None,
            }
        }
    };
    Ok((vec![server], vec![export]))
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingAcpToolExporter(pub(crate) std::sync::Mutex<Vec<(String, Vec<String>)>>);

#[cfg(test)]
#[async_trait::async_trait]
impl AcpToolExporter for RecordingAcpToolExporter {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        _tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String> {
        self.0.lock().unwrap().push((
            server_name.into(),
            descriptors
                .into_iter()
                .map(|descriptor| descriptor.id)
                .collect(),
        ));
        Ok(AcpToolExport::new(
            awaken_run_executor_acp::McpServerConfig {
                name: server_name.into(),
                transport: awaken_run_executor_acp::McpTransport::Http {
                    url: format!("http://127.0.0.1/{server_name}"),
                },
            },
            (),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::permission::ToolPermissionVerdict;

    struct Echo(&'static str);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::tool::RawTool for Echo {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: awaken_runtime_contract::tool::ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                call.call_id,
                self.0,
            ))
        }
    }

    fn descriptor(id: &str) -> awaken_runtime_contract::resolved::ToolDescriptor {
        awaken_runtime_contract::resolved::ToolDescriptor::pinned(
            "test",
            id,
            id,
            serde_json::json!({"type": "object"}),
        )
    }

    struct AskPolicy;

    #[async_trait::async_trait]
    impl ToolPermissionPolicy for AskPolicy {
        async fn evaluate(&self, call: &ToolCall) -> ToolPermissionVerdict {
            ToolPermissionVerdict::RequireConfirmation {
                correlation_id: format!("approval:{}", call.call_id),
            }
        }
    }

    #[tokio::test]
    async fn exported_tool_requires_and_consumes_one_exact_awaken_grant() {
        use awaken_run_executor_acp::PermissionGrantObserver;

        let grants = Arc::new(AcpToolExecutionGrants::new(["write".into()]));
        let guarded = PermissionGuardedAcpTool::new(
            Arc::new(Echo("write")),
            Arc::new(AskPolicy),
            grants.clone(),
        );
        let call = ToolCall {
            call_id: "wire-1".into(),
            tool_id: "write".into(),
            arguments: serde_json::json!({"path":"outputs/proof.md"}),
        };
        let blocked = guarded.invoke(call.clone()).await.unwrap_err();
        assert!(blocked.to_string().contains("TOOL_APPROVAL_REQUIRED"));

        grants.on_allow(&call);
        let mut changed = call.clone();
        changed.arguments = serde_json::json!({"path":"outputs/substituted.md"});
        let substituted = guarded.invoke(changed).await.unwrap_err();
        assert!(
            substituted.to_string().contains("TOOL_APPROVAL_REQUIRED"),
            "a grant cannot authorize substituted arguments"
        );
        assert!(!guarded.invoke(call.clone()).await.unwrap().is_error);
        let replay = guarded.invoke(call).await.unwrap_err();
        assert!(
            replay.to_string().contains("TOOL_APPROVAL_REQUIRED"),
            "one approval cannot authorize a second side effect"
        );
    }

    #[tokio::test]
    async fn acp_export_reuses_the_exact_native_descriptor_executor_set() {
        // Cause/effect decision table: R1 exact two-tool set -> two deterministic
        // MCP routes and two retained leases; R2 missing executor -> reject before
        // exporting anything. This is the ACP/native compatibility invariant:
        // only transport changes, never the semantic tool contracts.
        let exporter = RecordingAcpToolExporter::default();
        let (servers, leases) = export_tools(
            &exporter,
            "awaken_session",
            vec![descriptor("list_memories"), descriptor("read_memory")],
            vec![
                Arc::new(Echo("list_memories")),
                Arc::new(Echo("read_memory")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(servers.len(), 1, "R1 one route");
        assert_eq!(leases.len(), 1, "R1 one lease");
        assert_eq!(
            exporter.0.lock().unwrap().as_slice(),
            &[(
                "awaken_session".into(),
                vec!["list_memories".into(), "read_memory".into()]
            )],
            "R1 exact contracts"
        );

        let missing = RecordingAcpToolExporter::default();
        assert!(
            export_tools(
                &missing,
                "awaken_session",
                vec![descriptor("read_memory")],
                Vec::new(),
            )
            .await
            .is_err(),
            "R2"
        );
        assert!(missing.0.lock().unwrap().is_empty(), "R2 no side effect");
    }
}
