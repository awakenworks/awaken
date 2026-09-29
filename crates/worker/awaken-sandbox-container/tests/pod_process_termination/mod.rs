// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
use super::*;
use awaken_sandbox_container::k8s::{
    ObservedPodProcessExit, PodProcessBinding, PodProcessObserver, PodProcessStopper,
    PodProcessTermination, WORKER_PROCESS_EVIDENCE_FINALIZER,
};

async fn wait_for_binding(
    observer: &PodProcessObserver,
    pod: &str,
    uid: &str,
) -> PodProcessBinding {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(binding) = observer.bind_pod_process(pod, uid).await {
                break binding;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("process binding before fault")
}

async fn stop_and_observe(
    stopper: &PodProcessStopper,
    binding: &PodProcessBinding,
) -> ObservedPodProcessExit {
    // Exercise the real provider command, including exact-CAS retries. An API
    // error or accepted DELETE never satisfies the positive exit oracle.
    let mut last_error = None;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match stopper.request_stop(binding).await {
                Ok(PodProcessTermination::Observed(receipt)) => break receipt,
                Ok(PodProcessTermination::Unknown) => {}
                Err(error) => last_error = Some(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("independent exact exit observation: {last_error:?}"))
}

fn hard_stop_container(cluster: &str, pod: &k8s_openapi::api::core::v1::Pod, container: &str) {
    let node = pod.spec.as_ref().unwrap().node_name.as_ref().unwrap();
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
    let id = pod
        .status
        .as_ref()
        .unwrap()
        .container_statuses
        .as_ref()
        .unwrap()
        .iter()
        .find(|status| status.name == container)
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
}

#[tokio::test]
async fn independent_observer_requires_exact_runtime_exit_and_retains_missing_history() {
    // Live decision table (production adapter, real kubelet/CRI; not a Dispatch
    // recovery claim): KPT1 pre-bound running domain => Unknown; KPT2 PID-1
    // SIGKILL with another container live => Unknown; KPT3 every container exits
    // => same fact; KPT4 delayed exec => rejected by the terminated domain;
    // KPT5 API deletion after exit => Unknown, not reconstructed history.
    // KPT6 same-name replacement => old UID cannot bind, old query is Unknown;
    // KPT7 delayed DELETE carrying only the old UID => API conflict, successor
    // stays live; KPT8 successor exit => only its fresh binding yields evidence.
    // KPT9 adding retention after the old binding cannot authorize its stop.
    // KPT10 all containers crash before stop: a read-only exit observation is
    // possible, but the stop command must still close the exact Pod and return
    // Unknown before a subsequent independent closed-exit observation.
    // KPT5/6/8 correspond to ExactCompleteObservation in the existing three-
    // generation K8sProcessObservation model. KPT7 tests the API's UID-precondition
    // trusted boundary, not a new Dispatch retirement or replay implementation.
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
    let provider = ContainerProvider::new(runtime.clone(), fixture_image())
        .with_clock(awaken_lease_contract::system_clock());
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
    let finalizer = WORKER_PROCESS_EVIDENCE_FINALIZER;
    let uid = pods.get(&pod).await.unwrap().metadata.uid.unwrap();
    let unretained = wait_for_binding(&observer, &pod, &uid).await;
    k8s_common::set_test_finalizer(&pods, &pod, finalizer, true).await;
    let binding = wait_for_binding(&observer, &pod, &uid).await;
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
    hard_stop_container(&cluster, &current, "agent");
    let observer = PodProcessObserver::new(
        awaken_sandbox_container::k8s::connect_control_client()
            .await
            .unwrap(),
        &namespace,
    )
    .unwrap();
    let stopper = PodProcessStopper::new(
        PodProcessObserver::new(
            awaken_sandbox_container::k8s::connect_control_client()
                .await
                .unwrap(),
            &namespace,
        )
        .unwrap(),
    );
    assert!(stopper.request_stop(&unretained).await.is_err(), "KPT9");
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
    let receipt = stop_and_observe(&stopper, &binding).await;
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
    sandbox
        .dispose()
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

    // Reuse the production provider's stable-scope realization. A hand-authored
    // replacement Pod would bypass the shape and naming contract under test.
    let replacement = provider.create_container(&spec).await.unwrap();
    assert_eq!(pod_of(&replacement), pod, "KPT6 exact name reuse");
    let replacement_uid = pods.get(&pod).await.unwrap().metadata.uid.unwrap();
    assert_ne!(replacement_uid, uid, "KPT6 different physical domain");
    k8s_common::set_test_finalizer(&pods, &pod, finalizer, true).await;
    let replacement_binding = wait_for_binding(&observer, &pod, &replacement_uid).await;
    assert_eq!(
        stopper.request_stop(&binding).await.unwrap(),
        PodProcessTermination::Unknown,
        "KPT6 stale stop command"
    );
    assert!(observer.bind_pod_process(&pod, &uid).await.is_err(), "KPT6");
    assert_eq!(
        observer
            .observe_pod_process_termination(&binding)
            .await
            .unwrap(),
        PodProcessTermination::Unknown,
        "KPT6 no old evidence from a live successor"
    );
    let stale_delete = pods
        .delete(
            &pod,
            &kube::api::DeleteParams::default().preconditions(kube::api::Preconditions {
                uid: Some(uid),
                resource_version: None,
            }),
        )
        .await;
    assert!(
        matches!(stale_delete, Err(kube::Error::Api(error)) if error.code == 409),
        "KPT7"
    );
    let survivor = pods.get(&pod).await.unwrap();
    assert_eq!(
        survivor.metadata.uid.as_deref(),
        Some(replacement_uid.as_str())
    );
    assert!(
        survivor.metadata.deletion_timestamp.is_none(),
        "KPT7 no deletion intent"
    );
    assert!(
        kubectl(&["-n", &namespace, "exec", &pod, "--", "true"])
            .status
            .success(),
        "KPT7 successor still executes"
    );
    // Crash the entire successor before asking the recovery provider to close
    // it. Reuse the exact CRI injector; ordinary exit and partial crash above
    // must not stand in for this all-processes-already-exited fault cut.
    for container in &survivor.spec.as_ref().unwrap().containers {
        hard_stop_container(&cluster, &survivor, &container.name);
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if matches!(
                observer
                    .observe_pod_process_termination(&replacement_binding)
                    .await
                    .unwrap(),
                PodProcessTermination::Observed(_)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("KPT10 every exact process exited before stop");
    assert!(
        pods.get(&pod)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none(),
        "KPT10 exit alone did not close the Pod"
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match stopper.request_stop(&replacement_binding).await {
                Ok(result) => {
                    assert_eq!(
                        result,
                        PodProcessTermination::Unknown,
                        "KPT10 accepted closure is not yet a closed-exit observation"
                    );
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    })
    .await
    .expect("KPT10 exact closure despite status CAS races");
    assert!(
        pods.get(&pod)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some(),
        "KPT10 API-owned launch closure retained before receipt"
    );
    let replacement_receipt = stop_and_observe(&stopper, &replacement_binding).await;
    assert_eq!(replacement_receipt.binding(), &replacement_binding, "KPT8");
    assert_ne!(replacement_receipt.binding(), receipt.binding(), "KPT8");
    assert_eq!(
        receipt.binding(),
        &binding,
        "KPT8 old receipt keeps its subject"
    );
    assert_eq!(
        observer
            .observe_pod_process_termination(&binding)
            .await
            .unwrap(),
        PodProcessTermination::Unknown,
        "KPT8 successor exit cannot repair old missing history"
    );
    k8s_common::set_test_finalizer(&pods, &pod, finalizer, false).await;
    replacement.dispose().await.unwrap();
}
