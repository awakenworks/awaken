//! Real Docker backend (ADR-0041 Slice 5, `docker` feature).
//!
//! Implements [`ContainerRuntime`] over **bollard** — the Docker Engine HTTP API
//! via the SDK, never the `docker` CLI. Faithful to awaken-next's `DockerHandWorker`:
//! the agent runs as the container's main command (process-as-container); its stdio
//! port is **published** and reached by a **network dial** (via [`crate::net`]), not
//! `docker exec`. Validated against a real daemon in `tests/docker_it.rs`.
//!
//! ## Lease / GC
//!
//! Docker has no native TTL or ownerReference GC. The approach:
//!
//! - `create` stamps `awaken.lease-ttl` and `awaken.lease-renewed-at` labels on
//!   every container so the reaper can identify awaken-managed containers and their
//!   intended TTL even after the control plane that created them has restarted.
//! - `touch_lease` records the heartbeat instant in an in-process map shared with
//!   `DockerReaper`. Since Docker labels cannot be updated on a running container,
//!   the in-process map is authoritative for containers whose control plane is live.
//! - [`DockerReaper::sweep`] reaps containers whose heartbeat has lapsed — using the
//!   in-process map for managed containers, the `awaken.lease-renewed-at` label for
//!   orphans from a previous process run.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, KillContainerOptions,
    ListContainersOptions, RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::models::{HostConfig, PortBinding};
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

/// Unix epoch seconds from the system clock.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Label key: dead-man's-switch TTL stamped at container creation time.
pub const LABEL_LEASE_TTL: &str = "awaken.lease-ttl";
/// Label key: Unix epoch (seconds) when the container was created / lease started.
pub const LABEL_LEASE_RENEWED_AT: &str = "awaken.lease-renewed-at";

/// Whether a container's lease has expired.
///
/// Pure function; extracted for unit testing without a Docker daemon.
///
/// - `now_secs` — current Unix epoch seconds.
/// - `last_heartbeat` — most-recent in-process `touch_lease` timestamp, or `None`
///   for orphaned containers whose control plane is no longer running.
/// - `created_at_secs` — the `awaken.lease-renewed-at` label value (stamped at
///   create time); used as a fallback for orphans.
/// - `ttl_secs` — from the `awaken.lease-ttl` label.
pub fn lease_expired(
    now_secs: u64,
    last_heartbeat: Option<u64>,
    created_at_secs: u64,
    ttl_secs: u64,
) -> bool {
    let reference = last_heartbeat.unwrap_or(created_at_secs);
    now_secs.saturating_sub(reference) > ttl_secs
}

/// A Docker-backed [`ContainerRuntime`]. `agent_port` is the container-internal TCP
/// port the agent listens on; it is published to an ephemeral host port that
/// [`ContainerRuntime::open_channel`] discovers (via inspect) and dials.
///
/// `leases` is shared with any [`DockerReaper`] created from this runtime via
/// [`DockerRuntime::reaper`].
pub struct DockerRuntime {
    docker: Docker,
    agent_port: u16,
    /// In-process heartbeat map: container ID → epoch seconds of last `touch_lease`.
    leases: Arc<Mutex<HashMap<String, u64>>>,
}

