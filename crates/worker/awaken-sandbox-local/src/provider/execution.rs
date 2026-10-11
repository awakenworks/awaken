// @formal entrypoint sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
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

/// Deployment bytes, not execution authority. Selection may precede Session
/// binding; a selected scope cannot launch until its one authority is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalExecutionDeployment {
    launcher: PathBuf,
    helper: PathBuf,
}

impl LocalExecutionDeployment {
    /// Resolve once from operator PATH, not Agent environment. Retain the exact
    /// executable used by the probe; later launches do not repeat PATH lookup.
    pub fn discover(helper: PathBuf) -> Result<Self, pc::SandboxError> {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        Self::from_operator_path(helper, &paths)
    }

    fn from_operator_path(
        helper: PathBuf,
        paths: &std::ffi::OsStr,
    ) -> Result<Self, pc::SandboxError> {
        let launcher = awaken_local_process::resolve_operator_executable("bwrap", paths)
            .ok_or_else(|| {
                pc::SandboxError::unsupported(
                    "TrustedHost scoped execution requires bwrap on an absolute operator PATH",
                )
            })?;
        Self::new(launcher, helper)
    }

    /// Exercise the actual private handoff and exact-tree wait, not merely
    /// --version or Namespace readiness. No Session, business data, credentials
    /// or durable resource ownership is created by this disposable probe.
    pub async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        let command = pc::MaterializedCommand::new(["/usr/bin/true"]);
        let process = ScopedProcess::spawn_authorized(
            &self.launcher,
            &self.helper,
            ScopedCommand {
                materialized: &command,
                cwd: Path::new("/"),
                stdio: pc::Stdio::Null.into(),
            },
            ExecutionDeadline::after(CONTROL_TIMEOUT)
                .map_err(|error| pc::SandboxError::backend("probe_ready", error))?,
            CONTROL_TIMEOUT,
            |_, grant| grant.commit(),
        )
        .await
        .map_err(|_| {
            pc::SandboxError::backend(
                "probe_ready",
                "TrustedHost scoped execution probe could not launch",
            )
        })?;
        let status = tokio::time::timeout(CONTROL_TIMEOUT, process.wait())
            .await
            .map_err(|_| {
                pc::SandboxError::timeout("TrustedHost scoped execution probe timed out")
            })??;
        if status.code != Some(0) || status.signaled {
            return Err(pc::SandboxError::backend(
                "probe_ready",
                "TrustedHost scoped execution probe failed",
            ));
        }
        Ok(())
    }

    pub fn new(launcher: PathBuf, helper: PathBuf) -> Result<Self, pc::SandboxError> {
        if !launcher.is_absolute() || !helper.is_absolute() {
            return Err(pc::SandboxError::invalid(
                "execution deployment binaries must be absolute",
            ));
        }
        Ok(Self { launcher, helper })
    }
}

#[derive(Default)]
struct LiveExecution {
    closed: bool,
    processes: Vec<Weak<ScopedProcess>>,
}

