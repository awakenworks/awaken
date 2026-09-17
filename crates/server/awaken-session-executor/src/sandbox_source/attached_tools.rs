//! Attempt-local MCP bytes over the Session's existing attached process channel.
//! Tool execution and approval remain in the original guarded export set.

use super::*;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub(super) struct AttachedTools {
    pub(super) server: awaken_run_executor_acp::SessionMcpServer,
    process: Arc<dyn pc::ProcessHandle>,
    state: Arc<tokio::sync::Mutex<State>>,
}

#[derive(Default)]
struct State {
    export: Option<awaken_runtime_contract::tool_export::AcpToolExport>,
    stopped: bool,
}

async fn close(
    process: &dyn pc::ProcessHandle,
    state: &tokio::sync::Mutex<State>,
) -> Result<(), pc::SandboxError> {
    let mut state = state.lock().await;
    if state.stopped {
        return Ok(());
    }
    // Revoke MCP admission first, then let the helper unlink its rendezvous.
    // This does not infer that an already dispatched tool had no side effect.
    state.export.take();
    process.signal(pc::Signal::Term).await?;
    match tokio::time::timeout(Duration::from_secs(5), process.wait()).await {
        Ok(status) => {
            status?;
        }
        Err(_) => {
            process.signal(pc::Signal::Kill).await?;
            tokio::time::timeout(Duration::from_secs(5), process.wait())
                .await
                .map_err(|_| {
                    pc::SandboxError::timeout("attached MCP process cleanup timed out")
                })??;
        }
    }
    state.stopped = true;
    Ok(())
}

impl Drop for AttachedTools {
    fn drop(&mut self) {
        // Cancellation of source.open can occur between any two awaits. Retain
        // the process handle and export until the same cleanup finishes; the
        // Session Environment remains the ultimate physical teardown authority.
        let process = self.process.clone();
        let state = self.state.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { if let Err(error) = close(process.as_ref(), &state).await { tracing::warn!(%error, "attached MCP cleanup failed; Environment disposal remains required"); } });
        }
    }
}

impl AttachedTools {
    pub(super) async fn close(&self) -> Result<(), pc::SandboxError> {
        close(self.process.as_ref(), &self.state).await
    }

    pub(super) async fn open(
        sandbox: &dyn crate::session_environment::AgentSandbox,
        tools: &crate::acp_tool_export::AttachedToolSet,
    ) -> Result<Self, OpenError> {
        let helper = sandbox
            .channel_helper()
            .ok_or_else(|| OpenError("Environment has no attached channel helper".into()))?;
        let socket = format!("/tmp/awaken-mcp-{}.sock", uuid::Uuid::new_v4().simple());
        let (process, mut channel) = sandbox
            .spawn_agent(pc::Command {
                argv: vec![
                    helper.clone(),
                    "channel-listen".into(),
                    "--unix".into(),
                    socket.clone(),
                ],
                cwd: String::new(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await
            .map_err(|error| OpenError(format!("attached MCP channel launch: {error}")))?;
        let server = awaken_run_executor_acp::SessionMcpServer {
            name: "awaken_session".into(),
            command: Some(helper.clone()),
            args: vec!["channel-connect".into(), "--unix".into(), socket],
            url: None,
            auth: None,
        };
        let attached = Self {
            server,
            process: Arc::from(process),
            state: Arc::new(tokio::sync::Mutex::new(State::default())),
        };
        let ready = awaken_connection_plan::socket_stdio::READY;
        let mut received = vec![0; ready.len()];
        let readiness =
            tokio::time::timeout(Duration::from_secs(10), channel.read_exact(&mut received)).await;
        if !matches!(readiness, Ok(Ok(_))) || received != ready {
            attached
                .close()
                .await
                .map_err(|error| OpenError(error.to_string()))?;
            return Err(OpenError(
                "attached MCP channel did not report exact readiness".into(),
            ));
        }
        let (reader, writer) = tokio::io::split(channel);
        let result = tools
            .exporter
            .export_channel_set(
                awaken_runtime_contract::resolved::AcpMcpServer {
                    name: attached.server.name.clone(),
                    transport: awaken_runtime_contract::resolved::AcpMcpTransport::Stdio {
                        command: helper,
                        args: attached.server.args.clone(),
                    },
                },
                tools.descriptors.clone(),
                tools.executors.clone(),
                Box::new(reader),
                Box::new(writer),
            )
            .await;
        match result {
            Ok(export) => attached.state.lock().await.export = Some(export),
            Err(error) => {
                attached
                    .close()
                    .await
                    .map_err(|error| OpenError(error.to_string()))?;
                return Err(OpenError(format!("attached MCP export: {error}")));
            }
        }
        Ok(attached)
    }
}

pub(super) struct ProcessWithTools {
    pub(super) process: Box<dyn pc::ProcessHandle>,
    pub(super) attached: AttachedTools,
}

#[async_trait]
impl pc::ProcessHandle for ProcessWithTools {
    fn id(&self) -> &str {
        self.process.id()
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let status = self.process.wait().await?;
        self.attached.close().await?;
        Ok(status)
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let status = self.process.poll().await?;
        if status.is_some() {
            self.attached.close().await?;
        }
        Ok(status)
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        // Graceful ACP shutdown keeps MCP alive until the SDK has finished
        // persisting. A forced kill may revoke the companion immediately.
        self.process.signal(signal).await?;
        if signal == pc::Signal::Kill {
            self.attached.close().await?;
        }
        Ok(())
    }
}
