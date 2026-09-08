use awaken_worker::{StandardManifestConfig, WorkerNodeBuilder, WorkerShutdown};
use awaken_worker_contract::{VersionRange, WorkerManifest};
use awaken_worker_transport_security::{
    SignedWorkerRequestAuthorizer, WorkerSigningCredential, WorkerUpstream,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod support;
use support::FakeWorkerUpstream;

fn manifest() -> WorkerManifest {
    WorkerManifest {
        build_digest: "worker-node-test".to_string(),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange {
            min: 1,
            max: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        },
        ..WorkerManifest::default()
    }
}

fn local_coordinator_deployment() -> awaken_session_executor::DeploymentConfig {
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    deployment.disable_local_pool = true;
    deployment.sandbox_tier = awaken_session_executor::SandboxTier::Namespace;
    deployment
}

fn lifecycle_worker(upstream: WorkerUpstream) -> WorkerNodeBuilder {
    // These cases own registry/claim/heartbeat lifecycle, not Native execution.
    // Bind the existing explicit provider fixture on every OS instead of
    // accidentally opting into TrustedHost and requiring a local Hand binary.
    // Fault/capacity cases replace this SAME provider via the ordinary builder.
    // Real Native/TrustedHost execution remains in the portability gate.
    WorkerNodeBuilder::new(upstream)
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider::default()),
        )
        .with_hand_executor_factory(Arc::new(NoHandFactory))
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

