//! Optional physical execution boundary on the existing Local spawn seam.
//! Native Hand, ACP and sandbox-stdio MCP all use that seam. Durable evidence
//! lives in the realization marker; weak handles here only deliver live cutoff
//! refresh/stop requests and can never reconstruct authorization after a crash.

use super::*;
use awaken_local_process::scoped::{ExecutionDeadline, ScopedCommand, ScopedProcess, ScopedStdio};
use pc::ProcessHandle as _;
use std::{
    io,
    path::Path,
    sync::{Mutex, Weak},
    time::Duration,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

use pc::CurrentExecutionAuthority;

#[derive(Default)]
struct LiveExecution {
    closed: bool,
    processes: Vec<Weak<ScopedProcess>>,
}

pub(super) struct ScopedExecution {
    launcher: PathBuf,
    helper: PathBuf,
    authority: Arc<dyn CurrentExecutionAuthority>,
    live: Mutex<LiveExecution>,
}

pub(super) enum ExecutionBinding {
    Unsupervised,
    Scoped(Box<ScopedExecution>),
}

fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

impl LocalSandbox {
    /// Composition installs this on an exact current TrustedHost realization
    /// before its first spawn. This does not advertise a Worker capability:
    /// probe, Session binding, renewal and recovery must all be installed there.
    pub fn install_scoped_execution(
        &self,
        launcher: &Path,
        helper: &Path,
        authority: Arc<dyn CurrentExecutionAuthority>,
    ) -> Result<(), pc::SandboxError> {
        if !self.trusted_host_access
            || self.realization.current().is_none()
            || !launcher.is_absolute()
            || !helper.is_absolute()
        {
            return Err(err(
                "scoped execution requires an exact TrustedHost realization and absolute deployment binaries",
            ));
        }
        self.execution
            .set(ExecutionBinding::Scoped(Box::new(ScopedExecution {
                launcher: launcher.into(),
                helper: helper.into(),
                authority,
                live: Mutex::new(LiveExecution::default()),
            })))
            .map_err(|_| err("execution boundary is already selected; late upgrade is forbidden"))
    }

    /// Entry for the EXISTING Session renewal scheduler; composition must wire
    /// it explicitly. This provider method never starts a timer.
    pub async fn refresh_scoped_execution(&self) -> Result<(), pc::SandboxError> {
        if let Some(ExecutionBinding::Scoped(execution)) = self.execution.get() {
            for process in execution.live_processes()? {
                if process.poll().await?.is_some() {
                    continue;
                }
                let refresh = process
                    .refresh_deadline_authorized(CONTROL_TIMEOUT, |grant| {
                        let mut grant = Some(grant);
                        execution.authority.with_current(&mut |remaining| {
                            let live = execution.live.lock().map_err(|_| denied())?;
                            if live.closed {
                                return Err(denied());
                            }
                            grant
                                .take()
                                .ok_or_else(denied)?
                                .commit(ExecutionDeadline::after(remaining)?)
                        })
                    })
                    .await;
                if let Err(error) = refresh {
                    // The exact tree can exit after the preflight poll, including
                    // while current authority is being checked. Only a fresh
                    // complete ProcessHandle receipt makes that race benign;
                    // channel failure or leader exit alone cannot do so. This
                    // is a skipped renewal, never a Session stop receipt.
                    if process.poll().await?.is_none() {
                        return Err(err(error));
                    }
                }
            }
        }
        Ok(())
    }

    /// Irreversibly close admission, request exact-scope KILL, then require the
    /// existing marker to prove all participants absent. A launch still holding
    /// that lock yields a retryable error, never a premature quiescence receipt.
    /// Returns whether this realization has an installed scoped boundary. An
    /// unconfigured provider does not manufacture an OS tree-stop receipt.
    pub async fn stop_scoped_execution(&self) -> Result<bool, pc::SandboxError> {
        let Some(ExecutionBinding::Scoped(execution)) = self.execution.get() else {
            return Ok(false);
        };
        execution.live.lock().map_err(err)?.closed = true;
        let processes = execution.live_processes()?;
        for process in &processes {
            process.signal(pc::Signal::Kill).await?;
        }
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            for process in &processes {
                process.wait().await?;
            }
            Ok::<_, pc::SandboxError>(())
        })
        .await
        .map_err(|_| err("execution tree stop is still unconfirmed"))??;
        let evidence = self
            .realization
            .current()
            .ok_or_else(|| err("scoped realization evidence missing"))?;
        let removal = self.terminal_removal.lock().map_err(err)?;
        if let Some(removal) = removal.as_ref() {
            removal.verify_execution_quiescence()?;
        } else {
            crate::realization_marker::verify_execution_quiescence(self.root.root(), evidence)?;
        }
        Ok(true)
    }
}

impl ScopedExecution {
    fn live_processes(&self) -> Result<Vec<Arc<ScopedProcess>>, pc::SandboxError> {
        let mut live = self.live.lock().map_err(err)?;
        let processes = live
            .processes
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        live.processes.retain(|process| process.strong_count() > 0);
        Ok(processes)
    }

    pub(super) async fn spawn(
        &self,
        sandbox: &LocalSandbox,
        command: pc::Command,
        stdio: ScopedStdio,
    ) -> Result<ScopedProcess, pc::SandboxError> {
        let evidence = sandbox
            .realization
            .current()
            .ok_or_else(|| err("scoped realization evidence missing"))?;
        let mut guard = crate::realization_marker::begin_execution(sandbox.root.root(), evidence)?;
        let mut deadline = None;
        self.authority
            .with_current(&mut |remaining| {
                if self.live.lock().map_err(|_| denied())?.closed || deadline.is_some() {
                    return Err(denied());
                }
                deadline = Some(ExecutionDeadline::after(remaining)?);
                Ok(())
            })
            .map_err(err)?;
        let deadline =
            deadline.ok_or_else(|| err("current authority omitted execution deadline"))?;
        let prepared = sandbox.build_command(command).await?;
        ScopedProcess::spawn_authorized(
            &self.launcher,
            &self.helper,
            ScopedCommand {
                materialized: &prepared.command,
                cwd: &prepared.cwd,
                stdio,
            },
            deadline,
            CONTROL_TIMEOUT,
            |identity, grant| {
                let mut grant = Some(grant);
                self.authority.with_current(&mut |_| {
                    let live = self.live.lock().map_err(|_| denied())?;
                    if live.closed {
                        return Err(denied());
                    }
                    guard.record_execution(identity).map_err(io::Error::other)?;
                    grant.take().ok_or_else(denied)?.commit()
                })
            },
        )
        .await
        .map_err(err)
    }

    pub(super) fn track(
        &self,
        process: ScopedProcess,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let mut live = self.live.lock().map_err(err)?;
        if live.closed {
            return Err(err("execution admission closed during launch"));
        }
        let process = Arc::new(process);
        live.processes.retain(|process| process.strong_count() > 0);
        live.processes.push(Arc::downgrade(&process));
        Ok(Box::new(SharedScopedProcess(process)))
    }
}

/// Protocol consumers keep the lifecycle handle; the renewal binding holds
/// only Weak references. Last-owner drop still invokes ScopedProcess's stop.
struct SharedScopedProcess(Arc<ScopedProcess>);

#[async_trait]
impl pc::ProcessHandle for SharedScopedProcess {
    fn id(&self) -> &str {
        self.0.id()
    }
    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        self.0.wait().await
    }
    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.0.poll().await
    }
    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        self.0.signal(signal).await
    }
}
