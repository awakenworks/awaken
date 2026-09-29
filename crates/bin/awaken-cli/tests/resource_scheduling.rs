//! Integrated proof that the public policy contract is the only source of
//! Kubernetes resource requests. The test crosses the real Control authoring,
//! Environment registration, Session freeze, Host provisioning, container
//! planning, and Kubernetes Pod projection boundaries.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_cli::build_all_in_one_router_with_host_customizer;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::tool::ToolExecutor;
use awaken_sandbox_container::{ContainerEnvironment, ContainerEnvironmentProvider};
use awaken_scenario_host::EchoModel;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct RecordingProvider {
    specs: Arc<Mutex<Vec<pc::SandboxSpec>>>,
}

#[test]
fn recording_provider_cannot_advertise_process_stop_evidence() {
    // RP1 a scheduling-only fixture has no physical process supervisor => it
    // must not gain stop-proof capability when SandboxCapabilities evolves.
    let provider = RecordingProvider {
        specs: Arc::new(Mutex::new(Vec::new())),
    };
    assert!(!provider.sandbox_capabilities().process_tree_supervision);
}

#[async_trait]
impl ContainerEnvironmentProvider for RecordingProvider {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    async fn observe_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }
    async fn adopt_environment_for_effect(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment adoption",
        ))
    }
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_sandbox_container::ContainerEffectFence>,
        _terminal_effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Container,
            process_tree_supervision: false,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: true,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: true,
            package_provisioning: true,
            control_services: Default::default(),
        }
    }

    async fn create_unattached_environment(
        &self,
        _spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        panic!("managed Session creation must use the fenced provider entrance");
    }

    async fn create_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        fence: &pc::SandboxEffectFence,
        source: &dyn pc::CurrentExecutionAuthority,
        intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        assert!(matches!(
            intent,
            awaken_container_contract::EnvironmentRealizationIntent::Create
        ));
        pc::with_current_sandbox_effect(fence, source, |_| {
            let mut specs = self.specs.lock().unwrap();
            if let Some(prior) = specs.iter().find(|prior| prior.scope == spec.scope) {
                assert_eq!(
                    prior.requests, spec.requests,
                    "retry preserves frozen requests"
                );
                assert_eq!(prior.limits, spec.limits, "retry preserves frozen limits");
            } else {
                specs.push(spec.clone());
            }
            Ok(())
        })?;
        // This test owns projection, not process creation or stop evidence.
        // End the probe here; no fabricated SandboxHandle/remote Hand is needed.
        Err(pc::SandboxError::backend(
            "create_environment_for_effect",
            "resource projection probe complete",
        ))
    }

    async fn adopt_unattached_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::invalid(
            "resource proof does not recover a Session",
        ))
    }
}

struct UnusedHandFactory;

impl awaken_session_environment_contract::HandExecutorFactory for UnusedHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        panic!("resource projection stops before Hand execution")
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn pod_requests(spec: &pc::SandboxSpec) -> (String, String, String) {
    let plan = awaken_sandbox_container::container_plan(
        spec,
        "ghcr.io/awaken/sandbox:test",
        &["sleep".into(), "infinity".into()],
        None,
    )
    .expect("the frozen policy produces one valid container plan");
    let pod = awaken_sandbox_container::k8s::pod_for_plan("resource-proof", &plan);
    let agent = pod
        .spec
        .expect("Kubernetes PodSpec")
        .containers
        .into_iter()
        .find(|container| container.name == "agent")
        .expect("agent container");
    let requests = agent
        .resources
        .expect("resource requirements")
        .requests
        .expect("requests map");
    (
        requests.get("cpu").unwrap().0.clone(),
        requests.get("memory").unwrap().0.clone(),
        requests.get("ephemeral-storage").unwrap().0.clone(),
    )
}

async fn trigger_session_provisioning(app: &Router, session_id: &str, rule: &str) {
    let (status, _) = call(
        app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "provision resources" }]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rule} trigger Run");
}

