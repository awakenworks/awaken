//! Real Docker backend (ADR-0041 Slice 5, `docker` feature).
//!
//! Implements [`ContainerRuntime`] over **bollard** — the Docker Engine HTTP API
//! via the SDK, never the `docker` CLI. Faithful to awaken-next's `DockerHandWorker`:
//! the agent runs as the container's main command (process-as-container); its stdio
//! port is **published** and reached by a **network dial** (via [`crate::net`]), not
//! `docker exec`. Validated against a real daemon in `tests/docker_it.rs`.

use std::collections::HashMap;
use std::net::SocketAddr;

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
use crate::{
    ContainerPlan, ContainerRuntime, ContainerState, ManagedContainer, REAPER_LABEL, RuntimeError,
};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

/// Map neutral resource limits onto a bollard `HostConfig`'s cgroup fields (limits
/// only — the caller merges binds/ports). Pure, so the swap-pin and disk mapping are
/// unit-testable without a daemon.
fn cgroup_host_config(limits: &pc::ResourceLimits) -> HostConfig {
    let caps = crate::CgroupCaps::from_limits(limits);
    HostConfig {
        memory: caps.memory_bytes,
        // Pin swap to the memory cap so a memory-limited agent cannot escape it by
        // swapping (the swap-escape close).
        memory_swap: caps.memory_swap_bytes,
        nano_cpus: caps.nano_cpus,
        pids_limit: caps.pids,
        storage_opt: caps.disk_size.map(|size| {
            let mut o = HashMap::new();
            o.insert("size".to_string(), size);
            o
        }),
        ..Default::default()
    }
}

/// The tmpfs map that keeps the writable app paths usable under a read-only rootfs
/// (`{dir: "rw,noexec,nosuid,size=64m"}`). Pure/testable.
fn tmpfs_for(plan: &ContainerPlan) -> HashMap<String, String> {
    crate::writable_dirs(plan)
        .into_iter()
        .map(|d| (d, "rw,noexec,nosuid,size=64m".to_string()))
        .collect()
}

#[cfg(test)]
mod cgroup_host_config_tests {
    use super::*;

    fn plan() -> ContainerPlan {
        ContainerPlan {
            image: "img:1".into(),
            command: vec!["a".into()],
            env: Vec::new(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            limits: pc::ResourceLimits::default(),
            memory_mounts: Vec::new(),
            rootfs: crate::RootfsPlan::HostUserland,
        }
    }

    #[test]
    fn tmpfs_keeps_outputs_and_tmp_writable_under_ro_rootfs() {
        let t = tmpfs_for(&plan());
        assert!(t.contains_key("/mnt/session/outputs"));
        assert!(t.contains_key("/tmp"));
        assert!(t["/tmp"].contains("noexec"));
    }

    #[test]
    fn memory_limit_pins_swap_and_maps_disk() {
        let hc = cgroup_host_config(&pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(256 * 1024 * 1024),
            pids: Some(64),
            disk_bytes: Some(1024),
        });
        assert_eq!(hc.memory, Some(256 * 1024 * 1024));
        assert_eq!(
            hc.memory_swap, hc.memory,
            "swap is pinned to the memory cap"
        );
        assert_eq!(hc.nano_cpus, Some(2_000_000_000));
        assert_eq!(hc.pids_limit, Some(64));
        assert_eq!(
            hc.storage_opt
                .as_ref()
                .and_then(|o| o.get("size"))
                .map(String::as_str),
            Some("1024")
        );
    }

    #[test]
    fn no_limits_leaves_the_cgroup_fields_empty() {
        let hc = cgroup_host_config(&pc::ResourceLimits::default());
        assert!(hc.memory.is_none());
        assert!(hc.memory_swap.is_none());
        assert!(hc.storage_opt.is_none());
    }

    #[test]
    fn signal_name_maps_every_signal() {
        assert_eq!(signal_name(pc::Signal::Term), "SIGTERM");
        assert_eq!(signal_name(pc::Signal::Kill), "SIGKILL");
        assert_eq!(signal_name(pc::Signal::Int), "SIGINT");
    }

    fn plan_with_binds() -> ContainerPlan {
        ContainerPlan {
            binds: vec![
                crate::BindPlan {
                    source_ref: "/host/ro".into(),
                    mount_path: "/in".into(),
                    read_only: true,
                    content: None,
                    content_bytes: None,
                },
                crate::BindPlan {
                    source_ref: "/host/rw".into(),
                    mount_path: "/work".into(),
                    read_only: false,
                    content: None,
                    content_bytes: None,
                },
            ],
            ..plan()
        }
    }

    // The bollard client builds lazily (no dial until a request), so the pure
    // host-config assembly is unit-testable without a live daemon.
    #[test]
    fn host_config_hardens_rootfs_publishes_the_agent_port_and_maps_binds_ro_flag() {
        let rt = DockerRuntime::connect_local(8080).expect("client builds without a daemon");
        let hc = rt.host_config(&plan_with_binds());
        assert_eq!(hc.readonly_rootfs, Some(true));
        assert!(hc.tmpfs.as_ref().unwrap().contains_key("/tmp"));
        assert!(hc.port_bindings.as_ref().unwrap().contains_key("8080/tcp"));
        let binds = hc.binds.unwrap();
        assert!(binds.contains(&"/host/ro:/in:ro".to_string()));
        assert!(binds.contains(&"/host/rw:/work".to_string()));
    }

