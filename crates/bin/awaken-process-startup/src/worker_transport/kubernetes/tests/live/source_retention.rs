//! Original Worker factories and signed broker, with actual Kubernetes exec.
//! This extends the existing isolated-cluster case, not its process-exit claim.

use super::*;
use awaken_deployment_config::DeploymentConfig;
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_session_environment_contract::ContainerHandResidency;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::ObjectMeta;

// The provider's existing disposal fixture remains the only test constructor.
#[allow(dead_code)]
#[path = "../../../../../../../worker/awaken-sandbox-container/tests/common/disposal.rs"]
mod disposal;

/// One retiring Worker's read of one original source it holds.
struct ObservationRow {
    source_residency: ContainerHandResidency,
    actual: Result<pc::SandboxObservation, pc::SandboxError>,
    /// `Some((before, after))` when the identity projection changed.
    changed: Option<(serde_json::Value, serde_json::Value)>,
}

/// The facts a retained-source read must not alter: object identity, the
/// deletion/GC/guard state that keeps the original alive, and the declared
/// shape (Pod spec; PVC spec including `volumeName`). Labels and annotations
/// carry the provider's ownership and isolation marks, so a read that wrote
/// any of them would be a mutation. `status`, `resourceVersion`,
/// `managedFields` and `generation` are excluded: kubelet and the volume
/// controller advance them for a running Pod/bound PVC without any writer
/// touching these facts, so comparing them would fail spuriously.
fn retained_identity(metadata: &ObjectMeta, spec: serde_json::Value) -> serde_json::Value {
    json!({
        "uid": metadata.uid,
        "name": metadata.name,
        "namespace": metadata.namespace,
        "labels": metadata.labels,
        "annotations": metadata.annotations,
        "finalizers": metadata.finalizers,
        "ownerReferences": metadata.owner_references,
        "deletionTimestamp": metadata.deletion_timestamp,
        "deletionGracePeriodSeconds": metadata.deletion_grace_period_seconds,
        "spec": spec,
    })
}

async fn read_retained_identity(
    pods: &kube::Api<Pod>,
    claims: &kube::Api<PersistentVolumeClaim>,
    pod_name: &str,
    claim_name: &str,
) -> serde_json::Value {
    let pod = pods.get(pod_name).await.unwrap();
    let claim = claims.get(claim_name).await.unwrap();
    json!({
        "pod": retained_identity(&pod.metadata, json!(pod.spec)),
        "claim": retained_identity(&claim.metadata, json!(claim.spec)),
    })
}

