//! Neutral Host port for exporting a Session-owned tool to an ACP workload.
//!
//! The Host owns when a tool is projected and how long it lives. A higher
//! protocol adapter owns the concrete MCP server transport.

use std::sync::Arc;

use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolConcurrency, ToolError, ToolExecutionTarget, ToolExecutor, ToolOutput,
    ToolRecoveryCapability,
};

use awaken_runtime_contract::tool_export::{AcpToolExport, AcpToolExporter};

/// ACP transport adapter for the Environment's one canonical `ToolExecutor`.
/// The descriptor owns the public id; invocation, path projection, recovery and
/// concurrency remain owned by the already-bound Environment executor.
struct ExecutorBackedAcpTool {
    id: String,
    executor: Arc<dyn ToolExecutor>,
}

#[async_trait::async_trait]
impl RawTool for ExecutorBackedAcpTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.executor.recovery_capability(&self.id)
    }

    fn concurrency(&self, arguments: &serde_json::Value) -> ToolConcurrency {
        self.executor.concurrency(&self.id, arguments)
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.executor.invoke(&call).await
    }
}

/// Bind the selected Hand descriptors to one Environment executor without
/// rebuilding a host-side RawTool registry for Namespace or Container tiers.
pub(crate) fn environment_executor_tools(
    ids: impl IntoIterator<Item = String>,
    executor: Arc<dyn ToolExecutor>,
) -> Vec<Arc<dyn RawTool>> {
    ids.into_iter()
        .map(|id| {
            Arc::new(ExecutorBackedAcpTool {
                id,
                executor: executor.clone(),
            }) as Arc<dyn RawTool>
        })
        .collect()
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

/// Bind the MCP-facing RawTool port to the existing Environment executor.
/// Workdir, Namespace and Container use this same dispatch path; this adapter
/// owns neither a tool registry nor a second execution/sandbox lifecycle.
pub(crate) struct EnvironmentAcpTool {
    id: String,
    executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor>,
}
impl EnvironmentAcpTool {
    pub(crate) fn new(
        id: String,
        executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor>,
    ) -> Self {
        Self { id, executor }
    }
}
#[async_trait::async_trait]
impl RawTool for EnvironmentAcpTool {
    fn id(&self) -> &str {
        &self.id
    }
    fn recovery_capability(&self) -> awaken_runtime_contract::tool::ToolRecoveryCapability {
        self.executor.recovery_capability(&self.id)
    }
    fn concurrency(
        &self,
        args: &serde_json::Value,
    ) -> awaken_runtime_contract::tool::ToolConcurrency {
        self.executor.concurrency(&self.id, args)
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        if call.tool_id != self.id {
            return Err(ToolError::Execution(
                "ACP tool identity does not match the bound Environment tool".into(),
            ));
        }
        self.executor.invoke(&call).await
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
    let descriptor_ids = descriptors
        .iter()
        .map(|descriptor| descriptor.id.clone())
        .collect::<Vec<_>>();
    let executor_ids = executors
        .iter()
        .map(|tool| tool.id().to_string())
        .collect::<Vec<_>>();
    let executors = executors
        .into_iter()
        .map(|tool| (tool.id().to_string(), tool))
        .collect::<std::collections::BTreeMap<_, _>>();
    if descriptors.len() != executors.len() {
        return Err(format!(
            "ACP tool descriptor/executor sets differ: descriptors={descriptor_ids:?}, executors={executor_ids:?}"
        ));
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

    #[derive(Default)]
    struct RecordingExecutor(std::sync::Mutex<Vec<ToolCall>>);

    #[async_trait::async_trait]
    impl ToolExecutor for RecordingExecutor {
        fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
            assert_eq!(tool_id, "write");
            ToolRecoveryCapability::Idempotent
        }

        fn concurrency(&self, tool_id: &str, _arguments: &serde_json::Value) -> ToolConcurrency {
            assert_eq!(tool_id, "write");
            ToolConcurrency::Serial
        }

        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(call.clone());
            Ok(ToolOutput::ok(call.call_id.clone(), "environment-hand"))
        }
    }

    #[tokio::test]
    async fn acp_hand_adapter_delegates_only_to_the_bound_environment_executor() {
        // Causes: C1 a publication selects Hand id `write`; C2 the realized
        // Environment supplies its canonical executor; C3 ACP calls that id.
        // Effects: E1 recovery/concurrency are delegated for the same id; E2 the
        // exact call reaches the Environment executor once; E3 no default
        // host-side RawTool is constructed. Decision rule H1=C1+C2+C3=>E1+E2;
        // E3 follows structurally because this adapter stores only id+executor.
        let executor = Arc::new(RecordingExecutor::default());
        let tools = environment_executor_tools(
            ["write".to_string()],
            executor.clone() as Arc<dyn ToolExecutor>,
        );
        let tool = &tools[0];
        assert_eq!(
            tool.recovery_capability(),
            ToolRecoveryCapability::Idempotent
        );
        assert_eq!(
            tool.concurrency(&serde_json::json!({})),
            ToolConcurrency::Serial
        );
        let call = ToolCall {
            call_id: "call-1".into(),
            tool_id: "write".into(),
            arguments: serde_json::json!({"file_path":"/mnt/session/outputs/result.txt"}),
        };
        assert_eq!(
            tool.invoke(call.clone()).await.unwrap().text(),
            "environment-hand"
        );
        assert_eq!(executor.0.lock().unwrap().as_slice(), &[call]);
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

#[cfg(test)]
mod environment_tool_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Executor(AtomicUsize);
    #[async_trait::async_trait]
    impl awaken_runtime_contract::tool::ToolExecutor for Executor {
        fn concurrency(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> awaken_runtime_contract::tool::ToolConcurrency {
            awaken_runtime_contract::tool::ToolConcurrency::Serial
        }
        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert_eq!(call.tool_id, "write");
            assert_eq!(call.call_id, "call");
            assert_eq!(
                call.arguments,
                serde_json::json!({"path":"/mnt/session/outputs/file"})
            );
            Ok(ToolOutput::ok(&call.call_id, "bound environment"))
        }
    }
    #[tokio::test]
    async fn bound_environment_is_the_only_dispatch_owner() {
        // Causes: matching canonical id + exact call/args + selected executor.
        // R1 match => forward once with original correlation and concurrency;
        // R2 mismatch => refuse before dispatch. Actual Namespace/Docker realization
        // is covered by the topology E2Es, never inferred from this fake executor.
        let executor = Arc::new(Executor(AtomicUsize::new(0)));
        let tool = EnvironmentAcpTool::new("write".into(), executor.clone());
        let mut call = ToolCall {
            tool_id: "write".into(),
            call_id: "call".into(),
            arguments: serde_json::json!({"path":"/mnt/session/outputs/file"}),
        };
        assert!(matches!(
            tool.concurrency(&call.arguments),
            awaken_runtime_contract::tool::ToolConcurrency::Serial
        ));
        tool.invoke(call.clone()).await.unwrap();
        call.tool_id = "bash".into();
        assert!(tool.invoke(call).await.is_err());
        assert_eq!(executor.0.load(Ordering::SeqCst), 1);
    }
}
