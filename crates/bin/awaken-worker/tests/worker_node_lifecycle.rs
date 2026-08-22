use awaken_worker::{StandardManifestConfig, WorkerNodeBuilder, WorkerShutdown};
use awaken_worker_contract::{VersionRange, WorkerManifest};
use awaken_worker_transport_security::{
    SignedWorkerRequestAuthorizer, WorkerSigningCredential, WorkerUpstream,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod support;
use support::FakeWorkerUpstream;

fn manifest() -> WorkerManifest {
    WorkerManifest {
        build_digest: "worker-node-test".to_string(),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        ..WorkerManifest::default()
    }
}

fn local_coordinator_deployment() -> awaken_runtime_host::DeploymentConfig {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.disable_local_pool = true;
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    deployment
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral admin port")
        .local_addr()
        .expect("read the ephemeral admin address")
        .port()
}

fn http_status(address: &str, method: &str, path: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(address).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nhost: {address}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn poll_status(address: &str, method: &str, path: &str, expected: u16) -> bool {
    for _ in 0..300 {
        if http_status(address, method, path) == Some(expected) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[derive(Default)]
struct ExternalSessionProvider {
    ready: Option<Arc<AtomicBool>>,
}

impl ExternalSessionProvider {
    fn with_readiness(ready: Arc<AtomicBool>) -> Self {
        Self { ready: Some(ready) }
    }
}

#[derive(Default)]
struct RecordingCapacity {
    warmups: Mutex<Vec<(awaken_provisioning_contract::SandboxSpec, usize)>>,
    discarded: Mutex<Vec<awaken_provisioning_contract::SandboxSpec>>,
    ready: Mutex<BTreeMap<String, usize>>,
    fail_warmup: AtomicBool,
    shut_down: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironmentCapacity for RecordingCapacity {
    async fn prewarm_to(
        &self,
        spec: &awaken_provisioning_contract::SandboxSpec,
        target: usize,
    ) -> Result<usize, awaken_provisioning_contract::SandboxError> {
        self.warmups.lock().unwrap().push((spec.clone(), target));
        if self.fail_warmup.load(Ordering::SeqCst) {
            Err(awaken_provisioning_contract::SandboxError::new(
                "injected warmup failure",
            ))
        } else {
            self.ready
                .lock()
                .unwrap()
                .insert(serde_json::to_string(spec).unwrap(), target);
            Ok(target)
        }
    }

    fn ready_capacity(&self, spec: &awaken_provisioning_contract::SandboxSpec) -> usize {
        self.ready
            .lock()
            .unwrap()
            .get(&serde_json::to_string(spec).unwrap())
            .copied()
            .unwrap_or(0)
    }

    async fn shutdown_capacity(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
    }

    async fn discard_shape(&self, spec: &awaken_provisioning_contract::SandboxSpec) {
        self.discarded.lock().unwrap().push(spec.clone());
        self.ready
            .lock()
            .unwrap()
            .remove(&serde_json::to_string(spec).unwrap());
    }
}

struct NoHandFactory;

impl awaken_runtime_host::HandExecutorFactory for NoHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn awaken_runtime_contract::tool::ToolExecutor> {
        panic!("lifecycle fixture never binds a hand channel")
    }
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironmentProvider for ExternalSessionProvider {
    fn sandbox_capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
        awaken_provisioning_contract::SandboxCapabilities {
            isolation: awaken_provisioning_contract::IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: true,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: true,
            package_provisioning: false,
        }
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        if self
            .ready
            .as_ref()
            .is_none_or(|ready| ready.load(Ordering::SeqCst))
        {
            Ok(())
        } else {
            Err(awaken_provisioning_contract::SandboxError::new(
                "injected provider evidence drift",
            ))
        }
    }

    async fn create_environment(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::new(
            "manifest-only fixture",
        ))
    }

    async fn adopt_environment(
        &self,
        _handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::new(
            "manifest-only fixture",
        ))
    }
}

#[test]
fn builder_rejects_incomplete_or_invalid_topology() {
    let missing = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .build()
        .err()
        .expect("manifest is mandatory");
    assert!(missing.to_string().contains("manifest"));

    let empty_upstream = WorkerNodeBuilder::new(WorkerUpstream::new(""))
        .with_manifest(manifest())
        .build()
        .err()
        .expect("empty upstream is invalid");
    assert!(empty_upstream.to_string().contains("upstream"));

    let mut zero_capacity = manifest();
    zero_capacity.capacity.max_concurrent = 0;
    let invalid_capacity = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_manifest(zero_capacity)
        .build()
        .err()
        .expect("zero capacity is invalid");
    assert!(invalid_capacity.to_string().contains("max_concurrent"));

    let invalid_standard_capacity = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_standard_manifest_config(
            StandardManifestConfig::new("worker-node-test").with_max_concurrent(0),
        )
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("standard and explicit manifests share capacity validation");
    assert!(
        invalid_standard_capacity
            .to_string()
            .contains("max_concurrent")
    );
}

#[test]
fn standard_manifest_is_derived_from_builder_topology() {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.acp = Some(
        awaken_runtime_host::AcpWorkerProfile::new(vec!["codex".to_string()])
            .expect("valid ACP profile"),
    );
    let worker = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_deployment_config(deployment)
        .with_standard_manifest(std::collections::BTreeSet::from([
            "application:test/v1".to_string()
        ]))
        .build()
        .expect("standard manifest derives a valid worker topology");

    assert!(worker.manifest().capabilities.contains("native-runtime"));
    assert!(
        worker
            .manifest()
            .capabilities
            .contains("application:test/v1")
    );
    assert!(worker.manifest().capabilities.contains("acp:codex"));
}

#[test]
fn external_session_provider_requires_its_hand_channel_port() {
    // Cause/effect decision rules: E1 external container provider + missing
    // hand factory fails during build; E2 an empty backend fails on identity
    // before runtime. Capability advertisement cannot outpace executable wiring.
    let missing_hand = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider::default()),
        )
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("E1 incomplete external provider topology");
    assert!(missing_hand.to_string().contains("hand executor factory"));

    let empty_backend = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_session_container_provider(" ", Arc::new(ExternalSessionProvider::default()))
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("E2 provider identity is required");
    assert!(empty_backend.to_string().contains("backend"));
}

