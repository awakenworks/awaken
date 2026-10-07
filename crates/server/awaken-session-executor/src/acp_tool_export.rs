// @formal entrypoint acp.tool_export_requires_claim_execution_environment
//! Neutral Host port for exporting a Session-owned tool to an ACP workload.
//!
//! The Host owns when a tool is projected and how long it lives. A higher
//! protocol adapter owns the concrete MCP server transport.

use std::sync::Arc;

use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
use awaken_tool_contract::tool::{
    RawTool, ToolCall, ToolError, ToolExecutionTarget, ToolExecutor, ToolOutput,
};

use awaken_session_environment_contract::tool_export::{AcpToolExport, AcpToolExporter};

/// Bind the selected Hand descriptors to one Environment executor without
/// rebuilding a host-side RawTool registry for Namespace or Container tiers.
pub(crate) fn environment_executor_tools(
    ids: impl IntoIterator<Item = String>,
    executor: Arc<dyn ToolExecutor>,
) -> Vec<Arc<dyn RawTool>> {
    ids.into_iter()
        .map(|id| Arc::new(EnvironmentAcpTool::new(id, executor.clone())) as Arc<dyn RawTool>)
        .collect()
}

/// Model-visible client tools share the Session MCP export, while their adapter
/// only signals arrival to the existing Runtime continuation.
pub(crate) struct ClientExecutedAcpTools {
    pub(crate) descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    pub(crate) executors: Vec<Arc<dyn RawTool>>,
    pub(crate) ids: std::collections::HashSet<String>,
    pub(crate) requests: Option<Arc<ClientExecutedAcpToolRequests>>,
}

const CLIENT_EXECUTED_TOOL_ARRIVAL_CAPACITY: usize = 32;

fn client_executed_tool_arrivals(
    capacity: usize,
) -> (
    tokio::sync::mpsc::Sender<ToolCall>,
    Arc<ClientExecutedAcpToolRequests>,
) {
    let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
    (
        sender,
        Arc::new(ClientExecutedAcpToolRequests {
            receiver: tokio::sync::Mutex::new(receiver),
        }),
    )
}