pub(super) async fn assert_original_sources_remain_observable(
    client: kube::Client,
    namespace: &str,
    create_transport: Arc<WorkerK8sCreateClient>,
    worker: &WorkerIdentity,
    clock: Arc<ManualClock>,
) {
    // Cause/effect graph F1. C1 a real registered successor uses the canonical
    // Worker namespace/factory and its signed Create client; C2 the original
    // Pod/PVC and producer-installed guard exist; C3 original Hand residency is
    // attached/resident. R1-R2 are the C3 rows: the retiring Worker reads the
    // source through its own provider for the retention it reports, and fixed
    // C1+C2 -> RetainedOriginal for every row with unchanged original
    // identity. These are read facts, never process-exit, ordinary adoption or
    // physical-disposal authority; the Coordinator decides retirement from
    // the reported fact and observes nothing. retained_plan owns missing
    // UID/guard/API-error rows. Keep the old Worker's C7 one-ticket assertion:
    // this extension uses only the separately registered successor's group.
    // Every created source is disposed before any row or precondition failure
    // is reported, so a RED leaves no finalizer-guarded Pod/PVC behind.
    let policy_base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../deploy/k3d/bases/sandbox-network-policy/resources.yaml");
    assert!(
        std::process::Command::new("kubectl")
            .args(["-n", namespace, "apply", "-f"])
            .arg(policy_base)
            .status()
            .unwrap()
            .success(),
        "F1 use the original policy base in this test-owned namespace"
    );
    let image = std::env::var("AWAKEN_K8S_CONTROL_IMAGE")
        .expect("original k3d runner must supply its already-qualified Sandbox digest");
    assert!(
        image.contains("@sha256:"),
        "F1 no role tag substitutes for the operator image"
    );
    let pods = kube::Api::<Pod>::namespaced(client.clone(), namespace);
    let claims = kube::Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    let mut observations = Vec::new();
    for residency in [
        ContainerHandResidency::AttachedExec,
        ContainerHandResidency::Resident,
    ] {
        let mut deployment = DeploymentConfig::ephemeral();
        deployment.disable_local_pool = true;
        deployment.sandbox_tier = awaken_deployment_config::SandboxTier::K8s;
        deployment.container_image = Some(image.clone());
        deployment.sandbox.k8s_namespace = namespace.into();
        deployment.sandbox.warm_pool_size = 0;
        deployment.sandbox.container_hand_residency = residency;
        deployment.sandbox.k8s_continuation_volume =
            Some(awaken_container_contract::K8sContinuationVolume {
                storage_class_name: None,
                size: "1Gi".into(),
            });
        let policy = awaken_execution_composition::ContainerTierPolicy {
            k8s_create_transport: Some(create_transport.clone()),
            ..Default::default()
        };
        let components = awaken_execution_composition::build_container_environment_for_realization(
            awaken_worker::worker_realization_namespace(&deployment).unwrap(),
            deployment.sandbox_tier,
            deployment.container_image.as_deref(),
            &deployment.sandbox,
            &policy,
            clock.clone(),
        )
        .await
        .expect("F1 original Worker provider factory and live startup evidence");
        let scope = format!("source-observation-{}", uuid::Uuid::now_v7());
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: scope.clone(),
            isolation: pc::IsolationClass::Container,
            environment: Some(pc::EnvironmentKind::Image {
                reference: image.clone(),
            }),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 300".into()],
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: pc::ResourceLimits {
                pids: None,
                ..Default::default()
            },
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            control_services: Default::default(),
            lease_ttl_secs: None,
        };
        let create = pc::SandboxEffectFence::new(
            format!("{scope}:create"),
            &worker.worker_id,
            worker.lease_owner(),
            1,
            u64::MAX,
        )
        .unwrap();
        let create_authority = pc::test_support::EffectAuthority::new(&create);
        let source = components
            .provider
            .claim_for_effect(
                &spec,
                pc::SandboxEffect::Fenced {
                    fence: &create,
                    authority: &create_authority,
                },
                pc::SandboxRealizationIntent::Create,
            )
            .await
            .expect("F1 signed original broker + actual provider API/exec must issue the handle");
        let handle = source.handle();
        let pod_name = handle.container_payload().unwrap().container_id;
        // Preconditions and reads run as their own task: a panic there is
        // held until the published handle below has been disposed.
        let examined = tokio::spawn(examine_original_source(
            client.clone(),
            namespace.to_owned(),
            components.provider.clone(),
            spec.clone(),
            handle.clone(),
            create.clone(),
            residency,
        ))
        .await;
        drop(source);
        // Finish through the same provider's published-handle preparation and
        // UID-fenced disposal BEFORE asserting the read result.
        let terminal = pc::SandboxEffectFence::new(
            format!("{scope}:terminal"),
            create.owner(),
            create.runtime_incarnation(),
            2,
            u64::MAX,
        )
        .unwrap();
        let authority = pc::test_support::EffectAuthority::new(&terminal);
        let cleanup = components
            .provider
            .prepare_terminal_for_effect(&spec, Some(&handle), Some(&create), &terminal, &authority)
            .await
            .expect("F1 original published-handle terminal preparation")
            .expect("F1 original source still exists for its own cleanup owner");
        let prepared = cleanup
            .prepare_disposal_for_effect(&terminal, &authority)
            .await
            .unwrap();
        let authorization = disposal::disposal_authorization(&prepared);
        cleanup
            .dispose_for_effect(
                &authorization,
                &pc::test_support::EffectAuthority::new(authorization.effect_fence()),
            )
            .await
            .expect("F1 original UID-fenced disposal");
        assert!(pods.get_opt(&pod_name).await.unwrap().is_none());
        let (claim_name, rows) = match examined {
            Ok(examined) => examined,
            Err(failure) => std::panic::resume_unwind(failure.into_panic()),
        };
        assert!(claims.get_opt(&claim_name).await.unwrap().is_none());
        observations.extend(rows);
    }
    assert_eq!(
        observations.len(),
        2,
        "F1 both original sources completed their read and cleanup"
    );
    for row in observations {
        let case = row.source_residency;
        assert!(
            row.changed.is_none(),
            "F1 {case:?}: original identity changed; inspect concurrent API writers before attributing this to observation: {:?}",
            row.changed
        );
        assert!(
            matches!(
                &row.actual,
                Ok(pc::SandboxObservation::RetainedOriginal { .. })
            ),
            "F1 {case:?}: original source was not observable: {:?}",
            row.actual
        );
    }
}