#[test]
fn container_deployment_cannot_bypass_canonical_provider_preparation() {
    /* FMECA / cause-effect graph for Worker assembly: C1=container tier;
     * C2=canonical provider prepared; C3=explicit substitute installed.
     * E1=manifest, Host, and heartbeat share one instance; E2=build fails.
     * S=10/O=4/D=1, RPN=40. Rules A1 !C1=>normal; A2 C1&&(C2||C3)=>E1;
     * A3 C1&&!C2&&!C3=>E2. This case owns A3; provider drift owns A2. */
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::K8s;
    let error = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_deployment_config(deployment)
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("A3 must reject an unprepared container deployment");
    assert!(
        error.to_string().contains("canonical Session provider"),
        "A3: {error}"
    );
}

#[test]
fn explicit_and_standard_manifest_sources_are_mutually_exclusive() {
    let error = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_manifest(manifest())
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("conflicting manifest sources fail closed");

    assert!(error.to_string().contains("mutually exclusive"));

    let reverse = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_standard_manifest(Default::default())
        .with_manifest(manifest())
        .build()
        .err()
        .expect("reverse manifest source conflict also fails closed");
    assert!(reverse.to_string().contains("mutually exclusive"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runs_register_ready_drain_quiesce_and_deregister() {
    let upstream = FakeWorkerUpstream::start();
    let request_authorizer = Arc::new(SignedWorkerRequestAuthorizer::new(
        WorkerSigningCredential::new(
            "worker-node-test",
            "key-1",
            "credential-1",
            b"fixture-secret".to_vec(),
        )
        .expect("signing credential is valid"),
    ));
    let registered_identity = Arc::new(Mutex::new(None));
    let observed = registered_identity.clone();
    let coordinator_defaults = local_coordinator_deployment();
    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url())
            .with_request_authorizer(request_authorizer)
            .with_worker_id("worker-node-test"),
    )
    .with_deployment_config(coordinator_defaults)
    .with_manifest(manifest())
    .with_attempt_decorator_factory(Arc::new(move |context| {
        *observed.lock().expect("identity observation mutex") = Some(context.identity().clone());
        let decorator: awaken_runtime_host::AttemptExecutorDecorator = Arc::new(|inner| inner);
        Ok(decorator)
    }))
    .without_admin_surface()
    .build()
    .expect("valid explicit Worker topology")
    .run_until(async {
        // Cause graph and decision table:
        // C1 registered + C2 initial Ready + C3 WorkerNode role
        // => E1 claim loop starts before shutdown, even when injected deployment
        // carried coordinator defaults (`durable=false`, local pool disabled).
        // | Rule | C1 | C2 | C3 | E1 |
        // | W1   | 1  | 1  | 1  | 1  |
        for _ in 0..100 {
            if upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/dispatch/claim")
            {
                return Ok(WorkerShutdown::Prompt);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("W1: Ready Worker did not poll the authoritative dispatch queue");
    })
    .await
    .expect("Worker lifecycle completes");

    let identity = registered_identity
        .lock()
        .expect("identity observation mutex")
        .clone()
        .expect("factory receives registered identity");
    assert_eq!(identity.worker_id, "worker-node-test");
    assert_eq!(identity.generation, 1);

    let requests = upstream.requests();
    let positions: Vec<_> = [
        "/v1/worker/register",
        "/v1/worker/heartbeat",
        "/v1/worker/dispatch/claim",
        "/v1/worker/drain",
        "/v1/worker/quiesced",
        "/v1/worker/deregister",
    ]
    .into_iter()
    .map(|path| {
        requests
            .iter()
            .position(|request| request == path)
            .unwrap_or_else(|| panic!("missing {path} in {requests:?}"))
    })
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "Worker lifecycle calls are ordered: {requests:?}"
    );
    let headers = upstream.request_headers();
    assert_eq!(headers.len(), requests.len());
    assert!(headers.iter().all(|header| {
        header
            .to_ascii_lowercase()
            .contains("authorization: awakenworker ")
    }));
}

/// Cause/effect design:
/// C1=Worker has external provider+capacity, C2=warm target > 0, C3=warmup
/// succeeds, C4=prompt shutdown after Ready and asynchronous warmup. E1=Ready
/// is independent of capacity evidence, E2=the canonical empty Container shape
/// is warmed once with the exact target, E3=shutdown drains capacity before
/// deregistration completes. Decision rule: (C1,C2,C3,C4)->(E1,E2,E3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_publishes_ready_then_warms_and_drains_capacity_on_shutdown() {
    let upstream = FakeWorkerUpstream::start();
    let capacity = Arc::new(RecordingCapacity::default());
    let mut deployment = local_coordinator_deployment();
    deployment.sandbox.warm_pool_size = 2;

    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-warmup-lifecycle-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
    .with_hand_executor_factory(Arc::new(NoHandFactory))
    .with_manifest(manifest())
    .without_admin_surface()
    .build()
    .expect("valid warm-capacity Worker topology")
    .run_until(async {
        let mut observed_ready = false;
        for _ in 0..300 {
            if upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/heartbeat")
            {
                observed_ready = true;
            }
            let warmups = capacity.warmups.lock().unwrap().clone();
            if observed_ready && warmups.len() == 1 {
                assert_eq!(warmups[0].1, 2);
                assert_eq!(
                    warmups[0].0.isolation,
                    awaken_provisioning_contract::IsolationClass::Container
                );
                assert!(warmups[0].0.mounts.is_empty());
                return Ok(WorkerShutdown::Prompt);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Worker did not publish Ready")
    })
    .await
    .expect("warm-capacity Worker lifecycle completes");

    assert!(capacity.shut_down.load(Ordering::SeqCst));
    let requests = upstream.requests();
    let deregistered = requests
        .iter()
        .position(|path| path == "/v1/worker/deregister")
        .expect("Worker deregisters");
    assert!(deregistered > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_reconciles_current_environment_shape_after_ready() {
    // FMECA: F1 Ready is misread as exact Environment capacity evidence
    // (S5 O6 D2, RPN60); F2 warmup uses a default spec instead of the frozen
    // projection (S8 O4 D3, RPN96); F3 warmup transport failure blocks Worker
    // readiness (S7 O3 D2, RPN42); F4 receipt is published for failed/zero
    // capacity (S6 O3 D3, RPN54); F5 independent default and Environment
    // producers oversubscribe one global pool and immediately evict a just-created
    // container (S6 O6 D4, RPN144); F6 desired shapes beyond the global budget
    // rotate forever (S5 O6 D3, RPN90); F7 a config update retains unused old
    // capacity (S5 O5 D2, RPN50). Mitigation is one Environment-first capacity
    // plan, canonical typed shape identity, cold-path degradation, observed
    // receipts, stable budget selection, and desired-state discard.
    // Cause graph: C1=current cloud snapshot; C2=capacity enabled; C3=prewarm
    // succeeds; C4=transport available; C5=desired cost exceeds total budget;
    // C6=current config shape changes; C7=default and Environment compete for a
    // total budget of one. E1=Ready publishes independently and the exact shape
    // is requested asynchronously; E2=Worker
    // continues cold without receipt; E3=stable prefix selected within budget;
    // E4=old unused capacity discarded before replacement warmup; E5=only one
    // prewarm call occurs, with Environment priority. Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Effect |
    // | E1   | 1  | 1  | 1  | 1  | 0  | 0  | 0  | Ready, then exact warm |
    // | E2   | 1  | 1  | 0  | 1  | -  | 0  | -  | cold Ready, no receipt |
    // | E3   | -  | 1  | -  | 0  | -  | 0  | -  | cold Ready, prior receipt retained |
    // | E4   | 1  | 1  | 1  | 1  | 1  | 0  | 1  | E3,E5, one exact prewarm |
    // | E5   | 1  | 1  | 1  | 1  | -  | 1  | 1  | E4, then exact replacement |
    let upstream = FakeWorkerUpstream::start();
    let snapshot = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env-current".into(),
        revision: awaken_session_contract::EnvironmentRevision(7),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("config-v7".into()),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: Some("registry.example/env@sha256:exact".into()),
        network: awaken_session_contract::SessionNetworkPolicy::None,
        credential_realization:
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let mut over_budget = snapshot.clone();
    over_budget.environment_id = "env-over-budget".into();
    over_budget.network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
    upstream.set_environment_warmups_json(
        serde_json::to_string(&[snapshot.clone(), over_budget.clone()]).unwrap(),
    );
    let mut replacement = over_budget;
    replacement.environment_id = "env-current".into();
    replacement.revision = awaken_session_contract::EnvironmentRevision(8);
    replacement.prepared_image = Some("registry.example/env@sha256:replacement".into());
    let replacement_response = serde_json::to_string(&[replacement]).unwrap();
    let capacity = Arc::new(RecordingCapacity::default());
    let mut deployment = local_coordinator_deployment();
    deployment.sandbox.warm_pool_size = 1;
    deployment.sandbox.warm_pool_total_size = 1;

    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-current-environment-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
    .with_hand_executor_factory(Arc::new(NoHandFactory))
    .with_manifest(manifest())
    .without_admin_surface()
    .build()
    .unwrap()
    .run_until(async {
        let mut changed = false;
        let mut observed_ready = false;
        for _ in 0..1_500 {
            if upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/heartbeat")
            {
                observed_ready = true;
            }
            let warmups = capacity.warmups.lock().unwrap().clone();
            if observed_ready {
                if !changed && warmups.len() == 1 {
                    let exact = &warmups[0].0;
                    assert_eq!(
                        exact.network,
                        awaken_provisioning_contract::NetworkPolicy::None,
                        "E1 exact network"
                    );
                    assert_eq!(
                        exact.environment,
                        Some(awaken_provisioning_contract::EnvironmentKind::Image {
                            reference: "registry.example/env@sha256:exact".into()
                        }),
                        "E1 exact image"
                    );
                    upstream.set_environment_warmups_json(replacement_response.clone());
                    changed = true;
                } else if changed
                    && warmups.len() == 2
                    && !capacity.discarded.lock().unwrap().is_empty()
                {
                    assert_eq!(
                        warmups[1].0.network,
                        awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                        "E5 replacement exact network"
                    );
                    return Ok(WorkerShutdown::Prompt);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Worker did not reconcile initial and replacement Environment shapes")
    })
    .await
    .unwrap();
}

/// Cause/effect design:
/// C1=capacity warmup fails, C2=provider/isolation selection remains valid.
/// E1=Worker publishes Ready independently and the asynchronous warmup is
/// attempted once, E2=no alternate/lower-isolation provider is selected,
/// E3=capacity shutdown still runs.
/// Decision rule: (C1,C2)->(E1,E2,E3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warmup_failure_retains_cold_path_and_still_closes_capacity() {
    let upstream = FakeWorkerUpstream::start();
    let capacity = Arc::new(RecordingCapacity::default());
    capacity.fail_warmup.store(true, Ordering::SeqCst);
    let mut deployment = local_coordinator_deployment();
    deployment.sandbox.warm_pool_size = 1;

    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-warmup-failure-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
    .with_hand_executor_factory(Arc::new(NoHandFactory))
    .with_manifest(manifest())
    .without_admin_surface()
    .build()
    .expect("valid failure-degradation Worker topology")
    .run_until(async {
        let mut observed_ready = false;
        for _ in 0..300 {
            if upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/heartbeat")
            {
                observed_ready = true;
            }
            if observed_ready && capacity.warmups.lock().unwrap().len() == 1 {
                return Ok(WorkerShutdown::Prompt);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("warmup failure must not prevent Ready")
    })
    .await
    .expect("cold path remains available");

    assert!(capacity.shut_down.load(Ordering::SeqCst), "E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_evidence_drift_drains_before_the_next_heartbeat() {
    // Cause/effect graph: C1 provider evidence holds before initial Ready; C2
    // the same evidence fails before a periodic heartbeat; C3 no user shutdown.
    // Effects: E1 initial Ready; E2 no heartbeat from stale evidence; E3 the
    // existing authority-loss path drains and asks the supervisor to restart.
    // Rules P1 C1&&!C2=>normal (covered elsewhere); P2 !C1=>startup rejection;
    // P3 C1+C2+C3=>E1+E2+E3 (this test).
    let upstream = FakeWorkerUpstream::start();
    let ready = Arc::new(AtomicBool::new(true));
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        WorkerNodeBuilder::new(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-provider-drift-test"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider::with_readiness(ready.clone())),
        )
        .with_hand_executor_factory(Arc::new(NoHandFactory))
        .with_manifest(manifest())
        .with_graceful_drain(Duration::ZERO)
        .without_admin_surface()
        .build()
        .expect("valid provider-evidence topology")
        .run_until(async {
            for _ in 0..300 {
                if upstream
                    .requests()
                    .iter()
                    .any(|path| path == "/v1/worker/heartbeat")
                {
                    ready.store(false, Ordering::SeqCst);
                    return std::future::pending::<
                        Result<WorkerShutdown, Box<dyn std::error::Error + Send + Sync>>,
                    >()
                    .await;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("P3 initial Ready was not published")
        }),
    )
    .await
    .expect("P3 drift terminates without a user signal")
    .expect_err("P3 supervisor must restart the drained incarnation");
    assert!(
        result.to_string().contains("supervisor restart required"),
        "E3"
    );
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|path| path.as_str() == "/v1/worker/heartbeat")
            .count(),
        1,
        "E2 stale evidence is fenced before the next heartbeat"
    );
    assert!(
        upstream
            .requests()
            .iter()
            .any(|path| path == "/v1/worker/drain"),
        "E3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authority_loss_terminates_the_incarnation_for_supervisor_restart() {
    // Cause/effect decision table:
    // A1 initial heartbeat applied + periodic heartbeat applied -> Worker remains
    // Ready (covered by node_runs_register_ready_drain_quiesce_and_deregister);
    // A2 initial heartbeat applied + next heartbeat rejects this incarnation ->
    // local admission closes, lifecycle cleanup runs, and run_until returns an
    // error so the process supervisor creates a fresh registered incarnation.
    // A3 no external shutdown signal -> A2 must still terminate by itself.
    let upstream = FakeWorkerUpstream::start_rejecting_periodic_heartbeat();
    // The fixture validates Worker authority, not host sandbox discovery. Bind
    // the hermetic Local tier explicitly so a machine without bwrap reaches A2
    // while the production Namespace default remains fail-closed.
    let coordinator_defaults = local_coordinator_deployment();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        WorkerNodeBuilder::new(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-authority-loss-test"),
        )
        .with_deployment_config(coordinator_defaults)
        .with_manifest(manifest())
        .with_graceful_drain(Duration::ZERO)
        .without_admin_surface()
        .build()
        .expect("valid Worker topology")
        .run_until(std::future::pending()),
    )
    .await
    .expect("A3 authority loss terminates without a signal")
    .expect_err("A2 supervisor must observe a failed incarnation");
    assert!(
        result.to_string().contains("supervisor restart required"),
        "A2"
    );
    let requests = upstream.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|path| path.as_str() == "/v1/worker/heartbeat")
            .count(),
        2,
        "A2 initial and rejected periodic heartbeat"
    );
    assert!(
        requests.iter().any(|path| path == "/v1/worker/drain"),
        "A2 cleanup publishes drain when authority becomes reachable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_closes_local_admission_before_control_acknowledges() {
    let (upstream, drain_release) = FakeWorkerUpstream::start_with_blocked_drain();
    let admin_addr = format!("127.0.0.1:{}", free_port());
    let coordinator_defaults = local_coordinator_deployment();

    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-drain-order-test"),
    )
    .with_deployment_config(coordinator_defaults)
    .with_manifest(manifest())
    .with_admin_listen(&admin_addr)
    .build()
    .expect("valid explicit Worker topology")
    .run_until(async {
        assert!(
            poll_status(&admin_addr, "GET", "/readyz", 200),
            "worker reaches accepting before drain"
        );

        let drain_addr = admin_addr.clone();
        let drain_request = std::thread::spawn(move || {
            http_status(&drain_addr, "POST", "/admin/drain")
                .expect("admin drain returns an HTTP response")
        });
        for _ in 0..300 {
            if upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/drain")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            upstream
                .requests()
                .iter()
                .any(|path| path == "/v1/worker/drain"),
            "Control receives the drain mutation"
        );
        assert_eq!(
            http_status(&admin_addr, "GET", "/readyz"),
            Some(503),
            "local claim admission closes before the blocked Control response returns"
        );

        drain_release.store(true, Ordering::Release);
        assert_eq!(drain_request.join().expect("drain request joins"), 200);
        Ok(WorkerShutdown::Prompt)
    })
    .await
    .expect("Worker lifecycle completes");
}
