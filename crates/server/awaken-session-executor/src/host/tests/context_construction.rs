//! Construction failure/cancellation tests through the real Session entry point.
//! Scenarios RS-REC-002/003/004 in docs/design/runtime-scenario-validation.md.
use super::*;
use awaken_session_environment_contract::tool_export::{AcpToolExport, AcpToolExporter};
use awaken_session_environment_contract::{AcpSessionExecution, AcpSessionExecutorFactory};

#[tokio::test]
async fn failed_context_assembly_retains_one_environment_for_retry() {
    // C1 Environment acquisition succeeds; C2 later compact configuration is
    // invalid; C3 the same request retries. E1 no incomplete Runtime is cached;
    // E2 the lifecycle owner retains the same Environment. R1=C1+C2=>E1+E2;
    // R2=C1+C2+C3=>E1+E2 without another acquisition. Configuration stays
    // unchanged; successful retry/cache reuse is covered by the ACP fault cases.
    let mut host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    let id = awaken_ext_compact::COMPACT_PLUGIN_ID.to_string();
    host.tools.plugin_ids.push(id.clone());
    host.tools
        .plugin_config
        .insert(id.clone(), serde_json::json!({"keep_last": "invalid"}));
    let (host, _managed) = dispatch_test_host((host).build().expect("validated Host dependencies"));
    let thread = "failed-context-assembly";
    let error = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .err()
        .expect("R1 must fail");
    assert!(error.message.contains("invalid host compact plugin config"));
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_none())
            .expect("installed Sessions")
            .unwrap()
    );
    let retained = host
        .realization_executor
        .environments()
        .session_environment(thread)
        .await
        .expect("installed Sessions")
        .expect("E2 owner retained");

    let repeated = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .err()
        .expect("R2 must fail");
    assert!(
        repeated
            .message
            .contains("invalid host compact plugin config")
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_none())
            .expect("installed Sessions")
            .unwrap()
    );
    assert!(
        Arc::ptr_eq(
            &host
                .realization_executor
                .environments()
                .session_environment(thread)
                .await
                .expect("installed Sessions")
                .unwrap(),
            &retained
        ),
        "E2 exact owner"
    );
}

#[derive(Clone, Copy)]
enum FirstExport {
    Succeed,
    Fail,
    Block,
}

struct ExportLifetime(Arc<AtomicUsize>);

impl Drop for ExportLifetime {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct FaultExporter {
    first: FirstExport,
    calls: AtomicUsize,
    leases_dropped: Arc<AtomicUsize>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[async_trait::async_trait]
impl AcpToolExporter for FaultExporter {
    async fn export_set(
        &self,
        server_name: &str,
        descriptors: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String> {
        assert_eq!(descriptors.len(), tools.len());
        assert!(
            descriptors
                .iter()
                .any(|descriptor| descriptor.id == "desktop_search")
        );
        assert_eq!(
            descriptors
                .iter()
                .map(|descriptor| descriptor.id.as_str())
                .collect::<Vec<_>>(),
            tools.iter().map(|tool| tool.id()).collect::<Vec<_>>(),
            "export preserves exact descriptor/executor pairing",
        );
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first && matches!(self.first, FirstExport::Fail) {
            return Err("injected export failure".into());
        }
        let lease = ExportLifetime(self.leases_dropped.clone());
        if first && matches!(self.first, FirstExport::Block) {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            // awaken-allow(wait): force cancellation at the production export await,
            // with a partial transport lease already owned by this future.
            std::future::pending::<()>().await;
        }
        Ok(AcpToolExport::new(
            awaken_run_executor_acp::McpServerConfig {
                name: server_name.into(),
                transport: awaken_run_executor_acp::McpTransport::Http {
                    url: "http://127.0.0.1/context-fixture".into(),
                },
            },
            lease,
        ))
    }
}

struct FaultFactory {
    inner: Arc<dyn AcpSessionExecutorFactory>,
    fail_first: AtomicBool,
}

impl AcpSessionExecutorFactory for FaultFactory {
    fn create(
        &self,
        request: AcpSessionExecution,
    ) -> Result<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>, String> {
        if self.fail_first.swap(false, Ordering::SeqCst) {
            return Err("injected executor assembly failure".into());
        }
        self.inner.create(request)
    }

    fn adapter_policy(
        &self,
    ) -> &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    > {
        self.inner.adapter_policy()
    }

