//! Real Docker backend (ADR-0041 Slice 5, `docker` feature).
//!
//! Implements [`ContainerRuntime`] over **bollard** — the Docker Engine HTTP API
//! via the SDK, never the `docker` CLI. Faithful to awaken-next's `DockerHandWorker`:
//! the agent runs as the container's main command (process-as-container) and the
//! stdio channel is a **network dial to the published port** (via [`crate::net`]),
//! not `docker exec`. Compile-verified here; running requires a Docker daemon.

use std::net::SocketAddr;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, KillContainerOptions,
    RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::models::HostConfig;
use futures_util::StreamExt;

use crate::net::TcpAgentTransport;
use crate::{ContainerPlan, ContainerRuntime, ContainerState, RuntimeError};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

fn signal_name(signal: pc::Signal) -> &'static str {
    match signal {
        pc::Signal::Term => "SIGTERM",
        pc::Signal::Kill => "SIGKILL",
        pc::Signal::Int => "SIGINT",
    }
}

/// A Docker-backed [`ContainerRuntime`]. `agent_addr` is where the published agent
/// port is reachable (the runtime dials it for the [`AgentChannel`]).
pub struct DockerRuntime {
    docker: Docker,
    agent_addr: SocketAddr,
}

impl DockerRuntime {
    /// Connect using the local defaults (unix socket / named pipe / env).
    pub fn connect_local(agent_addr: SocketAddr) -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults().map_err(backend)?;
        Ok(Self { docker, agent_addr })
    }

    /// Wrap an already-built client (e.g. a remote endpoint).
    pub fn with_client(docker: Docker, agent_addr: SocketAddr) -> Self {
        Self { docker, agent_addr }
    }

    fn host_config(plan: &ContainerPlan) -> HostConfig {
        let binds: Vec<String> = plan
            .binds
            .iter()
            .map(|b| {
                let ro = if b.read_only { ":ro" } else { "" };
                format!("{}:{}{ro}", b.source_ref, b.mount_path)
            })
            .collect();
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            memory: plan.limits.memory_bytes.map(|m| m as i64),
            nano_cpus: plan.limits.cpu_millis.map(|c| i64::from(c) * 1_000_000),
            pids_limit: plan.limits.pids.map(i64::from),
            ..Default::default()
        }
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let env: Vec<String> = plan.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let config = Config {
            image: Some(plan.image.clone()),
            // Process-as-container: the agent argv IS the container command.
            cmd: Some(plan.command.clone()),
            env: Some(env),
            host_config: Some(Self::host_config(plan)),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: format!("awaken-{id}"),
                    platform: None,
                }),
                config,
            )
            .await
            .map_err(backend)?;
        self.docker
            .start_container(&created.id, None::<StartContainerOptions<String>>)
            .await
            .map_err(backend)?;
        Ok(created.id)
    }

    async fn open_channel(
        &self,
        _container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // The agent is the container's main process; reach its stdio over the
        // published port (awaken-next dials, it does not `docker exec`).
        TcpAgentTransport::new(self.agent_addr)
            .open_channel()
            .await
            .map_err(backend)
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
        Ok(if running {
            ContainerState::Running
        } else {
            ContainerState::Gone
        })
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        let mut stream = self
            .docker
            .wait_container(container_id, None::<WaitContainerOptions<String>>);
        match stream.next().await {
            Some(Ok(resp)) => Ok(pc::ExitStatus {
                code: Some(resp.status_code as i32),
                signaled: false,
            }),
            Some(Err(e)) => Err(backend(e)),
            None => Err(RuntimeError::NotFound(container_id.into())),
        }
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let state = info.state.unwrap_or_default();
        if state.running.unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(pc::ExitStatus {
            code: state.exit_code.map(|c| c as i32),
            signaled: false,
        }))
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        self.docker
            .kill_container(
                container_id,
                Some(KillContainerOptions {
                    signal: signal_name(signal),
                }),
            )
            .await
            .map_err(backend)
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band: artifacts are listed from the outputs volume/object store by
        // the deployment, not streamed through the Engine API. Wired per deployment.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        // A fallback path via the Engine API: copy the file out as a tar stream.
        let mut stream = self.docker.download_from_container(
            container_id,
            Some(DownloadFromContainerOptions { path: artifact_id }),
        );
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(backend)?);
        }
        Ok(buf)
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Docker has no native lease/TTL; a lightweight reaper watches lease labels.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(backend)
    }
}