impl DockerRuntime {
    /// Connect using the local defaults (unix socket / named pipe / env).
    pub fn connect_local(agent_port: u16) -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults().map_err(backend)?;
        Ok(Self {
            docker,
            agent_port,
            leases: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Wrap an already-built client.
    pub fn with_client(docker: Docker, agent_port: u16) -> Self {
        Self {
            docker,
            agent_port,
            leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a [`DockerReaper`] that shares this runtime's heartbeat map.
    ///
    /// The reaper is cheap to clone (everything is reference-counted) and safe
    /// to run on a background task.
    pub fn reaper(&self) -> DockerReaper {
        DockerReaper {
            docker: self.docker.clone(),
            leases: Arc::clone(&self.leases),
        }
    }

    /// Probe the daemon (for tests / health checks): `Ok` iff it responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.docker.version().await.map(|_| ()).map_err(backend)
    }

    fn port_key(&self) -> String {
        format!("{}/tcp", self.agent_port)
    }

    fn host_config(&self, plan: &ContainerPlan) -> HostConfig {
        let binds: Vec<String> = plan
            .binds
            .iter()
            .map(|b| {
                let ro = if b.read_only { ":ro" } else { "" };
                format!("{}:{}{ro}", b.source_ref, b.mount_path)
            })
            .collect();
        // Publish the agent port to an ephemeral 127.0.0.1 host port.
        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            self.port_key(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(String::new()),
            }]),
        );
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            port_bindings: Some(port_bindings),
            memory: plan.limits.memory_bytes.map(|m| m as i64),
            nano_cpus: plan.limits.cpu_millis.map(|c| i64::from(c) * 1_000_000),
            pids_limit: plan.limits.pids.map(i64::from),
            ..Default::default()
        }
    }

    /// Discover the ephemeral host address the agent port was published to.
    async fn agent_addr(&self, container_id: &str) -> Result<SocketAddr, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let host_port = info
            .network_settings
            .and_then(|n| n.ports)
            .and_then(|ports| ports.get(&self.port_key()).cloned().flatten())
            .and_then(|bindings| bindings.into_iter().next())
            .and_then(|b| b.host_port)
            .ok_or_else(|| backend("agent port is not published yet"))?;
        format!("127.0.0.1:{host_port}")
            .parse()
            .map_err(|e| backend(format!("bad published addr: {e}")))
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let env: Vec<String> = plan.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(self.port_key(), HashMap::new());

        // Stamp lease labels so the reaper can identify and GC orphaned containers.
        let labels = plan.lease_ttl_secs.map(|ttl| {
            let now = unix_secs().to_string();
            let mut m = HashMap::new();
            m.insert(LABEL_LEASE_TTL.to_string(), ttl.to_string());
            m.insert(LABEL_LEASE_RENEWED_AT.to_string(), now);
            m
        });

        let config = Config {
            image: Some(plan.image.clone()),
            // Process-as-container: the agent argv IS the container command.
            cmd: Some(plan.command.clone()),
            env: Some(env),
            exposed_ports: Some(exposed_ports),
            host_config: Some(self.host_config(plan)),
            labels,
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

        // Seed the in-process heartbeat map so this container is tracked immediately.
        if plan.lease_ttl_secs.is_some() {
            self.leases
                .lock()
                .unwrap()
                .insert(created.id.clone(), unix_secs());
        }

        Ok(created.id)
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // The agent is the container's main process; reach its stdio over the
        // published port (awaken-next dials, it does not `docker exec`).
        let addr = self.agent_addr(container_id).await?;
        TcpAgentTransport::new(addr)
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

    /// Record a heartbeat for this container. The in-process timestamp is the
    /// authoritative liveness signal for the [`DockerReaper`]; orphaned containers
    /// (from a crashed control plane) fall back to the creation-time label.
    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.leases
            .lock()
            .unwrap()
            .insert(container_id.to_string(), unix_secs());
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
            .map_err(backend)?;
        // Remove from the heartbeat map so the reaper does not try to GC it again.
        self.leases.lock().unwrap().remove(container_id);
        Ok(())
    }
}

// ── Reaper ────────────────────────────────────────────────────────────────────

/// Sweeps expired awaken-managed Docker containers.
///
/// Obtain one via [`DockerRuntime::reaper`]; the reaper shares the runtime's
/// in-process heartbeat map so it uses real heartbeats for managed containers and
/// the creation-time label for orphans.
///
/// Intended to run as a background task (e.g. a `tokio::spawn` loop):
///
/// ```ignore
/// let reaper = runtime.reaper();
/// tokio::spawn(async move {
///     loop {
///         let _ = reaper.sweep().await;
///         tokio::time::sleep(Duration::from_secs(30)).await;
///     }
/// });
/// ```
pub struct DockerReaper {
    docker: Docker,
    /// Shared with the owning `DockerRuntime`.
    leases: Arc<Mutex<HashMap<String, u64>>>,
}

impl DockerReaper {
    /// Scan all running awaken-managed containers and force-remove those whose
    /// lease has lapsed.
    ///
    /// Returns the number of containers reaped. Errors from individual container
    /// removals are suppressed (best-effort); only enumeration errors are returned.
    pub async fn sweep(&self) -> Result<u32, RuntimeError> {
        let now = unix_secs();
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![LABEL_LEASE_TTL.to_string()]);
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: false, // only running — stopped containers are already gone
                filters,
                ..Default::default()
            }))
            .await
            .map_err(backend)?;

        let mut reaped = 0u32;
        for c in containers {
            let id = match c.id.as_deref() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            let labels = c.labels.unwrap_or_default();
            let ttl = labels
                .get(LABEL_LEASE_TTL)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(3600);
            let created_at = labels
                .get(LABEL_LEASE_RENEWED_AT)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);

            let expired = {
                let map = self.leases.lock().unwrap();
                lease_expired(now, map.get(&id).copied(), created_at, ttl)
            };

            if expired {
                let _ = self
                    .docker
                    .remove_container(
                        &id,
                        Some(RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await;
                self.leases.lock().unwrap().remove(&id);
                reaped += 1;
            }
        }
        Ok(reaped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_not_expired_when_recently_touched() {
        assert!(!lease_expired(100, Some(90), 0, 60));
    }

    #[test]
    fn lease_expired_when_heartbeat_lapsed() {
        assert!(lease_expired(200, Some(100), 0, 60));
    }

    #[test]
    fn lease_not_expired_for_fresh_orphan() {
        // Orphan created 30s ago, TTL is 60s — not yet expired.
        assert!(!lease_expired(130, None, 100, 60));
    }

    #[test]
    fn lease_expired_for_stale_orphan() {
        // Orphan: no heartbeat map entry; created at t=0, TTL=60, now=100.
        assert!(lease_expired(100, None, 0, 60));
    }

    #[test]
    fn lease_boundary_is_exclusive() {
        // now - reference == ttl → not yet expired (strictly >).
        assert!(!lease_expired(160, Some(100), 0, 60));
        assert!(lease_expired(161, Some(100), 0, 60));
    }
}
