//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`crate::pod_plan`]: **process-as-container** (the Pod's container command is the
//! agent, `restartPolicy: Never`), **native GC** (an `ownerReference` reaps orphans),
//! and the stdio channel reached by a **network dial** to the pod's ClusterIP Service.
//!
//! Four correctness gaps closed from the initial stub:
//! - `open_channel` now creates a ClusterIP Service per Pod and dials its assigned
//!   cluster IP (instead of using a construction-time fixed address).
//! - `pod()` now maps `ContainerPlan::limits` onto `resources.requests/limits` so
//!   the kubelet enforces the declared CPU/memory bounds.
//! - `create()` creates a `NetworkPolicy` for `Allowlist` / `None` egress modes
//!   (the default-deny baseline; FQDN granularity requires a CNI FQDN extension).
//! - `wait()` now watches the Pod to completion instead of a single read, and both
//!   `wait()` and `poll()` return the real container exit code.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use futures_util::StreamExt as _;
use k8s_openapi::api::core::v1::{
    Container, EnvVar, Pod, PodSpec, ResourceRequirements, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::{NetworkPolicy as K8sNetworkPolicy, NetworkPolicySpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, PostParams, WatchParams};
use kube::{Api, Client};

use crate::net::TcpAgentTransport;
use crate::{ContainerPlan, ContainerRuntime, ContainerState, NetworkMode, RuntimeError};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

/// Label map used to tie a Pod to its Service and NetworkPolicy.
fn app_label(pod_name: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("app".to_string(), pod_name.to_string());
    m
}

fn service_name(container_id: &str) -> String {
    format!("{container_id}-svc")
}

fn netpol_name(container_id: &str) -> String {
    format!("{container_id}-netpol")
}

/// Extract the exit code from the first container's terminated state, if present.
fn exit_code_from_pod(pod: &Pod) -> Option<i32> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .next()?
        .state
        .as_ref()?
        .terminated
        .as_ref()
        .map(|t| t.exit_code)
}

/// Return a terminal exit status if the pod has finished, `None` while still live.
fn terminal_status(pod: &Pod) -> Option<pc::ExitStatus> {
    if let Some(code) = exit_code_from_pod(pod) {
        return Some(pc::ExitStatus {
            code: Some(code),
            signaled: false,
        });
    }
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("");
    match phase {
        "Succeeded" => Some(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        }),
        "Failed" => Some(pc::ExitStatus {
            code: None,
            signaled: false,
        }),
        _ => None,
    }
}

/// Map `ResourceLimits` onto a Kubernetes `ResourceRequirements` (requests == limits
/// so the container class is Guaranteed, not Burstable).
fn build_resource_requirements(limits: &pc::ResourceLimits) -> ResourceRequirements {
    let mut map: BTreeMap<String, Quantity> = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        map.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(mem) = limits.memory_bytes {
        map.insert("memory".to_string(), Quantity(mem.to_string()));
    }
    let opt_map = if map.is_empty() { None } else { Some(map) };
    ResourceRequirements {
        requests: opt_map.clone(),
        limits: opt_map,
        claims: None,
    }
}

/// A Kubernetes-backed [`ContainerRuntime`].
///
/// `agent_port` is the TCP port the agent process listens on inside the Pod.
/// A ClusterIP Service is created for each Pod at [`ContainerRuntime::create`] time;
/// [`ContainerRuntime::open_channel`] looks up the assigned cluster IP and dials it.
pub struct K8sRuntime {
    client: Client,
    namespace: String,
    agent_port: u16,
    owner: Option<OwnerReference>,
}