/// Checks the published original source, then reads it through the Worker's
/// own provider, as the retention it reports. Returns the bound claim name and
/// the unasserted row.
async fn examine_original_source(
    client: kube::Client,
    namespace: String,
    provider: Arc<
        dyn pc::SandboxProvider<Environment = dyn awaken_provisioning_contract::SessionSandbox>,
    >,
    spec: pc::SandboxSpec,
    handle: pc::SandboxHandle,
    create: pc::SandboxEffectFence,
    residency: ContainerHandResidency,
) -> (String, Vec<ObservationRow>) {
    let payload = handle.container_payload().unwrap();
    let (original_pod_uid, original_claim_uid) = match payload.runtime_handle.as_ref() {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid: Some(claim_uid),
            ..
        }) => (pod_uid.clone(), claim_uid.clone()),
        other => {
            panic!("F1 source precondition requires a published V3 original pair: {other:?}")
        }
    };
    assert!(!original_pod_uid.trim().is_empty() && !original_claim_uid.trim().is_empty());
    let pod_name = payload.container_id;
    let pods = kube::Api::<Pod>::namespaced(client.clone(), &namespace);
    let claims = kube::Api::<PersistentVolumeClaim>::namespaced(client, &namespace);
    let pod = pods.get(&pod_name).await.unwrap();
    let claim_names = pod
        .spec
        .as_ref()
        .unwrap()
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|volume| volume.persistent_volume_claim.as_ref())
        .map(|claim| claim.claim_name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        claim_names.len(),
        1,
        "F1 source precondition: one exact PVC binding"
    );
    let claim_name = claim_names[0].clone();
    let claim = claims.get(&claim_name).await.unwrap();
    assert_eq!(pod.metadata.uid.as_deref(), Some(original_pod_uid.as_str()));
    assert_eq!(
        claim.metadata.uid.as_deref(),
        Some(original_claim_uid.as_str())
    );
    assert_eq!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Running")
    );
    for metadata in [&pod.metadata, &claim.metadata] {
        assert!(
            metadata.deletion_timestamp.is_none(),
            "F1 source precondition: live original"
        );
        assert!(
            metadata.owner_references.as_ref().is_none_or(Vec::is_empty),
            "F1 source precondition: GC independence"
        );
        assert!(
            metadata.labels.as_ref().is_none_or(
                |labels| !labels.contains_key(awaken_container_contract::ISOLATION_LABEL)
            ),
            "F1 source precondition: no provider isolation cut"
        );
    }
    assert!(
        pod.metadata.finalizers.as_ref().is_some_and(|values| values
            .iter()
            .any(|value| value == awaken_container_contract::ORIGINAL_GUARD_FINALIZER)),
        "F1 guard must come from the original signed Create broker"
    );
    assert!(handle.container_adoption_fingerprint().unwrap().is_some());
    assert!(handle.realization_fingerprint().is_some());
    let before = read_retained_identity(&pods, &claims, &pod_name, &claim_name).await;
    let mut adoption = pc::SandboxAdoption::new(&spec, &handle);
    adoption.observation_purpose = pc::SandboxObservationPurpose::WorkerRetirement {
        source_prepared: false,
    };
    let actual = provider
        .observe(
            adoption,
            pc::SandboxObservationBasis::Effect {
                spec: &spec,
                effect_fence: &create,
            },
        )
        .await;
    eprintln!("F1 original-source read {residency:?}: {actual:?}");
    let after = read_retained_identity(&pods, &claims, &pod_name, &claim_name).await;
    let rows = vec![ObservationRow {
        source_residency: residency,
        actual,
        changed: (after != before).then(|| (before.clone(), after)),
    }];
    (claim_name, rows)
}