async fn wait_for_spec(
    app: &Router,
    session_id: &str,
    specs: &Arc<Mutex<Vec<pc::SandboxSpec>>>,
    count: usize,
) -> pc::SandboxSpec {
    for _ in 0..200 {
        if let Some(spec) = specs.lock().unwrap().get(count - 1).cloned() {
            return spec;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let (_, session) = call(app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    let (_, events) = call(
        app,
        "GET",
        &format!("/v1/sessions/{session_id}/events"),
        None,
    )
    .await;
    panic!("Host did not provision sandbox {count}; session={session}; events={events}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_http_policy_reaches_session_and_kubernetes_pod_requests() {
    // Cause/effect graph:
    // C1 policy v1 is authored over HTTP with requests+limits; C2 Environment
    // binds exact v1; C3 v2 is later published with different requests; C4 a
    // Session Run is triggered before rebinding; C5 the Environment is rebound
    // to v2 and a second Session Run is triggered. Effects: E1 Host receives v1
    // requests for C4 despite current policy v2; E2 container planning preserves
    // them; E3 the Kubernetes agent Pod carries exact cpu/memory/disk requests;
    // E4 C5 receives v2 requests. Constraint: no test writes a store or
    // constructs an EnvironmentSnapshot/SandboxSpec directly.
    //
    // | Rule | binding | current policy | Session | Pod requests |
    // | R1 | v1 | v2 | first | exact v1 |
    // | R2 | v2 | v2 | second | exact v2 |
    // RP2 both records must cross the current operation-bound authority before
    // recording. The unfenced entrance panics; the probe intentionally stops
    // before physical creation. Retries are checked against the same scope's
    // first frozen values and cannot be mistaken for a second Session.
    let specs = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider {
        specs: specs.clone(),
    });
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_session_executor::SandboxTier::Local;
    let app = build_all_in_one_router_with_host_customizer(
        Arc::new(EchoModel),
        ModelBinding::new("host", "echo", "genai"),
        deployment,
        None,
        move |host| host.with_session_container_provider(provider, Arc::new(UnusedHandFactory)),
    )
    .await;

    let (status, _) = call(
        &app,
        "POST",
        "/v1/awaken/sandbox-execution-policies",
        Some(json!({
            "id": "resource-proof",
            "config": {
                "isolation": "container",
                "requests": {
                    "cpu_millis": 250,
                    "memory_bytes": 33_554_432u64,
                    "disk_bytes": 67_108_864u64
                },
                "limits": { "memory_bytes": 134_217_728u64 }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "R1 create v1");

    let (status, environment) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "resource-proof" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 Environment");
    let environment_id = environment["id"].as_str().unwrap();
    let binding_path = format!("/v1/awaken/environments/{environment_id}/sandbox-execution-policy");
    let (status, _) = call(
        &app,
        "POST",
        &binding_path,
        Some(json!({ "policy_id": "resource-proof", "version": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 bind v1");

    let (status, _) = call(
        &app,
        "POST",
        "/v1/awaken/sandbox-execution-policies/resource-proof/versions",
        Some(json!({
            "expected_current": 1,
            "config": {
                "isolation": "container",
                "requests": {
                    "cpu_millis": 900,
                    "memory_bytes": 268_435_456u64,
                    "disk_bytes": 536_870_912u64
                },
                "limits": { "memory_bytes": 536_870_912u64 }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 publish v2");

    let (status, first_session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "default", "environment_id": environment_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1 first Session");
    trigger_session_provisioning(&app, first_session["id"].as_str().unwrap(), "R1").await;
    let v1 = wait_for_spec(&app, first_session["id"].as_str().unwrap(), &specs, 1).await;
    assert_eq!(
        pod_requests(&v1),
        ("250m".into(), "33554432".into(), "67108864".into()),
        "R1 exact v1 survives policy v2"
    );

    let (status, _) = call(
        &app,
        "POST",
        &binding_path,
        Some(json!({ "policy_id": "resource-proof", "version": 2 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R2 bind v2");
    let (status, second_session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "default", "environment_id": environment_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R2 second Session");
    trigger_session_provisioning(&app, second_session["id"].as_str().unwrap(), "R2").await;
    let v2 = wait_for_spec(&app, second_session["id"].as_str().unwrap(), &specs, 2).await;
    assert_eq!(
        pod_requests(&v2),
        ("900m".into(), "268435456".into(), "536870912".into()),
        "R2 exact v2"
    );
}
