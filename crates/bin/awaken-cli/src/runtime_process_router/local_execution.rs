// @formal evidence assembly_role.local_managed_mount_requires_exact_local_worker_component
//! Install physical execution only for the selected local Session placement.

use awaken_session_application::SessionExecutionPlacement;
use awaken_session_executor::{DeploymentConfig, SessionExecutorBuilder};
use std::path::PathBuf;

pub(super) async fn prepare(
    host: SessionExecutorBuilder,
    deployment: &DeploymentConfig,
    placement: SessionExecutionPlacement,
    namespace_hand_path: Option<PathBuf>,
    credentials: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> Result<SessionExecutorBuilder, String> {
    let host = if placement == SessionExecutionPlacement::LocalWorker {
        let hand_factory = awaken_worker::relay_hand_executor_factory();
        let containers = awaken_execution_composition::ContainerTierPolicy::default();
        let host = match namespace_hand_path {
            Some(path) => {
                awaken_execution_composition::with_session_environment_from_deployment_at(
                    host,
                    deployment,
                    Some(hand_factory),
                    path,
                    &containers,
                )
                .await?
            }
            None => {
                awaken_execution_composition::with_session_environment_from_deployment(
                    host,
                    deployment,
                    Some(hand_factory),
                    &containers,
                )
                .await?
            }
        };
        awaken_execution_composition::with_acp_from_deployment(host, deployment, credentials, None)
    } else {
        // Registered-Worker placement needs original-source observation, but
        // keeps every execution/Hand/capacity role unavailable on this process.
        let host =
            awaken_execution_composition::with_session_environment_observation_from_deployment(
                host, deployment,
            )
            .await?;
        host.with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy())
    };
    Ok(host)
}

#[cfg(all(test, feature = "container-k8s"))]
mod tests {
    use super::*;
    use awaken_provisioning_contract as pc;
    use std::sync::Arc;

    struct NoModel;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoModel {
        async fn infer(
            &self,
            _: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("Coordinator observation cannot execute a model")
        }
    }

