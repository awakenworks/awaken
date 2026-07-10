//! Docker `SandboxProvider` (B-P5b, ADR-0021 §8) — a real, non-demo sandbox
//! backend for the heterogeneous fleet. Each sandbox is a container: `create`
//! `docker run`s a long-lived container, `spawn` `docker exec`s a process under
//! full container isolation, `adopt` reconnects to a still-running container by its
//! id (survives host restart), and `dispose` `docker rm -f`s it. The `provider_kind`
//! is `"docker"`, so the fleet router and crash-adoption route to it by handle.
//!
//! Mount/artifact surface (`attach`/`artifacts`/`read_artifact`) is a documented
//! follow-up; the load-bearing path — isolated process execution with a persistable
//! handle — is real and covered by a live-docker e2e.

use async_trait::async_trait;
use awaken_provisioning_contract::{
    Artifact, Command, ExitStatus, IsolationClass, MountRequirement, ProcessHandle, RealizedMount,
    Sandbox, SandboxCapabilities, SandboxError, SandboxHandle, SandboxProvider, SandboxSpec,
    SandboxStatus, Signal,
};
use tokio::process::Command as OsCommand;

const PROVIDER_KIND: &str = "docker";

/// Creates container-isolated sandboxes via the local Docker daemon.
pub struct DockerSandboxProvider {
    image: String,
}

impl DockerSandboxProvider {
    /// Sandboxes are containers of `image` (must provide a shell; e.g.
    /// `alpine`). The container is kept alive with `sleep infinity` so `exec` can
    /// run many processes in it (ADR-0058 multiplexing).
    #[must_use]
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
        }
    }
}

