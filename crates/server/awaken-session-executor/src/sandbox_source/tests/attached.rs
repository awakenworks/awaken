//! Lifecycle decision rows through the existing production channel source.

use super::*;
use crate::session_environment::AgentSandbox;
use awaken_runtime_contract::tool_export::{AcpToolExport, AcpToolExporter};
use pc::ProcessHandle as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

struct Process {
    id: &'static str,
    finished: tokio::sync::watch::Sender<bool>,
    signals: Mutex<Vec<pc::Signal>>,
    ignore_term: bool,
}

impl Process {
    fn new(id: &'static str, ignore_term: bool) -> Arc<Self> {
        Arc::new(Self {
            id,
            finished: tokio::sync::watch::channel(false).0,
            signals: Mutex::new(vec![]),
            ignore_term,
        })
    }
}

struct Handle(Arc<Process>);

#[async_trait]
impl pc::ProcessHandle for Handle {
    fn id(&self) -> &str {
        self.0.id
    }
    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let mut finished = self.0.finished.subscribe();
        while !*finished.borrow_and_update() {
            finished.changed().await.unwrap();
        }
        Ok(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }
    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let finished = *self.0.finished.borrow();
        Ok(if finished {
            Some(self.wait().await?)
        } else {
            None
        })
    }
    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        self.0.signals.lock().unwrap().push(signal);
        if signal == pc::Signal::Kill || (signal == pc::Signal::Term && !self.0.ignore_term) {
            self.0.finished.send_replace(true);
        }
        Ok(())
    }
}

struct Sandbox {
    base: CapturingAgentSandbox,
    helper: Arc<Process>,
    primary: Arc<Process>,
    commands: Mutex<Vec<pc::Command>>,
    readiness: Option<&'static [u8]>,
    primary_fails: bool,
}

impl Sandbox {
    fn new(readiness: Option<&'static [u8]>, primary_fails: bool, ignore_term: bool) -> Arc<Self> {
        Arc::new(Self {
            base: CapturingAgentSandbox::default(),
            helper: Process::new("helper", ignore_term),
            primary: Process::new("primary", false),
            commands: Mutex::new(vec![]),
            readiness,
            primary_fails,
        })
    }
}

#[async_trait]
impl AgentSandbox for Sandbox {
    fn is_container(&self) -> bool {
        true
    }
    fn channel_helper(&self) -> Option<String> {
        Some("/sandbox-helper".into())
    }
    fn supports_host_identity(&self) -> bool {
        false
    }
    fn config_home(&self) -> String {
        self.base.config_home()
    }
    fn config_home_logical(&self) -> String {
        self.base.config_home_logical()
    }
    fn workspace_cwd(&self) -> String {
        self.base.workspace_cwd()
    }
    async fn materialize_inline(&self, path: &str, bytes: &[u8]) -> Result<(), pc::SandboxError> {
        self.base.materialize_inline(path, bytes).await
    }
    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        let helper = command
            .argv
            .get(1)
            .is_some_and(|arg| arg == "channel-listen");
        self.commands.lock().unwrap().push(command);
        if !helper && self.primary_fails {
            return Err(pc::SandboxError::backend(
                "spawn_agent",
                "primary fixture failure",
            ));
        }
        let process = if helper {
            self.helper.clone()
        } else {
            self.primary.clone()
        };
        let (channel, mut peer) = tokio::io::duplex(4096);
        let ready = self.readiness;
        let lifetime = process.clone();
        tokio::spawn(async move {
            if helper && let Some(ready) = ready {
                let _ = peer.write_all(ready).await;
            }
            let _ = Handle(lifetime).wait().await;
            drop(peer);
        });
        Ok((Box::new(Handle(process)), Box::new(channel)))
    }
}

struct Exporter {
    calls: AtomicUsize,
    drops: Arc<AtomicUsize>,
    fail: bool,
}
struct Lease {
    drops: Arc<AtomicUsize>,
    _io: tokio::sync::Mutex<(
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    )>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl AcpToolExporter for Exporter {
    async fn export_set(
        &self,
        _: &str,
        _: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        _: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Result<AcpToolExport, String> {
        panic!("attached tools must never export a Worker loopback endpoint")
    }

    fn attached_channels(
        &self,
    ) -> Option<&dyn awaken_runtime_contract::tool_export::AttachedChannelToolExporter> {
        Some(self)
    }
}

#[async_trait]
impl awaken_runtime_contract::tool_export::AttachedChannelToolExporter for Exporter {
    async fn export_channel_set(
        &self,
        server: awaken_runtime_contract::resolved::AcpMcpServer,
        _: Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
        _: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) -> Result<AcpToolExport, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err("export fixture failure".into());
        }
        Ok(AcpToolExport::new(
            server,
            Lease {
                drops: self.drops.clone(),
                _io: tokio::sync::Mutex::new((reader, writer)),
            },
        ))
    }
}

