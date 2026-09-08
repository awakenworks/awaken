use super::*;
use awaken_sandbox_container::k8s::{PodProcessObserver, PodProcessTermination};

#[tokio::test]
async fn independent_observer_requires_exact_runtime_exit_and_retains_missing_history() {
    // Live decision table (production adapter, real kubelet/CRI; not a Dispatch
    // recovery claim): KPT1 pre-bound running domain => Unknown; KPT2 PID-1
    // SIGKILL with another container live => Unknown; KPT3 every container exits
    // => same fact; KPT4 delayed exec => rejected by the terminated domain;
    // KPT5 API deletion after exit => Unknown, not reconstructed history.
    // The dedicated k3d harness owns infrastructure; no ambient cluster mutation
    // is permitted unless the existing explicit live-suite gate is enabled.
    if !require_live_cluster() {
        return;
    }
    let cluster = std::env::var("AWAKEN_K3D_CLUSTER")
        .expect("explicit isolated k3d cluster for CRI fault injection");
    let context = kubectl(&["config", "current-context"]);
    assert_eq!(
        String::from_utf8(context.stdout).unwrap().trim(),
        format!("k3d-{cluster}"),
        "never inject faults into an ambient cloud context"
    );
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let runtime = Arc::new(
        K8sRuntime::connect(&namespace, "127.0.0.1:1".parse().unwrap())
            .await
            .unwrap(),
    );
    let provider = ContainerProvider::new(runtime.clone(), fixture_image());
    let scope = format!("process-stop-{}", std::process::id());
    let mut spec = spec_with_command(&scope, fixture_image(), session_argv());
    spec.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
    let sandbox = provider
        .create_container(&spec)
        .await
        .expect("create exact test process domain");
    let pod = pod_of(&sandbox);
    let client = kube::Client::try_default().await.unwrap();
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(client.clone(), &namespace);
    let observer = PodProcessObserver::new(client, &namespace).unwrap();
    let finalizer = "test.awaken.dev/retain-process-stop-evidence";
    k8s_common::set_test_finalizer(&pods, &pod, finalizer, true).await;
    let uid = pods.get(&pod).await.unwrap().metadata.uid.unwrap();
    let binding = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(binding) = observer.bind_pod_process(&pod, &uid).await {
                break binding;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("process binding before fault");
    assert_eq!(
        observer
            .observe_pod_process_termination(&binding)
            .await
            .unwrap(),
        PodProcessTermination::Unknown,
        "KPT1"
    );
    // Serialize only the identity. There is no persisted `stopped: true` input.
    let bytes = serde_json::to_vec(&binding).unwrap();
    let binding = serde_json::from_slice(&bytes).unwrap();
    // The canonical Session Pod intentionally keeps its own PID 1: changing a
    // tool command cannot crash it. Inject through the exact local node's CRI,
    // after verifying the node belongs to the explicitly selected k3d cluster.
    let current = pods.get(&pod).await.unwrap();
    let node = current.spec.as_ref().unwrap().node_name.as_ref().unwrap();
    let owned = std::process::Command::new("docker")
        .args([
            "inspect",
            node,
            "--format",
            "{{ index .Config.Labels \"k3d.cluster\" }}",
        ])
        .output()
        .unwrap();
    assert!(owned.status.success());
    assert_eq!(String::from_utf8(owned.stdout).unwrap().trim(), cluster);
    let id = current
        .status
        .as_ref()
        .unwrap()
        .container_statuses
        .as_ref()
        .unwrap()
        .iter()
        .find(|s| s.name == "agent")
        .unwrap()
        .container_id
        .as_ref()
        .unwrap()
        .strip_prefix("containerd://")
        .unwrap();
    assert!(!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let stopped = std::process::Command::new("docker")
        .args(["exec", node, "crictl", "stop", "--timeout", "0", id])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "exact CRI fault: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let observer = PodProcessObserver::new(
        awaken_sandbox_container::k8s::connect_control_client()
            .await
            .unwrap(),
        &namespace,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = pods.get(&pod).await.unwrap();
            let exited = current
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .is_some_and(|statuses| {
                    statuses.iter().any(|s| {
                        s.name == "agent"
                            && s.state.as_ref().is_some_and(|s| s.terminated.is_some())
                    })
                });
            if exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("main container hard crash");
    assert_eq!(
        observer
            .observe_pod_process_termination(&binding)
            .await
            .unwrap(),
        PodProcessTermination::Unknown,
        "KPT2 still-live projector vetoes whole-domain proof"
    );
    let current = pods.get(&pod).await.unwrap();
    pods.delete(
        &pod,
        &kube::api::DeleteParams::default()
            .grace_period(1)
            .preconditions(kube::api::Preconditions {
                uid: current.metadata.uid,
                resource_version: current.metadata.resource_version,
            }),
    )
    .await
    .unwrap();
    let evidence = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let evidence = observer
                .observe_pod_process_termination(&binding)
                .await
                .unwrap();
            if matches!(evidence, PodProcessTermination::Observed(_)) {
                break evidence;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("KPT3 independent exact exit observation");
    let status = pods.get(&pod).await.unwrap().status.unwrap();
    let exit = status
        .container_statuses
        .unwrap()
        .into_iter()
        .find(|status| status.name == "agent")
        .unwrap()
        .state
        .unwrap()
        .terminated
        .unwrap();
    assert_eq!(exit.exit_code, 137, "KPT2 must exercise SIGKILL");
    let PodProcessTermination::Observed(receipt) = evidence else {
        unreachable!()
    };
    assert_eq!(
        receipt.binding(),
        &binding,
        "receipt carries the frozen subject"
    );
    assert!(!receipt.resource_version().is_empty());
    assert!(
        matches!(
            observer
                .observe_pod_process_termination(&binding)
                .await
                .unwrap(),
            PodProcessTermination::Observed(_)
        ),
        "KPT3 repeated observation"
    );
    assert!(
        !kubectl(&["-n", &namespace, "exec", &pod, "--", "true"])
            .status
            .success(),
        "KPT4 cannot restart the old domain"
    );
    k8s_common::set_test_finalizer(&pods, &pod, finalizer, false).await;
    pc::Sandbox::dispose(&sandbox)
        .await
        .expect("dispose exact owned fixture");
    assert_eq!(
        observer
            .observe_pod_process_termination(&binding)
            .await
            .unwrap(),
        PodProcessTermination::Unknown,
        "KPT5 missing historical evidence remains unknown"
    );
}