    fn spec() -> pc::SandboxSpec {
        pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "source-observation".into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: vec!["unused".into()],
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
            control_services: Default::default(),
            lease_ttl_secs: None,
        }
    }

    async fn assert_observation_roles(package_builder: bool, implicit_fallback: bool) {
        let mut deployment = DeploymentConfig::ephemeral();
        deployment.disable_local_pool = true;
        deployment.sandbox_tier = awaken_session_executor::SandboxTier::K8s;
        deployment.container_image = Some(format!(
            "registry.invalid/sandbox@sha256:{}",
            "0".repeat(64)
        ));
        deployment.sandbox.k8s_namespace = "source-observation".into();
        deployment.sandbox.warm_pool_size = 2;
        if package_builder {
            deployment.sandbox.package_image_builder =
                Some(awaken_session_executor::PackageImageBuilder::Kubernetes);
            deployment.sandbox.package_image_registry = None;
        }
        let host = if implicit_fallback {
            awaken_session_executor_testkit::volatile_dependencies_with_deployment(
                Arc::new(NoModel),
                "unused",
                deployment.clone(),
            )
            .await
        } else {
            awaken_session_executor_testkit::unconfigured_dependencies_with_deployment(
                Arc::new(NoModel),
                "unused",
                deployment.clone(),
            )
            .await
        };
        assert!(
            !host.session_environment_provider_installed(),
            "RCO initial state"
        );
        assert_eq!(
            host.installed_session_environment_provider()
                .realizer()
                .effective_spec(&spec())
                .is_err(),
            !implicit_fallback,
            "RCO must preserve the selected pre-installation state"
        );
        let host = prepare(
            host,
            &deployment,
            SessionExecutionPlacement::RegisteredWorker,
            None,
            None,
        )
        .await
        .expect("RCO canonical Coordinator assembly");
        let provider = host.installed_session_environment_provider();
        let effective = provider
            .realizer()
            .effective_spec(&spec())
            .expect("RCO selected container metadata must be observable");
        assert_eq!(effective.isolation, pc::IsolationClass::Container, "RCO R1");
        assert!(
            !host.session_environment_provider_installed(),
            "RCO no execution install"
        );
        assert!(
            matches!(
                provider
                    .realizer()
                    .create_effective_for_effect(
                        &effective,
                        pc::SandboxEffect::Unattached,
                        None,
                        awaken_session_environment_contract::EnvironmentRealizationKind::Create,
                    )
                    .await,
                Err(pc::SandboxError::Unsupported(..))
            ),
            "RCO R2 create"
        );
        let handle = pc::SandboxHandle::new("unused", "no-physical-source");
        assert!(
            matches!(
                provider
                    .adoption()
                    .adopt_effective_for_effect(&effective, &handle, pc::SandboxEffect::Unattached,)
                    .await,
                Err(pc::SandboxError::Unsupported(..))
            ),
            "RCO R2 adopt"
        );
        assert!(
            provider
                .adoption()
                .effective_adoption_layout(&effective, &handle)
                .is_err()
        );
        assert!(
            provider
                .adoption()
                .validate_closed_observation(&handle, &pc::SandboxObservation::Ready,)
                .is_err()
        );
        assert!(matches!(
            provider
                .capacity()
                .prewarm_images(std::slice::from_ref(&effective))
                .await,
            Err(pc::SandboxError::NotReady(_))
        ));
        let fence = pc::SandboxEffectFence::new("retention", "owner", "boot", 3, u64::MAX).unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        assert!(
            matches!(
                provider
                    .terminal()
                    .prepare_terminal_effective_for_effect(
                        &effective,
                        Some(&handle),
                        Some(&fence),
                        &fence,
                        &authority,
                    )
                    .await,
                Err(pc::SandboxError::NotReady(_))
            ),
            "RCO implicit execution fallback was removed"
        );
    }

    fn is_policy_read(request: &str) -> bool {
        let mut parts = request.split_whitespace();
        parts.next() == Some("GET")
            && parts.next().and_then(|path| path.split('?').next())
                == Some("/apis/networking.k8s.io/v1/namespaces/source-observation/networkpolicies")
    }

    fn serve_policy_reads(
        endpoint: std::net::TcpListener,
        stop: Arc<std::sync::atomic::AtomicBool>,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            use std::sync::atomic::Ordering;
            endpoint.set_nonblocking(true).unwrap();
            let body = serde_json::to_vec(&serde_json::json!({
                "apiVersion":"networking.k8s.io/v1", "kind":"NetworkPolicyList",
                "metadata":{"resourceVersion":"1"},
                "items":[
                    {"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy",
                     "metadata":{"name":"awaken-sandbox-default-deny"},
                     "spec":{"podSelector":{"matchLabels":{"app":"awaken-sandbox"}},
                             "policyTypes":["Ingress","Egress"],"ingress":[],"egress":[]}},
                    {"apiVersion":"networking.k8s.io/v1","kind":"NetworkPolicy",
                     "metadata":{"name":"awaken-sandbox-open-egress"},
                     "spec":{"podSelector":{"matchLabels":{"app":"awaken-sandbox","awaken-egress":"open"}},
                             "policyTypes":["Egress"],"egress":[{}]}}
                ]
            })).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match endpoint.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("RCO fixture accept: {error}"),
                };
                // Keep the accept loop nonblocking; request reads are bounded blocking I/O.
                // Windows accepted streams can inherit the listener mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut header = String::new();
                loop {
                    header.clear();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                        break;
                    }
                }
                let accepted = is_policy_read(&request);
                requests.lock().unwrap().push(request.trim().to_owned());
                let status = if accepted {
                    "200 OK"
                } else {
                    "500 Unexpected Test Request"
                };
                write!(stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                ).unwrap();
                stream.write_all(&body).unwrap();
            }
        })
    }

    #[test]
    fn registered_worker_observation_does_not_install_execution() {
        // Cause/effect graph RCO: C1 registered-Worker placement; C2 K8s tier;
        // C3 the initial provider is uninstalled/an implicit Workdir fallback; C4 warm
        // capacity is configured; C5 an unusable write-only package builder is
        // absent/present. R1 C1+C2+C4 with either C3/C5 -> selected container metadata
        // is readable, but no Hand/local execution provider is installed.
        // R2 create/adopt/layout/closed validation/capacity calls stay refused
        // before backend I/O. Even construction may not probe or warm execution.
        // A child owns KUBECONFIG so this test never races global environment
        // changes. Its isolated API permits only the existing namespace policy
        // attestation LIST; every extra probe or write is recorded and refused.
        const CHILD: &str = "AWAKEN_TEST_COORDINATOR_OBSERVER_CHILD";
        const NAME: &str = "runtime_process_router::local_execution::tests::registered_worker_observation_does_not_install_execution";
        if std::env::var_os(CHILD).is_some() {
            awaken_service_lifecycle::run_composed_async_test(|| async {
                for package_builder in [false, true] {
                    for implicit_fallback in [false, true] {
                        assert_observation_roles(package_builder, implicit_fallback).await;
                    }
                }
            });
            return;
        }
        let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = endpoint.local_addr().unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = serve_policy_reads(endpoint, stop.clone(), requests.clone());
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("kubeconfig.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "apiVersion":"v1", "kind":"Config",
                "clusters":[{"name":"observation","cluster":{
                    "server":format!("http://{address}")
                }}],
                "users":[{"name":"observation","user":{}}],
                "contexts":[{"name":"observation","context":{
                    "cluster":"observation","user":"observation","namespace":"source-observation"
                }}],
                "current-context":"observation"
            }))
            .unwrap(),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .env("KUBECONFIG", &config)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env_remove("KUBERNETES_SERVICE_HOST")
            .env_remove("KUBERNETES_SERVICE_PORT")
            .output()
            .unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();
        assert!(
            output.status.success(),
            "RCO child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            4,
            "RCO exactly one existing attestation read per assembly"
        );
        assert!(
            requests.iter().all(|request| is_policy_read(request)),
            "RCO no execution probes or backend writes"
        );
    }
}