fn source(sandbox: Arc<Sandbox>, fail: bool) -> (BoundLocalChannelSource, Arc<Exporter>) {
    let exporter = Arc::new(Exporter {
        calls: AtomicUsize::new(0),
        drops: Arc::new(AtomicUsize::new(0)),
        fail,
    });
    let tools = Arc::new(crate::acp_tool_export::AttachedToolSet {
        exporter: exporter.clone(),
        descriptors: vec![],
        executors: vec![],
    });
    (
        bound_projecting_source(sandbox, "claude").with_attached_tools(Some(tools)),
        exporter,
    )
}

#[tokio::test]
async fn source_binds_the_exact_socket_and_reaps_tools_once_after_primary_exit() {
    // Graph: C1 exact readiness -> export -> primary launch; C2 primary TERM ->
    // keep helper available for SDK persistence; C3 primary wait/poll terminal
    // -> revoke export, TERM helper, reap once. C4 a second open -> fresh socket.
    // Rows A1(C1) projection points to the same helper/path used by spawn;
    // A2(C2) no premature export drop; A3(C3 repeated) one lease drop/cleanup;
    // A4(C4) no reuse of a prior attempt's rendezvous.
    let mut sockets = vec![];
    for poll in [false, true] {
        let sandbox = Sandbox::new(
            Some(awaken_connection_plan::socket_stdio::READY),
            false,
            false,
        );
        let (source, exporter) = source(sandbox.clone(), false);
        let session = awaken_run_executor_acp::open_agent_session(
            &source,
            &acp_activation("acp:claude"),
            &awaken_runtime_contract::RuntimeRunContext::new(),
        )
        .await
        .unwrap();
        let server = &session.mcp_session_servers[0];
        assert_eq!(server.command.as_deref(), Some("/sandbox-helper"));
        {
            let commands = sandbox.commands.lock().unwrap();
            assert_eq!(commands.len(), 2);
            assert_eq!(commands[0].argv[3], server.args[2]);
            assert_eq!(server.args[0], "channel-connect");
            sockets.push(server.args[2].clone());
        }

        assert!(session.process.poll().await.unwrap().is_none());
        session.process.signal(pc::Signal::Term).await.unwrap();
        assert!(sandbox.helper.signals.lock().unwrap().is_empty());
        assert_eq!(exporter.drops.load(Ordering::SeqCst), 0);
        if poll {
            assert!(session.process.poll().await.unwrap().is_some());
        } else {
            session.process.wait().await.unwrap();
        }
        session.process.wait().await.unwrap();
        assert_eq!(*sandbox.helper.signals.lock().unwrap(), [pc::Signal::Term]);
        assert_eq!(exporter.drops.load(Ordering::SeqCst), 1);
    }
    assert_ne!(sockets[0], sockets[1]);
}