    #[test]
    fn host_config_applies_the_planned_network_mode_and_gates_port_publishing() {
        let rt = DockerRuntime::connect_local(8080).expect("client builds without a daemon");

        // Open: default bridge (no explicit network_mode) with the agent port published.
        let open = rt.host_config(&plan()); // plan() defaults to NetworkMode::Open
        assert_eq!(open.network_mode, None);
        assert!(
            open.port_bindings
                .as_ref()
                .unwrap()
                .contains_key("8080/tcp")
        );

        // None: an empty network is applied and port publishing is dropped (Docker
        // forbids publishing under `--network none`). Without this the deny-egress
        // policy is planned but never enforced — a fail-open egress leak.
        let denied = rt.host_config(&ContainerPlan {
            network: crate::NetworkMode::None,
            ..plan()
        });
        assert_eq!(denied.network_mode.as_deref(), Some("none"));
        assert!(denied.port_bindings.is_none());

        // Allowlist: the container keeps the daemon's default bridge (so it can reach
        // the brokered proxy that enforces the allowlist) and the agent port is still
        // published. It must NOT be conflated with `None` (which would sever egress and
        // drop the channel) — only `None` denies the network.
        let allow = rt.host_config(&ContainerPlan {
            network: crate::NetworkMode::Allowlist(vec!["api.anthropic.com".into()]),
            ..plan()
        });
        assert_eq!(allow.network_mode, None);
        assert!(
            allow
                .port_bindings
                .as_ref()
                .unwrap()
                .contains_key("8080/tcp")
        );
    }

    #[test]
    fn with_client_wraps_a_handle_and_connect_local_builds_one() {
        let docker = Docker::connect_with_local_defaults().unwrap();
        let rt = DockerRuntime::with_client(docker, 9000);
        assert_eq!(rt.agent_port, 9000);
        assert_eq!(rt.port_key(), "9000/tcp");
    }

    #[tokio::test]
    async fn artifacts_are_out_of_band_and_touch_lease_is_a_noop() {
        let rt = DockerRuntime::connect_local(8080).unwrap();
        assert!(rt.artifacts("cid").await.unwrap().is_empty());
        assert!(rt.touch_lease("cid").await.is_ok());
    }
}

fn signal_name(signal: pc::Signal) -> &'static str {
    match signal {
        pc::Signal::Term => "SIGTERM",
        pc::Signal::Kill => "SIGKILL",
        pc::Signal::Int => "SIGINT",
    }
}

/// A Docker-backed [`ContainerRuntime`]. `agent_port` is the container-internal TCP
/// port the agent listens on; it is published to an ephemeral host port that
/// [`ContainerRuntime::open_channel`] discovers (via inspect) and dials.
pub struct DockerRuntime {
    docker: Docker,
    agent_port: u16,
}

impl DockerRuntime {
    /// Connect using the local defaults (unix socket / named pipe / env).
    pub fn connect_local(agent_port: u16) -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults().map_err(backend)?;
        Ok(Self { docker, agent_port })
    }

    /// Wrap an already-built client.
    pub fn with_client(docker: Docker, agent_port: u16) -> Self {
        Self { docker, agent_port }
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
        // Apply the planned egress policy at the container level. `None` gets its own
        // empty network (`--network none`); `Open`/`Allowlist` keep the daemon default
        // bridge (an allowlist is enforced at the brokered proxy, whose env is injected,
        // so the container still needs bridge egress to reach that chokepoint). Without
        // this the policy is planned but never applied — a fail-open egress leak.
        let deny_net = matches!(plan.network, crate::NetworkMode::None);
        // Publish the agent port to an ephemeral 127.0.0.1 host port — but not under
        // `--network none`, where Docker forbids port publishing (and there is no
        // reachable agent channel anyway).
        let port_bindings = (!deny_net).then(|| {
            let mut m = HashMap::new();
            m.insert(
                self.port_key(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some(String::new()),
                }]),
            );
            m
        });
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            port_bindings,
            network_mode: deny_net.then(|| "none".to_string()),
            // Harden the untrusted agent: read-only rootfs, writable app paths as tmpfs.
            readonly_rootfs: Some(true),
            tmpfs: Some(tmpfs_for(plan)),
            ..cgroup_host_config(&plan.limits)
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
        // The discovery label the cross-restart reaper (`crate::reaper`) filters on, so
        // a container this worker leaks on a crash is found + swept by a later process.
        let mut labels = HashMap::new();
        labels.insert(REAPER_LABEL.to_string(), "1".to_string());
        let config = Config {
            image: Some(plan.image.clone()),
            // Process-as-container: the agent argv IS the container command.
            cmd: Some(plan.command.clone()),
            env: Some(env),
            exposed_ports: Some(exposed_ports),
            host_config: Some(self.host_config(plan)),
            labels: Some(labels),
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

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>, RuntimeError> {
        // Discover every awaken-labeled container (running or stopped) so the reaper can
        // judge each. `all: true` includes exited ones — those are the finished-work
        // garbage. `created` is unix seconds; age = now - created against the host clock.
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{REAPER_LABEL}=1")]);
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .map_err(backend)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(list
            .into_iter()
            .filter_map(|c| {
                let id = c.id?;
                // Docker reports state as a lowercase string ("running", "exited", …).
                let running = c.state.as_deref() == Some("running");
                let age_secs = c
                    .created
                    .map(|created| now.saturating_sub(created.max(0) as u64))
                    .unwrap_or(0);
                Some(ManagedContainer {
                    id,
                    running,
                    age_secs,
                })
            })
            .collect())
    }
}