    fn credential_realization_capabilities(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, String> {
        self.inner.credential_realization_capabilities(backend)
    }
}

async fn acp_fixture(
    root: &std::path::Path,
    first: FirstExport,
    fail_factory: bool,
) -> (
    Arc<ExecutorComponents>,
    Arc<ExecutorComponents>,
    Arc<FaultExporter>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (entered, receiver) = tokio::sync::oneshot::channel();
    let exporter = Arc::new(FaultExporter {
        first,
        calls: AtomicUsize::new(0),
        leases_dropped: Arc::new(AtomicUsize::new(0)),
        entered: Mutex::new(Some(entered)),
    });
    let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        awaken_runtime_contract::resolved::ModelBinding::parse("local", "", "acp:codex").unwrap(),
        awaken_runtime_contract::CredentialRef {
            id: "fixture-login".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "codex-test",
        "sha256:codex-test",
        Default::default(),
    )
    .unwrap();
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(candidate)
        .tool(crate::config::client_tool_descriptor("desktop_search"))
        .build();
    let source =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot]).unwrap();
    let mut host = crate::acp_backend::with_fixture_acp(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_agent_publications(Arc::new(source))
            .with_acp_tool_exporter(exporter.clone()),
    );
    host.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::workdir_with_agent_stderr(
            root, false,
        )
        .into_session_provider(),
    );
    let factory = Arc::new(FaultFactory {
        inner: host.exec.acp.as_ref().unwrap().clone(),
        fail_first: AtomicBool::new(fail_factory),
    });
    let (host, managed) = dispatch_test_host(
        (host.with_acp(factory))
            .build()
            .expect("validated Host dependencies"),
    );
    (host, managed, exporter, receiver)
}

#[tokio::test]
async fn acp_context_failure_releases_exports_and_reuses_environment() {
    // C1 export fails before returning a lease; C2 executor assembly fails
    // after export; C3 retry succeeds. E1 no partial Runtime publication;
    // E2 returned leases are dropped on failure; E3 exactly one Environment
    // is reused; E4 successful export stays alive with the cached context.
    // Decision table: R1=C1=>E1+E3, zero returned leases;
    // R2=!C1+C2=>E1+E2+E3; R3=retry=>E3+E4; R4=cache hit=>no extra export.
    // The canonical ACP factory is delegated to without launching a process;
    // this tests Host ownership, not backend handshake/qualification.
    for (first, fail_factory, message, failed_drops) in [
        (FirstExport::Fail, false, "injected export failure", 0),
        (
            FirstExport::Succeed,
            true,
            "injected executor assembly failure",
            1,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (host, _managed, exporter, _entered) =
            acp_fixture(root.path(), first, fail_factory).await;
        let thread = "acp-context-failure";
        let error = host
            .run_assembly
            .ctx_for(thread, Some("assistant"))
            .await
            .err()
            .expect("injected failure");
        assert!(error.message.contains(message), "{error}");
        assert!(
            host.run_assembly
                .installed_sessions
                .read_run(thread, |run| run.context.is_none())
                .expect("installed Sessions")
                .unwrap(),
            "E1"
        );
        let retained = host
            .realization_executor
            .environments()
            .session_environment(thread)
            .await
            .expect("installed Sessions")
            .expect("E3 retained owner");
        assert_eq!(
            exporter.leases_dropped.load(Ordering::SeqCst),
            failed_drops,
            "E2"
        );
        let complete = host
            .run_assembly
            .ctx_for(thread, Some("assistant"))
            .await
            .expect("R3 retry");
        assert!(Arc::ptr_eq(complete.env.as_ref().unwrap(), &retained), "E3");
        assert_eq!(exporter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            exporter.leases_dropped.load(Ordering::SeqCst),
            failed_drops,
            "E4"
        );
        let cached = host
            .run_assembly
            .ctx_for(thread, Some("assistant"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&complete, &cached), "R4");
        assert_eq!(exporter.calls.load(Ordering::SeqCst), 2, "R4");
    }
}

#[tokio::test]
async fn cancelled_context_construction_releases_lock_and_partial_export() {
    // C1 Environment acquired; C2 export owns a lease and is suspended; C3
    // caller drops the construction future. E1 no partial Runtime cache;
    // E2 the partial lease is released; E3 the lifecycle lock is released;
    // E4 retry completes with the exact retained Environment and export lease.
    // R1=C1+C2+C3=>E1+E2+E3; R2=retry=>E4. A retained oneshot signal
    // proves the export boundary was reached; no scheduler sleeps/poll counts.
    let root = tempfile::tempdir().unwrap();
    let (host, _managed, exporter, entered) =
        acp_fixture(root.path(), FirstExport::Block, false).await;
    let thread = "cancelled-context-construction";
    let mut open = Box::pin(host.run_assembly.ctx_for(thread, Some("assistant")));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut open => panic!("must suspend during export: {:?}", result.err()),
            result = entered => result.expect("export reached"),
        }
    })
    .await
    .expect("bounded export barrier");
    let retained = host
        .realization_executor
        .environments()
        .session_environment(thread)
        .await
        .expect("installed Sessions")
        .expect("C1 resident owner");
    drop(open);
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_none())
            .expect("installed Sessions")
            .unwrap(),
        "E1"
    );
    assert_eq!(exporter.leases_dropped.load(Ordering::SeqCst), 1, "E2");
    let complete = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        host.run_assembly.ctx_for(thread, Some("assistant")),
    )
    .await
    .expect("E3 lock released")
    .expect("E4 retry succeeds");
    assert!(
        Arc::ptr_eq(complete.env.as_ref().unwrap(), &retained),
        "E4 exact owner"
    );
    assert_eq!(exporter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        exporter.leases_dropped.load(Ordering::SeqCst),
        1,
        "E4 live lease"
    );
}
