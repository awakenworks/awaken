// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The Session Host owns one attached helper and MCP export lease. It revokes admission
// @formal reason: before TERM/KILL/reap, retains handles across cancellation and leaves final physical
// @formal reason: disposal to the Session Environment. Bytes do not grant tool authority.
//! Attempt-local MCP bytes over the Session's existing attached process channel.
//! Tool execution and approval remain in the original guarded export set.

use super::*;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub(super) struct AttachedTools {
    pub(super) server: awaken_acp_contract::SessionMcpServer,
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
    // One canonical signal/observation ladder owns both ACP and attached MCP
    // process cleanup. Its typed entrypoint retains the provider's failure
    // class for the Session Environment; no parallel TERM/KILL loop lives here.
    awaken_run_executor_acp::Supervisor::reap_process(process, Duration::from_secs(5)).await?;
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
        sandbox: &dyn awaken_session_environment_contract::AgentProcessEnvironment,
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
        let server = awaken_acp_contract::SessionMcpServer {
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
        let result = match tools.exporter.attached_channels() {
            Some(exporter) => {
                exporter
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
                    .await
            }
            None => Err("ACP tool exporter does not support attached channels".to_string()),
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct RecoverableHelper {
        may_exit: AtomicBool,
        signals: Mutex<Vec<pc::Signal>>,
    }

    #[async_trait::async_trait]
    impl pc::ProcessHandle for RecoverableHelper {
        fn id(&self) -> &str {
            "attached-helper"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            if self.may_exit.load(Ordering::SeqCst) {
                Ok(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                })
            } else {
                std::future::pending().await
            }
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(self
                .may_exit
                .load(Ordering::SeqCst)
                .then_some(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                }))
        }

        async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
            self.signals.lock().unwrap().push(signal);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unobserved_helper_exit_retains_owner_for_the_same_handle_retry() {
        // Causes: C1 export is already revoked; C2 TERM and KILL receive no
        // exit receipt; C3 a later observation proves that exact handle exited.
        // Effects: E1 C2 returns typed Timeout while `stopped` stays false and
        // the same process Arc remains owned; E2 C3 marks stopped without a new
        // signal, process, export, or Session authority. Decision table:
        // R1 C1+C2 -> indeterminate cleanup, retain; R2 C1+C3 -> exact retry
        // succeeds; R3 already stopped -> idempotent no-op.
        let process = Arc::new(RecoverableHelper::default());
        let state = Arc::new(tokio::sync::Mutex::new(State::default()));
        let attached = AttachedTools {
            server: awaken_acp_contract::SessionMcpServer {
                name: "awaken_session".into(),
                command: Some("helper".into()),
                args: Vec::new(),
                url: None,
                auth: None,
            },
            process: process.clone(),
            state: state.clone(),
        };

        let error = attached.close().await.expect_err("exit was not observed");
        assert!(matches!(error, pc::SandboxError::Timeout(_)));
        assert!(!state.lock().await.stopped);
        assert!(
            Arc::strong_count(&process) >= 2,
            "the same handle is retained"
        );
        assert_eq!(
            process.signals.lock().unwrap().as_slice(),
            &[pc::Signal::Term, pc::Signal::Kill],
        );

        process.may_exit.store(true, Ordering::SeqCst);
        attached.close().await.expect("same-handle exit receipt");
        attached
            .close()
            .await
            .expect("terminal cleanup is idempotent");
        assert!(state.lock().await.stopped);
        assert_eq!(process.signals.lock().unwrap().len(), 2);
    }
}