async fn poll_status(address: &str, method: &str, path: &str, expected: u16) -> bool {
    for _ in 0..300 {
        if http_status(address, method, path) == Some(expected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[derive(Default)]
struct ExternalSessionProvider {
    ready: Option<Arc<AtomicBool>>,
    probe_fault: Option<(usize, ProbeFault)>,
    probe_calls: AtomicUsize,
    active_probes: AtomicUsize,
}

struct ActiveProbe<'a>(&'a AtomicUsize);

impl Drop for ActiveProbe<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum ProbeFault {
    Stall,
    StallOnce,
    Panic,
}

impl ExternalSessionProvider {
    fn with_readiness(ready: Arc<AtomicBool>) -> Self {
        Self {
            ready: Some(ready),
            ..Self::default()
        }
    }

    fn stalling_after_startup() -> Self {
        Self {
            probe_fault: Some((1, ProbeFault::Stall)),
            ..Self::default()
        }
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

impl awaken_session_executor::HandExecutorFactory for NoHandFactory {
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
            process_tree_supervision: false,
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
            control_services: Default::default(),
        }
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        self.active_probes.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveProbe(&self.active_probes);
        let ordinal = self.probe_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((after, fault)) = self.probe_fault
            && ordinal >= after
        {
            match fault {
                ProbeFault::Stall => std::future::pending::<()>().await,
                ProbeFault::StallOnce if ordinal == after => std::future::pending::<()>().await,
                ProbeFault::StallOnce => {}
                ProbeFault::Panic => panic!("injected provider probe panic"),
            }
        }
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
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
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
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    deployment.acp = Some(
        awaken_session_executor::AcpWorkerProfile::new(vec!["codex".to_string()])
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
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_session_executor::SandboxTier::K8s;
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
    lifecycle_worker(
        WorkerUpstream::new(upstream.url())
            .with_request_authorizer(request_authorizer)
            .with_worker_id("worker-node-test"),
    )
    .with_deployment_config(coordinator_defaults)
    .with_manifest(manifest())
    .with_attempt_decorator_factory(Arc::new(move |context| {
        *observed.lock().expect("identity observation mutex") = Some(context.identity().clone());
        let decorator: awaken_session_executor::AttemptExecutorDecorator = Arc::new(|inner| inner);
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

    lifecycle_worker(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-warmup-lifecycle-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
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

    lifecycle_worker(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-current-environment-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
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

    lifecycle_worker(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-warmup-failure-test"),
    )
    .with_deployment_config(deployment)
    .with_session_container_provider_and_capacity(
        "external-secure",
        Arc::new(ExternalSessionProvider::default()),
        Some(capacity.clone()),
    )
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
async fn provider_evidence_drift_drains_the_incarnation() {
    // Cause/effect graph: C1 provider evidence holds before initial Ready; C2
    // the independent evidence probe fails; C3 no user shutdown.
    // Effects: E1 initial Ready; E2 no heartbeat after observed evidence loss; E3 the
    // existing authority-loss path drains and asks the supervisor to restart.
    // Rules P1 C1&&!C2=>normal (covered elsewhere); P2 !C1=>startup rejection;
    // P3 C1+C2+C3=>E1+E2+E3 (this test).
    let upstream = FakeWorkerUpstream::start();
    let ready = Arc::new(AtomicBool::new(true));
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        lifecycle_worker(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-provider-drift-test"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider::with_readiness(ready.clone())),
        )
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
    let sequences = upstream.heartbeat_sequences();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(upstream.heartbeat_sequences(), sequences, "E2");
    assert!(
        upstream
            .requests()
            .iter()
            .any(|path| path == "/v1/worker/drain"),
        "E3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_control_registration_withdraws_before_runtime_startup() {
    // C1 valid exact registered identity; C2 Control protocol declaration;
    // C3 registry TTL positive/zero. P0 zero TTL => same bounded withdrawal,
    // primary TTL error even when C2 is also invalid; no identity lost.
    // P1 missing/null/old/future/non-integer/out-of-range => preserve C1 for
    // one bounded withdrawal, return protocol error, no factories/probes/Ready/
    // claims. P2 current integer => normal startup (the lifecycle test above).
    // P3 includes a v3 Control: it cannot select whole-claim v4 execution.
    // C2 is never defaulted to current. Real HTTP exercises tolerant metadata
    // decoding and the single post-registration exit, not just a predicate.
    let current = awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION.to_string();
    let future = (awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION + 1).to_string();
    for zero_ttl in [false, true] {
        for declaration in [
            None,
            Some("null"),
            Some("0"),
            Some("1"),
            Some("2"),
            Some("3"),
            Some(future.as_str()),
            Some("-1"),
            Some("3.5"),
            Some("\"3\""),
            Some("4294967296"),
            Some("{}"),
            Some("[]"),
            Some("true"),
            Some(current.as_str()),
        ] {
            if !zero_ttl && declaration == Some(current.as_str()) {
                continue; // P2 current+positive is the normal lifecycle journey.
            }
            let upstream = if zero_ttl {
                FakeWorkerUpstream::start_with_zero_registry_ttl()
            } else {
                FakeWorkerUpstream::start()
            };
            upstream.set_registration_protocol(declaration);
            let provider = Arc::new(ExternalSessionProvider::default());
            let factories = Arc::new(AtomicUsize::new(0));
            let calls = factories.clone();
            let worker = lifecycle_worker(
                WorkerUpstream::new(upstream.url()).with_worker_id("protocol-test"),
            )
            .with_deployment_config(local_coordinator_deployment())
            .with_session_container_provider("external-secure", provider.clone())
            .with_attempt_decorator_factory(Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("protocol rejection must precede factory".into())
            }))
            .with_manifest(manifest())
            .without_admin_surface()
            .build()
            .unwrap();
            let error = tokio::time::timeout(
                Duration::from_secs(8),
                worker.run_until(std::future::pending()),
            )
            .await
            .expect("bounded withdrawal")
            .expect_err("incompatible Control");
            assert!(
                error.to_string().contains(if zero_ttl {
                    "zero lease TTL"
                } else {
                    "runtime protocol"
                }),
                "{declaration:?}/zero_ttl={zero_ttl}: {error}"
            );
            assert_eq!(factories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.probe_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                upstream.requests(),
                ["/v1/worker/register", "/v1/worker/deregister"],
                "{declaration:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_failure_and_cancellation_share_one_registration_exit() {
    // Cause/effect table, first failed stage wins (later stages cannot run):
    // S1 decorator error/unwind; S2 Memory factory error/unwind; S3 provider
    // error/unwind/stall; S4 shutdown success/error while S3 is pending.
    // All -> one registration and one exact deregistration, zero Ready/claims/
    // warmups. S4-success returns Ok; other rows return Err without an unwind.
    // Existing normal/expired-receipt tests cover successful startup and late
    // heartbeat delivery. Timeouts are failure, not a successful probe receipt.
    for fault in [
        "decorator-error",
        "decorator-panic",
        "memory-error",
        "memory-panic",
        "probe-error",
        "probe-panic",
        "probe-timeout",
        "shutdown",
        "shutdown-error",
    ] {
        let upstream = FakeWorkerUpstream::start();
        let provider = Arc::new(ExternalSessionProvider {
            ready: Some(Arc::new(AtomicBool::new(fault != "probe-error"))),
            probe_fault: match fault {
                "probe-panic" => Some((0, ProbeFault::Panic)),
                "probe-timeout" | "shutdown" | "shutdown-error" => Some((0, ProbeFault::Stall)),
                _ => None,
            },
            ..Default::default()
        });
        let mut builder =
            lifecycle_worker(WorkerUpstream::new(upstream.url()).with_worker_id(fault))
                .with_deployment_config(local_coordinator_deployment())
                .with_session_container_provider("external-secure", provider.clone())
                .with_manifest(manifest())
                .without_admin_surface();
        if fault.starts_with("decorator") {
            builder = builder.with_attempt_decorator_factory(Arc::new(move |_| {
                assert_ne!(fault, "decorator-panic", "injected startup factory unwind");
                Err("injected decorator error".into())
            }));
        }
        if fault.starts_with("memory") {
            builder = builder.with_registered_memory_mounter_factory(Arc::new(move |_| {
                assert_ne!(fault, "memory-panic", "injected startup factory unwind");
                Err("injected Memory factory error".into())
            }));
        }
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            builder.build().unwrap().run_until(async {
                if fault.starts_with("shutdown") {
                    while provider.probe_calls.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    if fault == "shutdown-error" {
                        return Err(std::io::Error::other("injected shutdown error").into());
                    }
                    return Ok(WorkerShutdown::Prompt);
                }
                std::future::pending().await
            }),
        )
        .await
        .expect("startup and withdrawal have bounded completion");
        assert_eq!(result.is_ok(), fault == "shutdown", "{fault}");
        assert_eq!(
            provider.active_probes.load(Ordering::SeqCst),
            0,
            "startup future is consumed on every exit"
        );
        let requests = upstream.requests();
        assert_eq!(
            requests,
            vec!["/v1/worker/register", "/v1/worker/deregister"],
            "{fault}"
        );
        if fault.starts_with("decorator") || fault.starts_with("memory") {
            assert_eq!(
                provider.probe_calls.load(Ordering::SeqCst),
                0,
                "later stage must not run"
            );
        } else {
            assert_eq!(
                provider.probe_calls.load(Ordering::SeqCst),
                1,
                "startup cannot retry a failed probe"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_withdrawal_receipts_are_exact_bounded_and_preserve_primary_failure() {
    // W1 local cooperative stop + applied/absent/replaced exact registration ->
    // success; W2 rejected/malformed/missing reply -> Err, never false cleanup.
    // W3 primary startup error crosses every withdrawal result unchanged.
    // Each row sends one request; timeout permits local exit but is NOT a
    // remote removal receipt. WorkerDrain models this as outcome="unknown".
    for primary_failure in [false, true] {
        for (reply, confirmed) in [
            (Some(r#"{"mutation":"applied"}"#), true),
            (Some(r#"{"mutation":"not_found"}"#), true),
            (Some(r#"{"mutation":"stale_incarnation"}"#), true),
            (Some(r#"{"mutation":"invalid_transition"}"#), false),
            (Some(r#"{"mutation":"stale_sequence"}"#), false),
            (Some("{}"), false),
            (None, false),
        ] {
            let upstream = FakeWorkerUpstream::start();
            upstream.set_deregistration_response(reply.map(str::to_owned));
            let provider = Arc::new(ExternalSessionProvider {
                probe_fault: Some((0, ProbeFault::Stall)),
                ..Default::default()
            });
            let mut builder =
                lifecycle_worker(WorkerUpstream::new(upstream.url()).with_worker_id("withdrawal"))
                    .with_deployment_config(local_coordinator_deployment())
                    .with_session_container_provider("external-secure", provider.clone())
                    .with_manifest(manifest())
                    .without_admin_surface();
            if primary_failure {
                builder = builder.with_attempt_decorator_factory(Arc::new(|_| {
                    Err("primary failure sentinel".into())
                }));
            }
            let result = tokio::time::timeout(
                Duration::from_secs(8),
                builder.build().unwrap().run_until(async {
                    if primary_failure {
                        return std::future::pending().await;
                    }
                    while provider.probe_calls.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    // Cancel the pending probe, not a race with a ready response.
                    Ok(WorkerShutdown::Prompt)
                }),
            )
            .await
            .expect("one bounded withdrawal attempt");
            assert_eq!(result.is_ok(), !primary_failure && confirmed, "{reply:?}");
            if primary_failure {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("primary failure sentinel")
                );
            }
            assert_eq!(
                upstream.requests(),
                vec!["/v1/worker/register", "/v1/worker/deregister"]
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_initial_heartbeat_cannot_start_dispatch() {
    // W15 real HTTP startup: registry applies sequence 1, but delays its response
    // beyond the request-anchored proof window. E1 reject before enabling claim
    // polling; E2 deregister exactly once; E3 no periodic heartbeat or warmup.
    // H1 expiry and H2 observed shutdown while the initial Ready response is
    // pending both withdraw exactly once without enabling dispatch. Remote
    // delivery before cancellation is allowed; it cannot revive the local task.
    // The bounded startup request now expires BEFORE the delayed receipt can
    // be installed. This HTTP row owns timeout/withdrawal ordering, not receipt
    // arithmetic (the WorkerAuthorityProof decision table owns that boundary).
    for cancelled in [false, true] {
        let upstream = FakeWorkerUpstream::start_with_expired_initial_receipt();
        upstream.block_initial_heartbeat(cancelled);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            lifecycle_worker(
                WorkerUpstream::new(upstream.url()).with_worker_id("expired-initial-receipt"),
            )
            .with_deployment_config(local_coordinator_deployment())
            .with_manifest(manifest())
            .without_admin_surface()
            .build()
            .unwrap()
            .run_until(async {
                if cancelled {
                    while upstream.heartbeat_sequences().is_empty() {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    // The response was held until this shutdown branch wins;
                    // no scheduler-speed assumption chooses the interleaving.
                    upstream.block_initial_heartbeat(false);
                    return Ok(WorkerShutdown::Prompt);
                }
                std::future::pending().await
            }),
        )
        .await
        .unwrap();
        if cancelled {
            assert!(
                result.is_ok(),
                "cancelled startup withdraws without claiming"
            );
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("initial worker heartbeat timed out"),
                "unexpected startup failure: {error}"
            );
        }
        assert_eq!(upstream.heartbeat_sequences(), vec![1], "E3");
        let requests = upstream.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|path| path.as_str() == "/v1/worker/deregister")
                .count(),
            1,
            "E2"
        );
        assert!(
            !requests
                .iter()
                .any(|path| path.contains("/dispatch/") || path.contains("/warmups")),
            "E1/E3"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_provider_probe_does_not_block_renewal_but_expires() {
    // Cause/effect rule P4: startup evidence succeeds, then the same provider
    // probe never returns. Registry renewal progresses independently while the
    // initial environment proof remains valid. Probe timeouts never refresh
    // that proof: it expires and the same supervisor irreversibly drains.
    let upstream = FakeWorkerUpstream::start_with_short_registry_lease();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        lifecycle_worker(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-provider-stall-test"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider::stalling_after_startup()),
        )
        .with_manifest(manifest())
        .with_graceful_drain(Duration::ZERO)
        .without_admin_surface()
        .build()
        .expect("valid provider-evidence topology")
        .run_until(std::future::pending()),
    )
    .await
    .expect("P4 bounded provider probe terminates")
    .expect_err("P4 supervisor replacement required");
    assert!(
        result.to_string().contains("supervisor restart required"),
        "P4"
    );
    assert!(
        upstream.heartbeat_sequences().len() >= 2,
        "P4 renewal is not blocked by the probe"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_provider_timeout_recovers_without_incarnation_restart() {
    // Rule P5: startup passes; one probe exceeds its request budget; retry
    // succeeds before the old proof expires. Effects: concurrent registry
    // renewals continue, subsequent probes recover, and only user shutdown
    // terminates. This contrasts P4 (all retries stall) and P3 (explicit loss).
    let upstream = FakeWorkerUpstream::start_with_short_registry_lease();
    let provider = Arc::new(ExternalSessionProvider {
        probe_fault: Some((1, ProbeFault::StallOnce)),
        ..Default::default()
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        WorkerNodeBuilder::new(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-provider-recovery-test"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_session_container_provider("external-secure", provider.clone())
        .with_hand_executor_factory(Arc::new(NoHandFactory))
        .with_manifest(manifest())
        .with_graceful_drain(Duration::ZERO)
        .without_admin_surface()
        .build()
        .unwrap()
        .run_until(async {
            loop {
                if provider.probe_calls.load(Ordering::SeqCst) >= 4
                    && upstream.heartbeat_sequences().len() >= 4
                {
                    return Ok(WorkerShutdown::Prompt);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }),
    )
    .await
    .expect("P5 bounded recovery")
    .expect("P5 no authority loss");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_initial_provider_probe_never_publishes_ready() {
    // Rule P2: unlike P4 no initial evidence exists. An unknown startup probe
    // has no grace proof to reuse: bound it, reject startup, and emit no Ready.
    let upstream = FakeWorkerUpstream::start_with_short_registry_lease();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        WorkerNodeBuilder::new(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-provider-startup-stall"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_session_container_provider(
            "external-secure",
            Arc::new(ExternalSessionProvider {
                probe_fault: Some((1, ProbeFault::Stall)),
                probe_calls: AtomicUsize::new(1),
                ..Default::default()
            }),
        )
        .with_hand_executor_factory(Arc::new(NoHandFactory))
        .with_manifest(manifest())
        .without_admin_surface()
        .build()
        .unwrap()
        .run_until(std::future::pending()),
    )
    .await
    .expect("P2 startup is bounded")
    .expect_err("P2 absent evidence rejects startup");
    assert!(result.to_string().contains("timed out before Ready"), "P2");
    assert!(upstream.heartbeat_sequences().is_empty(), "P2 no Ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_heartbeat_probe_uses_authority_loss_and_requires_replacement() {
    // P5: startup Ready succeeds, then a real periodic probe unwinds while
    // there is no user shutdown. E1 no subsequent heartbeat/loop restart;
    // E2 normal drain+deregister lifecycle; E3 caller requires a new incarnation.
    // This drives actual Worker bootstrap and HTTP transport. The supervisor's
    // barrier test proves exact-once revocation before task completion; Host
    // revocation tests own active Session/physical cleanup evidence separately.
    let upstream = FakeWorkerUpstream::start_with_short_registry_lease();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        lifecycle_worker(WorkerUpstream::new(upstream.url()).with_worker_id("worker-probe-panic"))
            .with_deployment_config(local_coordinator_deployment())
            .with_session_container_provider(
                "external-secure",
                Arc::new(ExternalSessionProvider {
                    probe_fault: Some((1, ProbeFault::Panic)),
                    ..ExternalSessionProvider::default()
                }),
            )
            .with_manifest(manifest())
            .with_graceful_drain(Duration::ZERO)
            .without_admin_surface()
            .build()
            .unwrap()
            .run_until(std::future::pending()),
    )
    .await
    .expect("P5 worker exits after panic")
    .unwrap_err();
    assert!(
        result.to_string().contains("supervisor restart required"),
        "E3"
    );
    assert_eq!(upstream.heartbeat_sequences(), vec![1], "E1");
    let requests = upstream.requests();
    let drain = requests
        .iter()
        .position(|path| path == "/v1/worker/drain")
        .unwrap();
    let deregister = requests
        .iter()
        .position(|path| path == "/v1/worker/deregister")
        .unwrap();
    assert!(drain < deregister, "E2");
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
        lifecycle_worker(
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
async fn transient_heartbeat_failure_pauses_then_resumes_claim_admission() {
    // Cause/effect decision table for uncertain Worker-registry transport:
    //
    // | Rule | prior proof valid | heartbeat result | effect |
    // |---|---|---|---|
    // | H1 | yes | transport 503 | `/readyz`=503; in-flight authority retained |
    // | H2 | yes | later Applied before proof deadline | admission resumes; process lives |
    // | H3 | yes | graceful shutdown after H2 | ordinary terminal lifecycle |
    let (upstream, recovery_release) =
        FakeWorkerUpstream::start_transient_heartbeat_with_blocked_recovery();
    let admin_addr = format!("127.0.0.1:{}", free_port());
    lifecycle_worker(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-transient-heartbeat-test"),
    )
    .with_deployment_config(local_coordinator_deployment())
    .with_manifest(manifest())
    .with_admin_listen(&admin_addr)
    .build()
    .expect("valid Worker topology")
    .run_until(async {
        assert!(
            poll_status(&admin_addr, "GET", "/readyz", 200).await,
            "H1 precondition"
        );
        for _ in 0..400 {
            if upstream.heartbeat_sequences().len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(http_status(&admin_addr, "GET", "/readyz"), Some(503), "H1");
        recovery_release.store(true, Ordering::Release);
        assert!(poll_status(&admin_addr, "GET", "/readyz", 200).await, "H2");
        Ok(WorkerShutdown::Prompt)
    })
    .await
    .expect("H3 transient registry failure does not kill the incarnation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_applied_heartbeat_response_retries_with_a_higher_sequence() {
    // Cause/effect decision table for an ambiguous response:
    //
    // | Rule | server applied | response observed | retry effect |
    // |---|---|---|---|
    // | S1 | yes | connection closes | suspend admission and retry |
    // | S2 | yes | later Applied | strictly higher sequence converges; resume |
    let upstream = FakeWorkerUpstream::start_dropping_one_heartbeat_response();
    lifecycle_worker(
        WorkerUpstream::new(upstream.url()).with_worker_id("worker-lost-heartbeat-response-test"),
    )
    .with_deployment_config(local_coordinator_deployment())
    .with_manifest(manifest())
    .without_admin_surface()
    .build()
    .expect("valid Worker topology")
    .run_until(async {
        for _ in 0..800 {
            if upstream.heartbeat_sequences().len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let sequences = upstream.heartbeat_sequences();
        assert!(sequences.len() >= 3, "S1/S2: {sequences:?}");
        assert_eq!(&sequences[..3], &[1, 2, 3], "S2");
        assert_eq!(upstream.applied_heartbeat_sequence(), 3, "S1/S2");
        Ok(WorkerShutdown::Prompt)
    })
    .await
    .expect("S2 ambiguous response converges without process replacement");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_transport_failure_drains_only_after_the_proof_deadline() {
    // Cause/effect decision table for proof exhaustion:
    //
    // | Rule | heartbeat transport | proof window | effect |
    // |---|---|---|---|
    // | P1 | unavailable | remaining | retry; do not terminate immediately |
    // | P2 | unavailable | exhausted | drain/cancel and require supervisor restart |
    let upstream = FakeWorkerUpstream::start_unavailable_after_initial_heartbeat();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        lifecycle_worker(
            WorkerUpstream::new(upstream.url()).with_worker_id("worker-proof-deadline-test"),
        )
        .with_deployment_config(local_coordinator_deployment())
        .with_manifest(manifest())
        .without_admin_surface()
        .build()
        .expect("valid Worker topology")
        .run_until(std::future::pending()),
    )
    .await
    .expect("P2 proof exhaustion terminates")
    .expect_err("P2 supervisor restart is required");
    assert!(
        result.to_string().contains("supervisor restart required"),
        "P2"
    );
    assert!(
        upstream.heartbeat_sequences().len() > 2,
        "P1 retries before P2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_closes_local_admission_before_control_acknowledges() {
    let (upstream, drain_release) = FakeWorkerUpstream::start_with_blocked_drain();
    let admin_addr = format!("127.0.0.1:{}", free_port());
    let coordinator_defaults = local_coordinator_deployment();

    lifecycle_worker(WorkerUpstream::new(upstream.url()).with_worker_id("worker-drain-order-test"))
        .with_deployment_config(coordinator_defaults)
        .with_manifest(manifest())
        .with_admin_listen(&admin_addr)
        .build()
        .expect("valid explicit Worker topology")
        .run_until(async {
            assert!(
                poll_status(&admin_addr, "GET", "/readyz", 200).await,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_observation_publication_budget_withdraws_or_drains() {
    // C1 authored P=10ms,T=400ms pass local validation; C2 the Coordinator
    // negotiates 600ms lease (H=200,R=100, sum=310) or 900ms (sum=460).
    // C3 negotiation changes at initial receipt or after claims have started.
    // E1 600ms admits ordinary work; E2 initial 900ms withdraws once without
    // claims; E3 later 900ms drains and terminates the incarnation once. No
    // implicit TTL widening, retry-to-rejoin, or second observation publisher.
    // Rules: B1 initial incompatible => E2; B2 compatible -> incompatible =>
    // E1 then E3. Pure table owns strict equality/overflow/zero boundaries.
    for initial_incompatible in [true, false] {
        let upstream = FakeWorkerUpstream::start_with_short_registry_lease();
        if initial_incompatible {
            upstream.set_lease_ttl_ms(900);
        }
        let worker = lifecycle_worker(WorkerUpstream::new(upstream.url()))
            .with_manifest(manifest())
            .with_deployment_config(local_coordinator_deployment())
            .with_credential_observation_window(
                Duration::from_millis(10),
                Duration::from_millis(400),
            )
            .build()
            .unwrap();
        let task = tokio::spawn(worker.run_until(std::future::pending()));
        if !initial_incompatible {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !upstream
                    .requests()
                    .iter()
                    .any(|path| path.contains("/dispatch/claim"))
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("B2 compatible budget admits claims");
            upstream.set_lease_ttl_ms(900);
        }
        let result = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .expect("bounded termination")
            .unwrap();
        if initial_incompatible {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("heartbeat interval"),
                "B1 diagnostic"
            );
        }
        let paths = upstream.requests();
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.as_str() == "/v1/worker/deregister")
                .count(),
            1
        );
        assert_eq!(
            paths.iter().any(|p| p.contains("/dispatch/claim")),
            !initial_incompatible
        );
        assert_eq!(
            paths.iter().any(|p| p == "/v1/worker/drain"),
            !initial_incompatible
        );
    }
}
