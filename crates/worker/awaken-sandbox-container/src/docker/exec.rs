//! One Docker exec, and how it is addressed after the caller that started it
//! is gone.
//!
//! # Why this is one module
//!
//! A detached exec outlives the call that made it, so everything about
//! reaching it again lives here: the public id that carries both the exec id
//! and the in-container pid record, the wrapper argv that writes that record
//! before it execs the command, and the signal path that resolves it. They are
//! one module because they are one agreement — the wrapper writes exactly what
//! the signal reads, and a caller holding only the public id can reconstruct
//! the handle from it.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures_util::StreamExt as _;

use super::EXEC_SEQUENCE;

pub(super) struct DockerExecProcess {
    docker: Docker,
    container_id: String,
    pub(super) exec_id: String,
    public_id: String,
    pid_file: Option<String>,
}

impl DockerExecProcess {
    pub(super) fn new(
        docker: Docker,
        container_id: &str,
        exec_id: String,
        pid_file: String,
    ) -> Self {
        let public_id = format!("{exec_id}|{pid_file}");
        Self {
            docker,
            container_id: container_id.to_string(),
            exec_id,
            public_id,
            pid_file: Some(pid_file),
        }
    }

    pub(super) fn recovered(docker: Docker, container_id: &str, public_id: &str) -> Self {
        let (exec_id, pid_file) = public_id
            .split_once('|')
            .map_or((public_id, None), |(exec_id, pid_file)| {
                (exec_id, Some(pid_file.to_string()))
            });
        Self {
            docker,
            container_id: container_id.to_string(),
            exec_id: exec_id.to_string(),
            public_id: public_id.to_string(),
            pid_file,
        }
    }

    async fn status(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let state = self
            .docker
            .inspect_exec(&self.exec_id)
            .await
            .map_err(|error| pc::SandboxError::backend("status", error))?;
        if state.running.unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(pc::ExitStatus {
            code: state.exit_code.map(|code| code as i32),
            signaled: false,
        }))
    }
}

#[async_trait]
impl pc::ProcessHandle for DockerExecProcess {
    fn id(&self) -> &str {
        &self.public_id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        awaken_local_process::wait_for_process_exit(self).await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.status().await
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        if self.status().await?.is_some() {
            return Ok(());
        }
        let result = async {
            let pid_file = self.pid_file.as_ref().ok_or_else(|| {
                pc::SandboxError::unsupported(
                    "legacy docker exec handle has no in-container pid reference",
                )
            })?;
            let name = match signal {
                pc::Signal::Term => "TERM",
                pc::Signal::Kill => "KILL",
                pc::Signal::Int => "INT",
            };
            let request = self
                .docker
                .create_exec(
                    &self.container_id,
                    CreateExecOptions {
                        cmd: Some(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            "pid=$(cat -- \"$1\") && kill -\"$2\" \"$pid\"".to_string(),
                            "awaken-signal".to_string(),
                            pid_file.clone(),
                            name.to_string(),
                        ]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| pc::SandboxError::backend("signal", error))?;
            let mut output = match self
                .docker
                .start_exec(
                    &request.id,
                    Some(StartExecOptions {
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|error| pc::SandboxError::backend("signal", error))?
            {
                StartExecResults::Attached { output, .. } => output,
                StartExecResults::Detached => {
                    return Err(pc::SandboxError::backend(
                        "signal",
                        "docker returned detached result for signal exec",
                    ));
                }
            };
            while let Some(frame) = output.next().await {
                frame.map_err(|error| pc::SandboxError::backend("signal", error))?;
            }
            let state = self
                .docker
                .inspect_exec(&request.id)
                .await
                .map_err(|error| pc::SandboxError::backend("signal", error))?;
            if state.exit_code == Some(0) {
                Ok(())
            } else {
                Err(pc::SandboxError::backend(
                    "signal",
                    format!("docker exec signal failed with {:?}", state.exit_code),
                ))
            }
        }
        .await;
        match result {
            Err(_) if self.status().await?.is_some() => Ok(()),
            result => result,
        }
    }
}

/// The attempt identity Docker stamps on one exec.
///
/// Podman and Kubernetes already minted `{kind}-exec-{process}-{sequence}`;
/// Docker minted a bare process-local counter, which is why its pid file could
/// not be attributed to anything. The process identity is what keeps two Worker
/// incarnations exec'ing into the same adopted container from writing the same
/// record.
fn docker_exec_attempt_id() -> String {
    format!(
        "docker-exec-{}-{}",
        std::process::id(),
        EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn wrapped_exec_argv(command: Vec<String>) -> (String, Vec<String>) {
    let pid_file = crate::exec_pid_file(&docker_exec_attempt_id());
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "pid_file=$1; shift; {} printf '%s' \"$$\" > \"$pid_file\"; exec \"$@\"",
            crate::exec_pid_file_prelude()
        ),
        "awaken-exec".to_string(),
        pid_file.clone(),
    ];
    argv.extend(command);
    (pid_file, argv)
}