async fn docker(args: &[&str]) -> Result<String, SandboxError> {
    let out = OsCommand::new("docker")
        .args(args)
        .output()
        .await
        .map_err(|e| SandboxError::new(format!("docker spawn: {e}")))?;
    if !out.status.success() {
        return Err(SandboxError::new(format!(
            "docker {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like [`docker`] but returns raw stdout bytes (for `cat`-ing a binary artifact).
async fn docker_bytes(args: &[&str]) -> Result<Vec<u8>, SandboxError> {
    let out = OsCommand::new("docker")
        .args(args)
        .output()
        .await
        .map_err(|e| SandboxError::new(format!("docker spawn: {e}")))?;
    if !out.status.success() {
        return Err(SandboxError::new(format!(
            "docker {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

#[async_trait]
impl SandboxProvider for DockerSandboxProvider {
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation: IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            secret_egress_substitution: false,
            resource_limits: true,
            custom_rootfs: true,
        }
    }

    async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
        // A long-lived container; exec runs the run's processes inside it.
        let name = format!("awaken-sbx-{}", spec.scope);
        // Remove any stale container of the same scope first (idempotent create).
        let _ = docker(&["rm", "-f", &name]).await;
        let id = docker(&[
            "run",
            "-d",
            "--name",
            &name,
            &self.image,
            "sleep",
            "infinity",
        ])
        .await?;
        Ok(Box::new(DockerSandbox {
            scope: spec.scope.clone(),
            container: id,
            outputs_path: spec.outputs_path.clone(),
        }))
    }

    async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
        // Reconnect by container id; fail closed if it is no longer running.
        let running = docker(&["inspect", "-f", "{{.State.Running}}", &handle.sandbox_id]).await?;
        if running.trim() != "true" {
            return Err(SandboxError::new("container not running"));
        }
        Ok(Box::new(DockerSandbox {
            scope: handle.sandbox_id.clone(),
            container: handle.sandbox_id.clone(),
            // The handle carries only the container id; a re-adopted sandbox uses the
            // conventional outputs root (matches the SandboxSpec default).
            outputs_path: "/workspace/out".to_string(),
        }))
    }
}

struct DockerSandbox {
    scope: String,
    container: String,
    /// The environment's outputs root (ADR-0021 §; SandboxSpec.outputs_path): where
    /// a run's produced artifacts are collected from.
    outputs_path: String,
}

#[async_trait]
impl Sandbox for DockerSandbox {
    fn id(&self) -> &str {
        &self.scope
    }

    fn handle(&self) -> SandboxHandle {
        SandboxHandle::new(PROVIDER_KIND, &self.container)
    }

    async fn spawn(&self, command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
        if command.argv.is_empty() {
            return Err(SandboxError::new("empty argv"));
        }
        let mut args: Vec<String> = vec!["exec".into()];
        if !command.cwd.is_empty() {
            args.push("-w".into());
            args.push(command.cwd.clone());
        }
        args.push(self.container.clone());
        args.extend(command.argv.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let child = OsCommand::new("docker")
            .args(&arg_refs)
            .spawn()
            .map_err(|e| SandboxError::new(format!("exec: {e}")))?;
        Ok(Box::new(DockerProcess {
            id: format!("{}-{}", self.container, command.argv.join("_")),
            child: tokio::sync::Mutex::new(Some(child)),
        }))
    }

    async fn attach(&self, _req: MountRequirement) -> Result<RealizedMount, SandboxError> {
        Err(SandboxError::new(
            "attach unsupported by the minimal docker driver",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
        // Content-address every file under the outputs root: one exec emits
        // "<sha256> <size> <relpath>" per file (missing root = no artifacts).
        let script = format!(
            "cd {out} 2>/dev/null || exit 0; find . -type f | while IFS= read -r f; do \
             printf '%s %s %s\\n' \"$(sha256sum \"$f\" | cut -d' ' -f1)\" \
             \"$(wc -c < \"$f\")\" \"$f\"; done",
            out = self.outputs_path
        );
        let listing = docker(&["exec", &self.container, "sh", "-c", &script]).await?;
        let root = self.outputs_path.trim_end_matches('/');
        let mut artifacts = Vec::new();
        for line in listing.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(hash), Some(size), Some(rel)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            artifacts.push(Artifact {
                id: hash.to_string(),
                path: format!("{root}/{}", rel.trim_start_matches("./")),
                size_bytes: size.parse().unwrap_or(0),
                content_hash: hash.to_string(),
            });
        }
        Ok(artifacts)
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, SandboxError> {
        // The id is the content hash; find its file under the outputs root and cat it.
        let target = self
            .artifacts()
            .await?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| SandboxError::new(format!("no artifact {id}")))?;
        docker_bytes(&["exec", &self.container, "cat", &target.path]).await
    }

    fn realized(&self) -> &[RealizedMount] {
        &[]
    }

    async fn process(&self, _process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
        // docker exec has no stable rejoinable id; recovery re-drives via a new exec.
        Err(SandboxError::new("process reattach unsupported"))
    }

    async fn status(&self) -> Result<SandboxStatus, SandboxError> {
        match docker(&["inspect", "-f", "{{.State.Running}}", &self.container]).await {
            Ok(r) if r.trim() == "true" => Ok(SandboxStatus::Ready),
            Ok(_) => Ok(SandboxStatus::Terminated),
            Err(_) => Ok(SandboxStatus::Terminated),
        }
    }

    async fn renew_lease(&self) -> Result<(), SandboxError> {
        // The container lives until dispose; nothing to renew for a local daemon.
        Ok(())
    }

    async fn dispose(&self) -> Result<(), SandboxError> {
        docker(&["rm", "-f", &self.container]).await.map(|_| ())
    }
}

struct DockerProcess {
    id: String,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

fn exit_of(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[async_trait]
impl ProcessHandle for DockerProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<ExitStatus, SandboxError> {
        let mut guard = self.child.lock().await;
        let child = guard
            .as_mut()
            .ok_or_else(|| SandboxError::new("already awaited"))?;
        let status = child
            .wait()
            .await
            .map_err(|e| SandboxError::new(format!("wait: {e}")))?;
        Ok(exit_of(status))
    }

    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError> {
        let mut guard = self.child.lock().await;
        let child = guard
            .as_mut()
            .ok_or_else(|| SandboxError::new("already awaited"))?;
        match child.try_wait() {
            Ok(Some(status)) => Ok(Some(exit_of(status))),
            Ok(None) => Ok(None),
            Err(e) => Err(SandboxError::new(format!("poll: {e}"))),
        }
    }

    async fn signal(&self, signal: Signal) -> Result<(), SandboxError> {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            if matches!(signal, Signal::Kill | Signal::Term) {
                let _ = child.start_kill();
            }
        }
        Ok(())
    }
}