pub(super) struct ScopedExecution {
    launcher: PathBuf,
    helper: PathBuf,
    authority: std::sync::OnceLock<Arc<dyn CurrentExecutionAuthority>>,
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
    pub(super) async fn scoped_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        publication: Option<&pc::RepositoryPublicationExpectation>,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
        intent: pc::RepositoryRealizationIntent,
    ) -> Result<Option<crate::repository_effect::Outcome>, pc::SandboxError> {
        use crate::repository_effect::{self, MAX_FRAME, Request};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ExecutionBinding::Scoped(execution) = self
            .execution
            .get_or_init(|| ExecutionBinding::Unsupervised)
        else {
            return Ok(None);
        };
        let bytes = Request::new(
            self.root.root(),
            self.require_root_identity()?,
            plan,
            publication,
            credential,
            intent,
        )
        .encode()?;
        let mut command = pc::MaterializedCommand::new([
            execution.helper.to_string_lossy().into_owned(),
            "repository-effect".into(),
        ]);
        command.cwd = self.root.root().to_string_lossy().into_owned();
        command.stdio = pc::Stdio::Piped;
        // This is Worker-side transport, not Agent execution. Preserve the
        // existing direct Git caller's ambient environment, with opaque values;
        // never resolve or inherit Session base_env (including loader knobs).
        command.env = std::env::vars_os()
            .map(|(name, value)| {
                Ok(pc::MaterializedEnvVar {
                    name: name.into_string().map_err(|_| {
                        pc::SandboxError::invalid("repository environment name is not Unicode")
                    })?,
                    value: pc::MaterializedEnvValue::Secret(
                        value
                            .into_string()
                            .map_err(|_| {
                                pc::SandboxError::invalid(
                                    "repository environment value is not Unicode",
                                )
                            })?
                            .into(),
                    ),
                })
            })
            .collect::<Result<Vec<_>, pc::SandboxError>>()?;
        for variable in repository_effect::environment(self.root.root(), credential) {
            command.env.retain(|current| current.name != variable.name);
            command.env.push(variable);
        }
        let prepared = PreparedCommand {
            command,
            cwd: self.root.root().to_owned(),
        };
        let mut process = execution
            .spawn_prepared(self, async { Ok(prepared) }, pc::Stdio::Piped.into())
            .await?;
        let mut input = process.take_stdin().ok_or_else(|| {
            pc::SandboxError::backend("scoped_repository", "repository input pipe missing")
        })?;
        let output = process.take_stdout().ok_or_else(|| {
            pc::SandboxError::backend("scoped_repository", "repository output pipe missing")
        })?;
        let error = process.take_stderr().ok_or_else(|| {
            pc::SandboxError::backend("scoped_repository", "repository error pipe missing")
        })?;
        // Register before any I/O wait. The existing sweep can refresh this
        // operation; cancellation drops the same last-owner handle and leaves
        // durable participant evidence until the complete tree is absent.
        let process = execution.track(process)?;
        let write = async move {
            input.write_all(&bytes).await?;
            input.shutdown().await?;
            // ChildStdin shutdown need not close the pipe. The bounded codec
            // reads to EOF, so release the writer before joining its response.
            drop(input);
            Ok::<_, io::Error>(())
        };
        let read = async {
            let mut bytes = Vec::new();
            output
                .take((MAX_FRAME + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, io::Error>(bytes)
        };
        let drain = async {
            let mut sink = tokio::io::sink();
            tokio::io::copy(&mut error.take((MAX_FRAME + 1) as u64), &mut sink).await
        };
        let (written, output, drained, status) = tokio::join!(write, read, drain, process.wait());
        // Business stdout alone is never an exit/cleanup receipt. Do not expose
        // malformed process bytes or stderr in errors (they may contain secrets).
        written.map_err(|_| pc::SandboxError::transport("repository request delivery failed"))?;
        drained.map_err(|_| pc::SandboxError::transport("repository error stream failed"))?;
        let status = status?;
        if status.code != Some(0) || status.signaled {
            return Err(pc::SandboxError::backend(
                "scoped_repository",
                "repository helper failed",
            ));
        }
        let output = output
            .map_err(|_| pc::SandboxError::transport("repository response delivery failed"))?;
        if output.len() > MAX_FRAME {
            return Err(pc::SandboxError::backend(
                "scoped_repository",
                "repository response too large",
            ));
        }
        serde_json::from_slice(&output).map(Some).map_err(|_| {
            pc::SandboxError::backend("scoped_repository", "repository response invalid")
        })
    }

    pub(super) fn select_execution_boundary(
        &self,
        deployment: &LocalExecutionDeployment,
    ) -> Result<(), pc::SandboxError> {
        if !self.trusted_host_access {
            return Err(pc::SandboxError::unauthorized(
                "host-view execution requires TrustedHost authorization",
            ));
        }
        self.execution
            .set(ExecutionBinding::Scoped(Box::new(ScopedExecution {
                launcher: deployment.launcher.clone(),
                helper: deployment.helper.clone(),
                authority: Default::default(),
                live: Mutex::new(LiveExecution::default()),
            })))
            .map_err(|_| {
                pc::SandboxError::conflict(
                    "execution boundary is already selected; late upgrade is forbidden",
                )
            })
    }

    /// Bind the current Session exactly once; an unselected/weak sandbox is not
    /// upgraded implicitly. The same selected scope retains all process handles.
    pub fn bind_execution_authority(
        &self,
        authority: Arc<dyn CurrentExecutionAuthority>,
    ) -> Result<bool, pc::SandboxError> {
        let Some(ExecutionBinding::Scoped(execution)) = self.execution.get() else {
            return Ok(false);
        };
        execution
            .authority
            .set(authority)
            .map_err(|_| pc::SandboxError::conflict("execution authority is already bound"))?;
        Ok(true)
    }

    pub fn has_scoped_execution(&self) -> bool {
        matches!(self.execution.get(), Some(ExecutionBinding::Scoped(_)))
    }

    pub fn needs_execution_authority(&self) -> bool {
        matches!(self.execution.get(), Some(ExecutionBinding::Scoped(scope)) if scope.authority.get().is_none())
    }
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
            return Err(pc::SandboxError::unsupported(
                "scoped execution requires an exact TrustedHost realization and absolute deployment binaries",
            ));
        }
        self.select_execution_boundary(&LocalExecutionDeployment::new(
            launcher.into(),
            helper.into(),
        )?)?;
        self.bind_execution_authority(authority).map(|_| ())
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
                        ExecutionDeadline::with_current_authority(
                            execution.authority.get().ok_or_else(denied)?.as_ref(),
                            &mut |deadline| {
                                let live = execution.live.lock().map_err(|_| denied())?;
                                if live.closed {
                                    return Err(denied());
                                }
                                grant.take().ok_or_else(denied)?.commit(deadline)
                            },
                        )
                    })
                    .await;
                if let Err(error) = refresh {
                    // The exact tree can exit after the preflight poll, including
                    // while current authority is being checked. Only a fresh
                    // complete ProcessHandle receipt makes that race benign;
                    // channel failure or leader exit alone cannot do so. This
                    // is a skipped renewal, never a Session stop receipt.
                    if process.poll().await?.is_none() {
                        return Err(pc::SandboxError::backend("refresh_scoped_execution", error));
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
        execution
            .live
            .lock()
            .map_err(|error| pc::SandboxError::backend("stop_scoped_execution", error))?
            .closed = true;
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
        .map_err(|_| pc::SandboxError::not_ready("execution tree stop is still unconfirmed"))??;
        let evidence = self
            .realization
            .current()
            .ok_or_else(|| pc::SandboxError::not_found("scoped realization evidence missing"))?;
        let removal = self.terminal_removal.lock().await;
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
        let mut live = self
            .live
            .lock()
            .map_err(|error| pc::SandboxError::backend("live_processes", error))?;
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
        self.spawn_prepared(sandbox, sandbox.build_command(command), stdio)
            .await
    }

    async fn spawn_prepared(
        &self,
        sandbox: &LocalSandbox,
        prepare: impl std::future::Future<Output = Result<PreparedCommand, pc::SandboxError>>,
        stdio: ScopedStdio,
    ) -> Result<ScopedProcess, pc::SandboxError> {
        let evidence = sandbox
            .realization
            .current()
            .ok_or_else(|| pc::SandboxError::not_found("scoped realization evidence missing"))?;
        let mut guard = crate::realization_marker::begin_execution(sandbox.root.root(), evidence)?;
        let mut deadline = None;
        ExecutionDeadline::with_current_authority(
            self.authority
                .get()
                .ok_or_else(|| pc::SandboxError::invalid("execution authority is not bound"))?
                .as_ref(),
            &mut |current_deadline| {
                if self.live.lock().map_err(|_| denied())?.closed || deadline.is_some() {
                    return Err(denied());
                }
                deadline = Some(current_deadline);
                Ok(())
            },
        )
        .map_err(|error| pc::SandboxError::backend("spawn_prepared", error))?;
        let deadline = deadline.ok_or_else(|| {
            pc::SandboxError::fence_rejected("current authority omitted execution deadline")
        })?;
        let prepared = prepare.await?;
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
                self.authority
                    .get()
                    .ok_or_else(denied)?
                    .with_current(&mut |_| {
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
        .map_err(|error| pc::SandboxError::backend("spawn_prepared", error))
    }

    pub(super) fn track(
        &self,
        process: ScopedProcess,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let mut live = self
            .live
            .lock()
            .map_err(|error| pc::SandboxError::backend("track", error))?;
        if live.closed {
            return Err(pc::SandboxError::gone(
                "execution admission closed during launch",
            ));
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

#[cfg(test)]
mod deployment_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn deployment_discovery_is_absolute_executable_ordered_and_frozen() {
        // P1 missing/relative/non-executable/directory PATH candidates -> reject;
        // P2 first absolute executable wins and symlinks resolve once; P3 later
        // PATH/file change never silently retargets the retained deployment.
        // Pure input seam avoids mutating global PATH during parallel tests.
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let helper = root.path().join("helper");
        let paths = std::env::join_paths([Path::new("."), &first, &second]).unwrap();
        let lookup = || LocalExecutionDeployment::from_operator_path(helper.clone(), &paths);
        assert!(lookup().is_err());
        std::fs::write(first.join("bwrap"), "fixture").unwrap();
        std::fs::set_permissions(first.join("bwrap"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert!(lookup().is_err());
        std::fs::write(second.join("bwrap"), "fixture").unwrap();
        std::fs::set_permissions(second.join("bwrap"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let selected = lookup().unwrap();
        assert_eq!(selected.launcher, second.join("bwrap"));
        std::fs::remove_file(first.join("bwrap")).unwrap();
        symlink(second.join("bwrap"), first.join("bwrap")).unwrap();
        assert_eq!(lookup().unwrap(), selected);
        std::fs::remove_file(first.join("bwrap")).unwrap();
        std::fs::write(first.join("bwrap"), "different fixture").unwrap();
        std::fs::set_permissions(first.join("bwrap"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(lookup().unwrap().launcher, first.join("bwrap"));
        assert_eq!(selected.launcher, second.join("bwrap"));
        std::fs::remove_file(first.join("bwrap")).unwrap();
        std::fs::create_dir(first.join("bwrap")).unwrap();
        assert_eq!(lookup().unwrap(), selected);
        std::fs::remove_file(second.join("bwrap")).unwrap();
        assert!(lookup().is_err());
        assert_eq!(selected.launcher, second.join("bwrap"));
        assert!(LocalExecutionDeployment::new("relative".into(), helper).is_err());
    }
}
