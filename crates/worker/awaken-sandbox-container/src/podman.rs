//! Rootless **Podman** backend (`podman` feature) — the daemonless, worker-parented
//! executor for the container tier (awaken-next / oversight parity).
//!
//! Unlike the bollard/kube adapters, Podman is driven over its **CLI** (`podman run`
//! …): there is no daemon, so the container is a direct child of the worker and is
//! reaped as a unit (`--init` handles PID 1). It realizes the same [`ContainerRuntime`]
//! port as Docker/K8s and honors the [`crate::RootfsPlan`] (an `Image` or a private
//! `IsolatedRoot`), reached over the published agent port via [`crate::net`] — the
//! same dial the Docker adapter uses. Compile-verified here; running needs `podman`.

use std::net::SocketAddr;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use tokio::process::Command as OsCommand;

use crate::net::TcpAgentTransport;
use crate::{ContainerPlan, ContainerRuntime, ContainerState, RuntimeError, podman_run_argv};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

fn signal_flag(signal: pc::Signal) -> &'static str {
    match signal {
        pc::Signal::Term => "TERM",
        pc::Signal::Kill => "KILL",
        pc::Signal::Int => "INT",
    }
}

/// A rootless-Podman [`ContainerRuntime`]. `agent_port` is the container-internal TCP
/// port the agent listens on; it is published to an ephemeral `127.0.0.1` host port
/// that [`ContainerRuntime::open_channel`] discovers (`podman port`) and dials.
pub struct PodmanRuntime {
    bin: String,
    agent_port: u16,
}

impl PodmanRuntime {
    /// Use `podman` from `PATH` (override with `PODMAN_BIN`).
    #[must_use]
    pub fn new(agent_port: u16) -> Self {
        let bin = std::env::var("PODMAN_BIN").unwrap_or_else(|_| "podman".to_string());
        Self { bin, agent_port }
    }

    /// Run a podman subcommand, returning trimmed stdout (or a backend error).
    async fn run(&self, args: &[String]) -> Result<String, RuntimeError> {
        let out = OsCommand::new(&self.bin)
            .args(args)
            .output()
            .await
            .map_err(backend)?;
        if !out.status.success() {
            return Err(RuntimeError::Backend(format!(
                "podman {}: {}",
                args.first().cloned().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Probe the binary (for tests / health checks): `Ok` iff `podman` responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.run(&["info".into(), "--format".into(), "{{.Host.Arch}}".into()])
            .await
            .map(|_| ())
    }

    /// The ephemeral host address the agent port was published to (`podman port`).
    async fn agent_addr(&self, container_id: &str) -> Result<SocketAddr, RuntimeError> {
        let mapping = self
            .run(&[
                "port".into(),
                container_id.into(),
                format!("{}/tcp", self.agent_port),
            ])
            .await?;
        // e.g. "127.0.0.1:49153" (first line if multiple bindings).
        let host_port = mapping
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| backend("agent port is not published yet"))?;
        format!("127.0.0.1:{host_port}")
            .parse()
            .map_err(|e| backend(format!("bad published addr: {e}")))
    }
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let name = format!("awaken-{id}");
        // Idempotent: clear any stale container of this scope first.
        let _ = self.run(&["rm".into(), "-f".into(), name.clone()]).await;

        let mut args = podman_run_argv(&name, plan, &plan.rootfs);
        // Publish the agent's internal port to an ephemeral 127.0.0.1 host port so
        // `open_channel` can dial it (inserted after `--name <name>`, before the image).
        if let Some(i) = args.iter().position(|a| a == &name) {
            args.splice(
                i + 1..i + 1,
                ["-p".to_string(), format!("127.0.0.1::{}", self.agent_port)],
            );
        }
        self.run(&args).await?;
        Ok(name)
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // Process-as-container: reach the agent's stdio over its published port (a
        // network dial, not `podman exec`) — the same seam Docker/K8s use.
        let addr = self.agent_addr(container_id).await?;
        TcpAgentTransport::new(addr)
            .open_channel()
            .await
            .map_err(backend)
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        match self
            .run(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Running}}".into(),
                container_id.into(),
            ])
            .await
        {
            Ok(s) if s.trim() == "true" => Ok(ContainerState::Running),
            // Not running or not found → gone (adoption reconciles this to an orphan).
            _ => Ok(ContainerState::Gone),
        }
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        let code = self
            .run(&["wait".into(), container_id.into()])
            .await?
            .trim()
            .parse::<i32>()
            .map_err(|e| backend(format!("bad exit code: {e}")))?;
        Ok(pc::ExitStatus {
            code: Some(code),
            signaled: false,
        })
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        let s = self
            .run(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Status}} {{.State.ExitCode}}".into(),
                container_id.into(),
            ])
            .await?;
        let mut it = s.split_whitespace();
        let status = it.next().unwrap_or_default();
        if status == "running" {
            return Ok(None);
        }
        let code = it.next().and_then(|c| c.parse::<i32>().ok());
        Ok(Some(pc::ExitStatus {
            code,
            signaled: false,
        }))
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        self.run(&[
            "kill".into(),
            "--signal".into(),
            signal_flag(signal).into(),
            container_id.into(),
        ])
        .await
        .map(|_| ())
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band (like the Docker adapter): outputs are listed from the volume /
        // object store by the deployment, not streamed through the CLI.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        // Copy the file out as a tar stream to stdout (`podman cp <cid>:<path> -`),
        // mirroring the Docker adapter's tar-stream fallback.
        let out = OsCommand::new(&self.bin)
            .args(["cp", &format!("{container_id}:{artifact_id}"), "-"])
            .output()
            .await
            .map_err(backend)?;
        if !out.status.success() {
            return Err(backend(String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(out.stdout)
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Podman has no native lease/TTL; a lightweight reaper watches lease labels.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.run(&["rm".into(), "-f".into(), container_id.into()])
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_flag_maps_every_signal() {
        assert_eq!(signal_flag(pc::Signal::Term), "TERM");
        assert_eq!(signal_flag(pc::Signal::Kill), "KILL");
        assert_eq!(signal_flag(pc::Signal::Int), "INT");
    }

    #[test]
    fn new_defaults_to_podman_on_path() {
        // No PODMAN_BIN override → "podman".
        let rt = PodmanRuntime::new(9000);
        assert_eq!(rt.agent_port, 9000);
        assert!(rt.bin == "podman" || std::env::var("PODMAN_BIN").is_ok());
    }
}
