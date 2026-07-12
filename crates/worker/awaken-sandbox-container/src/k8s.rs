//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`crate::pod_plan`]: **process-as-container** (the Pod's container command is the
//! agent, `restartPolicy: Never`), **native GC** (an `ownerReference` reaps orphans),
//! and the stdio channel reached by a **network dial** to the Service (via
//! [`crate::net`]). Compile-verified here; running requires a cluster.

use std::net::SocketAddr;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{Container, EnvVar, Pod, PodSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;

use crate::net::TcpAgentTransport;
use crate::{ContainerPlan, ContainerRuntime, ContainerState, RuntimeError};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

/// The Pod container's `resources.limits` from a spec's caps, or `None` when unset —
/// so the tier actually enforces the `resource_limits` it advertises. k8s expresses
/// caps natively (CPU millicores, byte quantities), distinct from Docker's cgroup
/// fields, so the mapping is per-backend. `pids` has no standard pod limit key.
fn pod_resources(limits: &pc::ResourceLimits) -> Option<ResourceRequirements> {
    if !limits.is_set() {
        return None;
    }
    let mut m = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        m.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(mem) = limits.memory_bytes {
        m.insert("memory".to_string(), Quantity(mem.to_string()));
    }
    if let Some(disk) = limits.disk_bytes {
        m.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    if m.is_empty() {
        return None; // only `pids` was set — nothing k8s expresses as a pod limit
    }
    Some(ResourceRequirements {
        limits: Some(m),
        ..Default::default()
    })
}

/// A Kubernetes-backed [`ContainerRuntime`]. `agent_addr` is the Service endpoint the
/// runtime dials for the [`AgentChannel`]; `owner` (optional) is the GC owner.
pub struct K8sRuntime {
    client: Client,
    namespace: String,
    agent_addr: SocketAddr,
    owner: Option<OwnerReference>,
}

impl K8sRuntime {
    /// Connect via in-cluster ServiceAccount or the ambient kubeconfig.
    pub async fn connect(
        namespace: impl Into<String>,
        agent_addr: SocketAddr,
    ) -> Result<Self, RuntimeError> {
        // kube's rustls client needs a process-level CryptoProvider; install ring
        // once (idempotent — a prior install by the host is fine).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::try_default().await.map_err(backend)?;
        Ok(Self {
            client,
            namespace: namespace.into(),
            agent_addr,
            owner: None,
        })
    }

    /// Set the GC owner (e.g. a Lease/ConfigMap) whose deletion reaps orphan Pods.
    #[must_use]
    pub fn with_owner(mut self, owner: OwnerReference) -> Self {
        self.owner = Some(owner);
        self
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Probe the apiserver (for tests / health checks): `Ok` iff it responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.pods()
            .list(&ListParams::default().limit(1))
            .await
            .map(|_| ())
            .map_err(backend)
    }

    fn pod(&self, id: &str, plan: &ContainerPlan) -> Pod {
        let env = plan
            .env
            .iter()
            .map(|(k, v)| EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                value_from: None,
            })
            .collect();
        Pod {
            metadata: ObjectMeta {
                name: Some(format!("awaken-{id}")),
                // native GC: the platform reaps this Pod when the owner is deleted.
                owner_references: self.owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "agent".into(),
                    image: Some(plan.image.clone()),
                    // process-as-container: the agent argv is the container command.
                    command: Some(plan.command.clone()),
                    env: Some(env),
                    // Enforce the advertised resource caps as the container's limits.
                    resources: pod_resources(&plan.limits),
                    ..Default::default()
                }],
                // a finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                ..Default::default()
            }),
            status: None,
        }
    }
}

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let pod = self.pod(id, plan);
        let created = self
            .pods()
            .create(&PostParams::default(), &pod)
            .await
            .map_err(backend)?;
        created
            .metadata
            .name
            .ok_or_else(|| backend("created pod has no name"))
    }

    async fn open_channel(
        &self,
        _container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // Reach the agent's stdio over its Service (dial; not `kubectl exec`).
        TcpAgentTransport::new(self.agent_addr)
            .open_channel()
            .await
            .map_err(backend)
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
        Ok(if phase == "Running" || phase == "Pending" {
            ContainerState::Running
        } else {
            ContainerState::Gone
        })
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        // Compile path: read the terminated exit code. A running deployment watches
        // the Pod to completion instead of a single read.
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        let code = pod
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|c| c.into_iter().next())
            .and_then(|cs| cs.state)
            .and_then(|st| st.terminated)
            .map(|t| t.exit_code);
        match code {
            Some(code) => Ok(pc::ExitStatus {
                code: Some(code),
                signaled: false,
            }),
            None => Err(backend("agent pod has not terminated")),
        }
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        match self.inspect(container_id).await? {
            ContainerState::Running => Ok(None),
            ContainerState::Gone => Ok(Some(pc::ExitStatus {
                code: None,
                signaled: true,
            })),
        }
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        // K8s has no per-process signal; deletion is the reap. SIGKILL → grace 0.
        let params = match signal {
            pc::Signal::Kill => DeleteParams::default().grace_period(0),
            _ => DeleteParams::default(),
        };
        self.pods()
            .delete(container_id, &params)
            .await
            .map(|_| ())
            .map_err(backend)
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band: read from the outputs PVC, not through the API server.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        _container_id: &str,
        _artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        Err(backend(
            "k8s artifacts are read out-of-band from the outputs PVC",
        ))
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Native GC: the owner Lease's renewTime is patched by the owner controller;
        // ownerReference + TTL reaps orphans (no bespoke reaper here).
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.pods()
            .delete(container_id, &DeleteParams::default())
            .await
            .map(|_| ())
            .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_resources_maps_cpu_memory_and_disk() {
        let r = pod_resources(&pc::ResourceLimits {
            cpu_millis: Some(1500),
            memory_bytes: Some(1_073_741_824),
            pids: Some(256),
            disk_bytes: Some(2048),
        })
        .expect("limits are set");
        let limits = r.limits.expect("limits map present");
        assert_eq!(limits.get("cpu").unwrap().0, "1500m");
        assert_eq!(limits.get("memory").unwrap().0, "1073741824");
        assert_eq!(limits.get("ephemeral-storage").unwrap().0, "2048");
        // pids has no standard pod-level key.
        assert!(!limits.contains_key("pids"));
    }

    #[test]
    fn pod_resources_is_none_without_expressible_caps() {
        assert!(pod_resources(&pc::ResourceLimits::default()).is_none());
        // pids-only → nothing k8s expresses as a pod limit.
        assert!(
            pod_resources(&pc::ResourceLimits {
                pids: Some(9),
                ..Default::default()
            })
            .is_none()
        );
    }
}