impl K8sRuntime {
    /// Connect via in-cluster ServiceAccount or the ambient kubeconfig.
    pub async fn connect(
        namespace: impl Into<String>,
        agent_port: u16,
    ) -> Result<Self, RuntimeError> {
        // kube's rustls client needs a process-level CryptoProvider; install ring
        // once (idempotent — a prior install by the host is fine).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::try_default().await.map_err(backend)?;
        Ok(Self {
            client,
            namespace: namespace.into(),
            agent_port,
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

    fn services(&self) -> Api<Service> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn network_policies(&self) -> Api<K8sNetworkPolicy> {
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
        let pod_name = format!("awaken-{id}");
        let env: Vec<EnvVar> = plan
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
                name: Some(pod_name.clone()),
                // Selector label consumed by the paired ClusterIP Service.
                labels: Some(app_label(&pod_name)),
                // Native GC: the platform reaps this Pod when the owner is deleted.
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
                    // Enforce the declared CPU/memory limits on the kubelet.
                    resources: Some(build_resource_requirements(&plan.limits)),
                    ..Default::default()
                }],
                // A finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                ..Default::default()
            }),
            status: None,
        }
    }

    /// Create a headless-style ClusterIP Service that routes traffic to `pod_name`.
    async fn create_service(&self, pod_name: &str) -> Result<(), RuntimeError> {
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(service_name(pod_name)),
                owner_references: self.owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(app_label(pod_name)),
                ports: Some(vec![ServicePort {
                    port: i32::from(self.agent_port),
                    target_port: Some(IntOrString::Int(i32::from(self.agent_port))),
                    protocol: Some("TCP".into()),
                    ..Default::default()
                }]),
                type_: Some("ClusterIP".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.services()
            .create(&PostParams::default(), &svc)
            .await
            .map(|_| ())
            .map_err(backend)
    }

    /// Create a NetworkPolicy that enforces the declared egress policy for `pod_name`.
    ///
    /// `Open` → no policy (unrestricted egress). `Allowlist` → default-deny all
    /// egress (FQDN granularity requires a CNI extension such as Cilium's
    /// `CiliumNetworkPolicy`). `None` → deny all ingress and egress.
    async fn maybe_create_network_policy(
        &self,
        pod_name: &str,
        network: &NetworkMode,
    ) -> Result<(), RuntimeError> {
        let (policy_types, egress) = match network {
            NetworkMode::Open => return Ok(()),
            NetworkMode::Allowlist(_) => (
                Some(vec!["Egress".to_string()]),
                Some(vec![]), // empty vec = deny all egress
            ),
            NetworkMode::None => (
                Some(vec!["Egress".to_string(), "Ingress".to_string()]),
                Some(vec![]),
            ),
        };
        let np = K8sNetworkPolicy {
            metadata: ObjectMeta {
                name: Some(netpol_name(pod_name)),
                owner_references: self.owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: LabelSelector {
                    match_labels: Some(app_label(pod_name)),
                    ..Default::default()
                },
                policy_types,
                egress,
                ingress: None,
            }),
        };
        self.network_policies()
            .create(&PostParams::default(), &np)
            .await
            .map(|_| ())
            .map_err(backend)
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
        let pod_name = created
            .metadata
            .name
            .ok_or_else(|| backend("created pod has no name"))?;
        // Create the ClusterIP Service that open_channel will dial.
        self.create_service(&pod_name).await?;
        // Apply an egress NetworkPolicy when the plan restricts egress.
        self.maybe_create_network_policy(&pod_name, &plan.network)
            .await?;
        Ok(pod_name)
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // Look up the ClusterIP assigned to this pod's Service.
        let svc = self
            .services()
            .get(&service_name(container_id))
            .await
            .map_err(backend)?;
        let cluster_ip = svc
            .spec
            .and_then(|s| s.cluster_ip)
            .filter(|ip| !ip.is_empty() && ip != "None")
            .ok_or_else(|| backend("service has no ClusterIP assigned yet"))?;
        let addr: SocketAddr = format!("{cluster_ip}:{}", self.agent_port)
            .parse()
            .map_err(|e| backend(format!("bad service addr: {e}")))?;
        TcpAgentTransport::new(addr)
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

    /// Watch the Pod to completion and return the real exit code.
    ///
    /// Uses `resourceVersion=0` so the watch server sends a synthetic ADDED event
    /// for the current pod state first — covering the already-terminated case without
    /// a separate pre-flight GET. Retries the watch on timeout/disconnect until the
    /// pod reaches a terminal phase.
    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        let wp = WatchParams::default()
            .fields(&format!("metadata.name={container_id}"))
            .timeout(300);
        loop {
            // Pin the watch stream: kube's watch impl is !Unpin, so box it.
            let mut stream = Box::pin(self.pods().watch(&wp, "0").await.map_err(backend)?);
            while let Some(event) = stream.next().await {
                let pod = match event.map_err(backend)? {
                    kube::core::WatchEvent::Added(p) | kube::core::WatchEvent::Modified(p) => p,
                    kube::core::WatchEvent::Deleted(_) => {
                        // Pod deleted before a clean termination (no exit code).
                        return Ok(pc::ExitStatus {
                            code: None,
                            signaled: true,
                        });
                    }
                    _ => continue,
                };
                if let Some(status) = terminal_status(&pod) {
                    return Ok(status);
                }
            }
            // Watch timed out or the connection dropped; retry.
        }
    }

    /// Poll the pod once; returns `None` while still running, `Some` when done.
    ///
    /// Returns the real exit code from the container termination state rather than
    /// treating every non-Running phase as `signaled: true`.
    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        let pod = match self.pods().get(container_id).await {
            Ok(p) => p,
            Err(kube::Error::Api(e)) if e.code == 404 => {
                // Pod deleted without a recorded exit code (externally reaped).
                return Ok(Some(pc::ExitStatus {
                    code: None,
                    signaled: true,
                }));
            }
            Err(e) => return Err(backend(e)),
        };
        if let Some(status) = terminal_status(&pod) {
            return Ok(Some(status));
        }
        let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
        if phase == "Running" || phase == "Pending" {
            Ok(None)
        } else {
            // Unknown / other terminal phase with no container termination record.
            Ok(Some(pc::ExitStatus {
                code: None,
                signaled: false,
            }))
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
        // Native GC via ownerReference; no bespoke reaper touch needed here.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.pods()
            .delete(container_id, &DeleteParams::default())
            .await
            .map(|_| ())
            .map_err(backend)?;
        // Best-effort cleanup: Service and NetworkPolicy may not exist (e.g. Open
        // egress mode never creates a NetworkPolicy), so ignore Not Found errors.
        let _ = self
            .services()
            .delete(&service_name(container_id), &DeleteParams::default())
            .await;
        let _ = self
            .network_policies()
            .delete(&netpol_name(container_id), &DeleteParams::default())
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_and_netpol_names_are_deterministic() {
        assert_eq!(service_name("awaken-run-7"), "awaken-run-7-svc");
        assert_eq!(netpol_name("awaken-run-7"), "awaken-run-7-netpol");
    }

    #[test]
    fn build_resource_requirements_maps_cpu_and_memory() {
        let limits = pc::ResourceLimits {
            cpu_millis: Some(500),
            memory_bytes: Some(512 * 1024 * 1024),
            pids: None,
            disk_bytes: None,
        };
        let rr = build_resource_requirements(&limits);
        let lims = rr.limits.as_ref().unwrap();
        let reqs = rr.requests.as_ref().unwrap();
        assert_eq!(lims.get("cpu").unwrap().0, "500m");
        assert_eq!(reqs.get("cpu").unwrap().0, "500m");
        assert_eq!(
            lims.get("memory").unwrap().0,
            format!("{}", 512 * 1024 * 1024)
        );
    }

    #[test]
    fn build_resource_requirements_empty_if_no_limits() {
        let rr = build_resource_requirements(&pc::ResourceLimits::default());
        assert!(rr.limits.is_none());
        assert!(rr.requests.is_none());
    }

    #[test]
    fn terminal_status_none_for_running_pod() {
        let pod = Pod::default();
        assert!(terminal_status(&pod).is_none());
    }

    #[test]
    fn terminal_status_reads_exit_code_from_terminated_container() {
        use k8s_openapi::api::core::v1::{
            ContainerState as K8sContainerState, ContainerStateTerminated, ContainerStatus,
            PodStatus,
        };
        let pod = Pod {
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(K8sContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 42,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let status = terminal_status(&pod).unwrap();
        assert_eq!(status.code, Some(42));
        assert!(!status.signaled);
    }

    #[test]
    fn terminal_status_uses_succeeded_phase_as_zero_exit() {
        use k8s_openapi::api::core::v1::PodStatus;
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Succeeded".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let status = terminal_status(&pod).unwrap();
        assert_eq!(status.code, Some(0));
        assert!(!status.signaled);
    }

    #[test]
    fn terminal_status_failed_phase_no_code() {
        use k8s_openapi::api::core::v1::PodStatus;
        let pod = Pod {
            status: Some(PodStatus {
                phase: Some("Failed".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let status = terminal_status(&pod).unwrap();
        assert_eq!(status.code, None);
        assert!(!status.signaled);
    }

    #[test]
    fn app_label_uses_pod_name() {
        let labels = app_label("awaken-run-1");
        assert_eq!(labels.get("app").unwrap(), "awaken-run-1");
    }
}