pub(crate) fn client_executed_tools(
    source: &[awaken_runtime_contract::resolved::ToolDescriptor],
) -> ClientExecutedAcpTools {
    let descriptors = source
        .iter()
        .filter(|descriptor| {
            descriptor.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
        })
        .cloned()
        .collect::<Vec<_>>();
    let ids = descriptors
        .iter()
        .map(|descriptor| descriptor.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let arrivals = (!ids.is_empty())
        .then(|| client_executed_tool_arrivals(CLIENT_EXECUTED_TOOL_ARRIVAL_CAPACITY));
    let requests = arrivals.as_ref().map(|(_, requests)| requests.clone());
    let executors = descriptors
        .iter()
        .map(|descriptor| {
            Arc::new(ClientExecutedAcpTool::new(
                descriptor.id.clone(),
                arrivals
                    .as_ref()
                    .expect("client descriptors require one request edge")
                    .0
                    .clone(),
            )) as Arc<dyn RawTool>
        })
        .collect();
    ClientExecutedAcpTools {
        descriptors,
        executors,
        ids,
        requests,
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

impl awaken_runtime_contract::permission::PermissionGrantObserver for AcpToolExecutionGrants {
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

/// Uses the same frozen export membership as one-shot MCP grants. A durable
/// permission continuation enters Runtime directly; it never mints an extra MCP
/// grant or creates another tool catalog/connection.
pub(crate) struct AcpToolContinuation {
    pub(crate) runtime: Arc<awaken_runtime::Runtime>,
    pub(crate) host_exports: Option<Arc<AcpToolExecutionGrants>>,
    pub(crate) client_tools: std::collections::HashSet<String>,
    pub(crate) client_requests: Option<Arc<ClientExecutedAcpToolRequests>>,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::resume::OwnedToolContinuation for AcpToolContinuation {
    fn tool_await_reason(
        &self,
        tool_id: &str,
    ) -> Option<awaken_agent_contract::agent::awaiting::ToolAwaitReason> {
        if self.client_tools.contains(tool_id) {
            Some(awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution)
        } else if self
            .host_exports
            .as_ref()
            .is_some_and(|exports| exports.exported.contains(tool_id))
        {
            Some(awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission)
        } else {
            None
        }
    }

    async fn next_client_execution(&self) -> Option<ToolCall> {
        match &self.client_requests {
            Some(requests) => requests.next().await,
            None => std::future::pending().await,
        }
    }

    async fn resume(
        &self,
        activation: awaken_runtime_contract::activation::RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<awaken_agent_contract::agent::run::RunState>
    {
        let context = activation.narrow_context(context);
        let reader = context.reader.clone().ok_or_else(|| {
            awaken_runtime_contract::execution::Error::Execution(
                "owned tool resume requires committed history".into(),
            )
        })?;
        self.runtime.register_snapshot(activation.snapshot);
        self.runtime
            .resume_tool_batch(command, reader.as_ref(), context)
            .await
    }

    async fn recover(
        &self,
        activation: awaken_runtime_contract::activation::RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<awaken_agent_contract::agent::run::RunState>
    {
        self.runtime.recover_tool_batch(activation, context).await
    }
}

/// ACP needs a model-visible MCP contract for a client-executed tool, but the
/// client remains its only executor. Reaching this adapter means an ACP client
/// bypassed `session/request_permission`; fail closed without a Host effect.
pub(crate) struct ClientExecutedAcpTool {
    id: String,
    sender: tokio::sync::mpsc::Sender<ToolCall>,
}

impl ClientExecutedAcpTool {
    pub(crate) fn new(id: String, sender: tokio::sync::mpsc::Sender<ToolCall>) -> Self {
        Self { id, sender }
    }
}

pub(crate) struct ClientExecutedAcpToolRequests {
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ToolCall>>,
}

impl ClientExecutedAcpToolRequests {
    async fn next(&self) -> Option<ToolCall> {
        self.receiver.lock().await.recv().await
    }
}

#[async_trait::async_trait]
impl RawTool for ClientExecutedAcpTool {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.sender.send(call).await.map_err(|_| {
            ToolError::DefinitiveFailure(
                "CLIENT_TOOL_ARRIVAL_CLOSED: the Session continuation is no longer available"
                    .into(),
            )
        })?;
        std::future::pending().await
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

    fn execution_target(&self) -> awaken_tool_contract::tool::ToolExecutionTarget {
        self.inner.execution_target()
    }

    fn recovery_capability(&self) -> awaken_tool_contract::tool::ToolRecoveryCapability {
        self.inner.recovery_capability()
    }

    fn concurrency(
        &self,
        arguments: &serde_json::Value,
    ) -> awaken_tool_contract::tool::ToolConcurrency {
        self.inner.concurrency(arguments)
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        if self.grants.consume(&call.tool_id, &call.arguments) {
            return self.inner.invoke(call).await;
        }
        // The exported ACP tool reports only the outcome; the basis belongs to
        // the audit fact, not to a tool error string.
        match self.policy.evaluate(&call).await.verdict {
            ToolPermissionVerdict::Allow => self.inner.invoke(call).await,
            ToolPermissionVerdict::Deny { reason } => Err(ToolError::DefinitiveFailure(format!(
                "TOOL_PERMISSION_BLOCKED: {reason}"
            ))),
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                Err(ToolError::DefinitiveFailure(format!(
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
    executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor>,
}
impl EnvironmentAcpTool {
    pub(crate) fn new(
        id: String,
        executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor>,
    ) -> Self {
        Self { id, executor }
    }
}
#[async_trait::async_trait]
impl RawTool for EnvironmentAcpTool {
    fn execution_target(&self) -> ToolExecutionTarget {
        ToolExecutionTarget::Sandbox
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn recovery_capability(&self) -> awaken_tool_contract::tool::ToolRecoveryCapability {
        self.executor.recovery_capability(&self.id)
    }
    fn concurrency(&self, args: &serde_json::Value) -> awaken_tool_contract::tool::ToolConcurrency {
        self.executor.concurrency(&self.id, args)
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        if call.tool_id != self.id {
            return Err(ToolError::DefinitiveFailure(
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
    executors: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
) -> Result<
    (
        Vec<awaken_acp_contract::SessionMcpServer>,
        Vec<AcpToolExport>,
    ),
    String,
> {
    let (paired_descriptors, paired_executors) =
        awaken_session_environment_contract::tool_export::pair_tools(descriptors, executors)?;
    let export = exporter
        .export_set(namespace, paired_descriptors, paired_executors)
        .await?;
    let server = match export.server.transport.clone() {
        awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { command, args } => {
            awaken_acp_contract::SessionMcpServer {
                name: export.server.name.clone(),
                command: Some(command),
                args,
                url: None,
                auth: None,
            }
        }
        awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } => {
            awaken_acp_contract::SessionMcpServer {
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
        _tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
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
    use awaken_tool_contract::tool::{ToolConcurrency, ToolRecoveryCapability};

    struct Echo(&'static str);

    #[async_trait::async_trait]
    impl awaken_tool_contract::tool::RawTool for Echo {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: awaken_tool_contract::tool::ToolCall,
        ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError>
        {
            Ok(awaken_tool_contract::tool::ToolOutput::ok(
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

    use awaken_runtime_contract::permission::ToolPermissionRuling;

    #[async_trait::async_trait]
    impl ToolPermissionPolicy for AskPolicy {
        async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling {
            ToolPermissionRuling::policy(
                ToolPermissionVerdict::RequireConfirmation {
                    correlation_id: format!("approval:{}", call.call_id),
                },
                awaken_runtime_contract::permission::PermissionBasis::fixed(
                    awaken_runtime_contract::permission::PermissionPolicy::Rule,
                ),
            )
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
        // Causes: C1 the neutral permission observer received an Allow for the
        // exact tool and arguments; C2 the call matches that grant; C3 it has
        // not already been consumed. Effects: E1 only C1+C2+C3 executes the
        // tool once; E2 missing, substituted or replayed calls fail closed.
        // R1 no grant -> deny; R2 exact fresh grant -> one effect; R3 changed
        // args or consumed grant -> deny. The observer supplies no authority.
        use awaken_runtime_contract::permission::PermissionGrantObserver;

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
    async fn client_tool_export_is_model_visible_but_never_host_executable() {
        // Cause/effect decision table: C1 a descriptor is ClientExecuted and the
        // ACP adapter asks permission => the continuation classifies
        // ClientExecution; C2 an adapter bypasses that boundary and invokes MCP
        // directly => notify the existing continuation in FIFO order and remain
        // pending with no Host executor. C3 notification precedes/follows the
        // waiter => no lost wakeup. These rules cover discovery and execution
        // ownership without another persistent result store.
        use awaken_agent_contract::agent::awaiting::ToolAwaitReason;
        use awaken_runtime_contract::resume::OwnedToolContinuation;

        let (sender, requests) = client_executed_tool_arrivals(2);
        let tool = ClientExecutedAcpTool::new("desktop_search".into(), sender.clone());
        assert_eq!(tool.id(), "desktop_search");
        let invocation = tokio::spawn(async move {
            tool.invoke(ToolCall {
                call_id: "client-call-1".into(),
                tool_id: "desktop_search".into(),
                arguments: serde_json::json!({"query":"first"}),
            })
            .await
        });
        assert_eq!(
            requests.next().await.expect("open sender").call_id,
            "client-call-1",
            "C2"
        );
        let second_invocation = tokio::spawn(async move {
            ClientExecutedAcpTool::new("desktop_search".into(), sender)
                .invoke(ToolCall {
                    call_id: "client-call-2".into(),
                    tool_id: "desktop_search".into(),
                    arguments: serde_json::json!({"query":"second"}),
                })
                .await
        });
        assert_eq!(
            requests.next().await.expect("open sender").call_id,
            "client-call-2",
            "C2+C3"
        );
        assert!(!invocation.is_finished(), "C2 waits for the Session client");
        invocation.abort();
        second_invocation.abort();

        let continuation = AcpToolContinuation {
            runtime: Arc::new(awaken_runtime::Runtime::new()),
            host_exports: None,
            client_tools: ["desktop_search".to_string()].into_iter().collect(),
            client_requests: None,
        };
        assert_eq!(
            continuation.tool_await_reason("desktop_search"),
            Some(ToolAwaitReason::ClientExecution),
            "C1"
        );
        assert_eq!(continuation.tool_await_reason("DesktopSearch"), None);
    }

    #[tokio::test]
    async fn client_tool_arrival_channel_preserves_cancellation_backpressure_and_close() {
        // Cause/effect graph: C1 a waiter may be cancelled; C2 capacity may be
        // exhausted; C3 every sender may be dropped. Effects: E1 cancellation
        // consumes no call; E2 producers wait and FIFO is preserved; E3 the
        // consumer observes terminal closure instead of waiting forever.
        // Decision table: R1 C1 -> E1; R2 C2 -> E2; R3 C3 -> E3. These are the
        // loss, overload, and shutdown cases that the removed VecDeque+Notify
        // implementation could not express with one canonical queue primitive.
        let (sender, requests) = client_executed_tool_arrivals(1);

        let cancelled_waiter = tokio::spawn({
            let requests = requests.clone();
            async move { requests.next().await }
        });
        tokio::task::yield_now().await;
        cancelled_waiter.abort();

        let first = ToolCall {
            call_id: "first".into(),
            tool_id: "desktop_search".into(),
            arguments: serde_json::json!({"query":"first"}),
        };
        sender.send(first).await.expect("R1 channel remains open");

        let blocked_sender = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(ToolCall {
                        call_id: "second".into(),
                        tool_id: "desktop_search".into(),
                        arguments: serde_json::json!({"query":"second"}),
                    })
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!blocked_sender.is_finished(), "R2 bounded backpressure");
        assert_eq!(
            requests.next().await.expect("R1 first retained").call_id,
            "first"
        );
        blocked_sender
            .await
            .expect("R2 producer task")
            .expect("R2 send");
        assert_eq!(
            requests.next().await.expect("R2 second retained").call_id,
            "second"
        );

        drop(sender);
        assert!(requests.next().await.is_none(), "R3 terminal closure");
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
    #[tokio::test]
    async fn owned_continuation_uses_the_runtime_and_does_not_leave_an_mcp_grant() {
        // Cause/effect table: H1 exact owned Write + Allow => Runtime invokes the
        // frozen arguments once, commits its result, returns Running; H2 Deny
        // => no invocation and a committed error result. Both rows: no Native
        // model is configured; finalized reclaim never re-enters the tool; the
        // Session MCP guard still requires a fresh approval for any new call.
        // Membership is the same export set used by grants (canonical write yes,
        // external MCP/lookalike no), not a second inferred tool catalog.
        use awaken_agent_contract::agent::awaiting::{
            AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason,
        };
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
        use awaken_agent_contract::agent::run::{Id as RunId, RunState};
        use awaken_agent_contract::agent::state::StateKey;
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        use awaken_agent_contract::thread::commit::coordinator::Coordinator;
        use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
        use awaken_runtime_contract::resolved::{
            CatalogFingerprint, ModelBinding, ResolvedModelCandidate, ResolvedSpec,
        };
        use awaken_runtime_contract::resume::{OwnedToolContinuation, ResumeCommand, ResumeResult};
        use awaken_runtime_contract::snapshot::{
            AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
        };
        use awaken_runtime_contract::{CommittedThreadView, RuntimeRunContext};
        use awaken_tool_contract::{ActiveToolBatch, ToolBatch, ToolWaitKind};

        struct RecordingTool(Arc<std::sync::Mutex<Vec<ToolCall>>>);
        #[async_trait::async_trait]
        impl RawTool for RecordingTool {
            fn id(&self) -> &str {
                "write"
            }
            async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
                self.0.lock().unwrap().push(call.clone());
                Ok(ToolOutput::ok(call.call_id, "owned-write-complete"))
            }
        }
        for allow in [true, false] {
            let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            ));
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let tool: Arc<dyn RawTool> = Arc::new(RecordingTool(calls.clone()));
            let runtime = Arc::new(awaken_runtime::Runtime::new().with_tool(tool.clone()));
            let exports = Arc::new(AcpToolExecutionGrants::new(["write".into()]));
            let continuation = AcpToolContinuation {
                runtime,
                host_exports: Some(exports.clone()),
                client_tools: Default::default(),
                client_requests: None,
            };
            assert_eq!(
                continuation.tool_await_reason("write"),
                Some(ToolAwaitReason::Permission)
            );
            assert_eq!(continuation.tool_await_reason("mcp__external__write"), None);
            assert_eq!(continuation.tool_await_reason("Write"), None);
            let run_id = RunId("owned-run".into());
            let thread_id = ThreadId("owned-thread".into());
            let snapshot = ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("owned-snapshot".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("owned-agent".into()),
                fingerprint: CatalogFingerprint("owned-fp".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("owned-fp".into()),
                    instructions: String::new(),
                    max_steps: Some(4),
                    delegation_limits: Default::default(),
                    model_binding: ResolvedModelCandidate::host(ModelBinding::new(
                        "test",
                        "test",
                        "acp:claude",
                    )),
                    tool_descriptors: vec![descriptor("write")],
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
            };
            let activation = awaken_runtime_contract::activation::RunActivation::new(
                run_id.clone(),
                thread_id.clone(),
                snapshot.clone(),
                Vec::new(),
            );
            let call = ToolCall {
                call_id: "owned-call".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({"file_path":"/workspace/output.txt", "content":"é中文🙂\\\"\n".repeat(3000)}),
            };
            let ticket = ResumeTicket::new(
                "owned-permission",
                run_id.clone(),
                thread_id.clone(),
                snapshot.id.0,
                snapshot.fingerprint.0,
                AwaitTarget::ToolCall {
                    authority:
                        awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                    reason: ToolAwaitReason::Permission,
                    call_id: call.call_id.clone(),
                    tool: PendingTool {
                        tool_id: call.tool_id.clone(),
                        arguments: call.arguments.clone(),
                    },
                },
            );
            let mut batch =
                ToolBatch::for_step(run_id.clone(), 0, [(call.clone(), Default::default())])
                    .unwrap();
            batch
                .mark_awaiting(
                    &call.call_id,
                    ToolWaitKind::ToolPermission,
                    &ticket.correlation_id,
                )
                .unwrap();
            commit
                .commit(ThreadCommit::assemble(
                    thread_id.clone(),
                    RunDisposition::awaiting(ticket.clone()),
                    true,
                    vec![Message::new(
                        MessageId::assistant(&run_id, 0),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            &call.call_id,
                            &call.tool_id,
                            call.arguments.clone(),
                        )],
                    )],
                    vec![ActiveToolBatch::write(&Some(batch))],
                    Vec::new(),
                ))
                .await
                .unwrap();
            let context = RuntimeRunContext::new()
                .with_reader(commit.clone())
                .with_commit(commit.clone());
            let decision = if allow {
                ResumeResult::allow()
            } else {
                ResumeResult::deny(Some("user denied".into()))
            };
            assert_eq!(
                continuation
                    .resume(
                        activation.clone(),
                        ResumeCommand::from_ticket(&ticket, decision, 0),
                        context.clone()
                    )
                    .await
                    .unwrap(),
                RunState::Running
            );
            assert_eq!(
                *calls.lock().unwrap(),
                if allow {
                    vec![call.clone()]
                } else {
                    Vec::new()
                }
            );
            assert!(commit.resume_ticket(&run_id).is_none());
            let results = commit.committed_messages(&thread_id);
            let result = results
                .iter()
                .find(|message| message.id.is_tool_result_for_call(&call.call_id))
                .unwrap();
            assert!(
                matches!(&result.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. }
                if tool_use_id == &call.call_id && *is_error != allow)
            );
            let count = commit.commit_count();
            assert_eq!(
                continuation.recover(activation, context).await.unwrap(),
                RunState::Running
            );
            assert_eq!(commit.commit_count(), count);
            let guarded = PermissionGuardedAcpTool::new(tool, Arc::new(AskPolicy), exports.clone());
            assert!(
                guarded
                    .invoke(call)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("TOOL_APPROVAL_REQUIRED")
            );
            assert!(exports.pending.lock().unwrap().is_empty());
            assert_eq!(calls.lock().unwrap().len(), usize::from(allow));
        }
    }
}

#[cfg(test)]
mod environment_tool_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Executor(AtomicUsize);
    #[async_trait::async_trait]
    impl awaken_tool_contract::tool::ToolExecutor for Executor {
        fn concurrency(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> awaken_tool_contract::tool::ToolConcurrency {
            awaken_tool_contract::tool::ToolConcurrency::Serial
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
            awaken_tool_contract::tool::ToolConcurrency::Serial
        ));
        tool.invoke(call.clone()).await.unwrap();
        call.tool_id = "bash".into();
        assert!(tool.invoke(call).await.is_err());
        assert_eq!(executor.0.load(Ordering::SeqCst), 1);
    }
}
