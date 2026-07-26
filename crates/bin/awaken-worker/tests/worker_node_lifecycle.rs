use awaken_runtime_host::{SignedWorkerRequestAuthorizer, WorkerSigningCredential, WorkerUpstream};
use awaken_worker::{StandardManifestConfig, WorkerNodeBuilder, WorkerShutdown};
use awaken_worker_contract::{VersionRange, WorkerManifest};
use std::sync::{Arc, Mutex};

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

struct ExternalSessionProvider;

#[async_trait::async_trait]
impl awaken_runtime_host::ContainerEnvironmentProvider for ExternalSessionProvider {
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

    async fn create_environment(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_runtime_host::ContainerEnvironment>,
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
        Arc<dyn awaken_runtime_host::ContainerEnvironment>,
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
        awaken_runtime_host::AcpWorkerProfile::new(vec!["codex".to_string()], None)
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
fn standard_manifest_uses_the_installed_session_provider_as_authority() {
    let worker = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_session_container_provider("external-secure", Arc::new(ExternalSessionProvider))
        .with_standard_manifest(Default::default())
        .build()
        .expect("external provider is a complete standard topology");

    assert_eq!(
        worker.manifest().sandbox_backends,
        std::collections::BTreeSet::from(["external-secure".to_string()])
    );
    assert!(worker.manifest().sandbox.secret_egress_substitution);
    assert!(worker.manifest().sandbox.enforced_network_allowlist);

    let empty_backend = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_session_container_provider(" ", Arc::new(ExternalSessionProvider))
        .with_standard_manifest(Default::default())
        .build()
        .err()
        .expect("provider identity is required");
    assert!(empty_backend.to_string().contains("backend"));
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
    let mut coordinator_defaults = awaken_runtime_host::DeploymentConfig::ephemeral();
    coordinator_defaults.disable_local_pool = true;
    WorkerNodeBuilder::new(
        WorkerUpstream::new(upstream.url())
            .with_request_authorizer(request_authorizer)
            .with_worker_id("worker-node-test"),
    )
    .with_deployment_config(coordinator_defaults)
    .with_manifest(manifest())
    .with_application_factory(Arc::new(move |context| {
        *observed.lock().expect("identity observation mutex") = Some(context.identity().clone());
        let decorator: awaken_runtime_host::AttemptExecutorDecorator = Arc::new(|inner| inner);
        Ok(awaken_worker::RegisteredWorkerApplication::new(decorator))
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