#[tokio::test(start_paused = true)]
async fn startup_failure_and_cancellation_reap_without_starting_or_leaking_primary() {
    // Decision table A5 malformed readiness, A6 no readiness, A7 export failure,
    // A8 primary spawn failure, A9 cancellation during readiness, A15 old raw
    // wire preamble after upgrading Host. All revoke
    // the owned helper. Only A8 reaches primary spawn; only A8 creates a lease.
    for row in [5, 6, 7, 8, 9, 15] {
        let ready = match row {
            5 => Some(b"wrong readiness contents\n".as_slice()),
            15 => Some(b"AWAKEN_SOCKET_STDIO_READY\n".as_slice()),
            6 | 9 => None,
            _ => Some(awaken_connection_plan::socket_stdio::READY),
        };
        let sandbox = Sandbox::new(ready, row == 8, false);
        let (source, exporter) = source(sandbox.clone(), row == 7);
        let task = tokio::spawn(async move {
            awaken_run_executor_acp::open_agent_session(
                &source,
                &acp_activation("acp:claude"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
        });
        if row == 9 {
            while sandbox.commands.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        } else {
            assert!(task.await.unwrap().is_err(), "A{row}");
        }
        tokio::time::timeout(
            Duration::from_secs(2),
            Handle(sandbox.helper.clone()).wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            sandbox.commands.lock().unwrap().len(),
            if row == 8 { 2 } else { 1 }
        );
        assert_eq!(
            exporter.calls.load(Ordering::SeqCst),
            usize::from(row == 7 || row == 8)
        );
        assert_eq!(exporter.drops.load(Ordering::SeqCst), usize::from(row == 8));
    }
}

#[tokio::test(start_paused = true)]
async fn forced_stop_escalates_an_unresponsive_helper_and_duplicate_names_fail_before_spawn() {
    // A10 primary Kill + helper ignores TERM -> bounded grace then Kill/reap;
    // export revocation occurs once. A11 reserved server name collision -> no
    // helper, export or primary launch, rather than ambiguous tool ownership.
    let sandbox = Sandbox::new(
        Some(awaken_connection_plan::socket_stdio::READY),
        false,
        true,
    );
    let (source, exporter) = source(sandbox.clone(), false);
    let session = awaken_run_executor_acp::open_agent_session(
        &source,
        &acp_activation("acp:claude"),
        &awaken_runtime_contract::RuntimeRunContext::new(),
    )
    .await
    .unwrap();
    session.process.signal(pc::Signal::Kill).await.unwrap();
    assert_eq!(
        *sandbox.helper.signals.lock().unwrap(),
        [pc::Signal::Term, pc::Signal::Kill]
    );
    assert_eq!(exporter.drops.load(Ordering::SeqCst), 1);
    let sandbox = Sandbox::new(None, false, false);
    let (mut source, exporter) = self::source(sandbox.clone(), false);
    source
        .mcp_servers
        .push(awaken_run_executor_acp::SessionMcpServer {
            name: "awaken_session".into(),
            command: Some("existing".into()),
            args: vec![],
            url: None,
            auth: None,
        });
    assert!(
        awaken_run_executor_acp::open_agent_session(
            &source,
            &acp_activation("acp:claude"),
            &awaken_runtime_contract::RuntimeRunContext::new()
        )
        .await
        .is_err()
    );
    assert!(sandbox.commands.lock().unwrap().is_empty());
    assert_eq!(exporter.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unsupported_environment_adapter_and_exporter_fail_closed() {
    // A12 missing Environment helper -> no process/export. A13 no catalog ACP
    // adapter -> no attached process. A14 legacy HTTP-only exporter -> default
    // port rejects attached I/O and reaps helper, never starts the primary or
    // falls back to an inaccessible Worker HTTP URL.
    let exporter = Arc::new(crate::acp_tool_export::RecordingAcpToolExporter::default());
    let tools = Arc::new(crate::acp_tool_export::AttachedToolSet {
        exporter: exporter.clone(),
        descriptors: vec![],
        executors: vec![],
    });
    let sandbox = Arc::new(CapturingAgentSandbox::default());
    let source =
        bound_projecting_source(sandbox.clone(), "claude").with_attached_tools(Some(tools.clone()));
    let result = awaken_run_executor_acp::open_agent_session(
        &source,
        &acp_activation("acp:claude"),
        &awaken_runtime_contract::RuntimeRunContext::new(),
    )
    .await;
    assert!(
        matches!(result, Err(error) if error.to_string().contains("no attached channel helper"))
    );
    assert!(sandbox.command.lock().unwrap().is_none());

    let sandbox = Sandbox::new(None, false, false);
    let (mut source, _) = self::source(sandbox.clone(), false);
    source.launch =
        LaunchSource::Fixed(Box::new(AcpLaunch::custom(vec!["fixture".into()], vec![])));
    let result = awaken_run_executor_acp::open_agent_session(
        &source,
        &acp_activation("acp:claude"),
        &awaken_runtime_contract::RuntimeRunContext::new(),
    )
    .await;
    assert!(matches!(result, Err(error) if error.to_string().contains("catalog ACP adapter")));
    assert!(sandbox.commands.lock().unwrap().is_empty());

    let sandbox = Sandbox::new(
        Some(awaken_connection_plan::socket_stdio::READY),
        false,
        false,
    );
    let source =
        bound_projecting_source(sandbox.clone(), "claude").with_attached_tools(Some(tools));
    let result = awaken_run_executor_acp::open_agent_session(
        &source,
        &acp_activation("acp:claude"),
        &awaken_runtime_contract::RuntimeRunContext::new(),
    )
    .await;
    assert!(
        matches!(result, Err(error) if error.to_string().contains("does not support attached channels"))
    );
    assert_eq!(sandbox.commands.lock().unwrap().len(), 1);
    assert!(*sandbox.helper.finished.borrow());
    assert!(exporter.0.lock().unwrap().is_empty());
}
