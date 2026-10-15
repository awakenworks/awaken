//! Real Kubernetes (k3d) capacity end-to-end (ADR-0092): a namespace
//! `ResourceQuota` refusal and an unschedulable Pod both reach the provider as
//! `CapacityUnavailable` -- the class the Worker turns into a request-local
//! claim deferral -- leave no Pod behind, and the same request succeeds once
//! the cluster has room again.
//!
//! Gated on the `k8s` feature AND `AWAKEN_K8S_E2E=1` with a reachable cluster
//! (KUBECONFIG). Once requested, missing infrastructure is a hard failure. Run
//! by `scripts/e2e/k8s_container_e2e.sh`.
#![cfg(feature = "k8s")]

// The shared live fixtures serve several test binaries; this one uses a subset.
#[allow(dead_code, unused_imports)]
mod k8s_common;

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{ContainerEnvironmentProvider, ContainerProvider};
use k8s_common::{effect_fence, fixture_image, spec_with_command};
use k8s_openapi::api::core::v1::{Namespace, Pod, ResourceQuota};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use pc::test_support::EffectAuthority;

fn requested() -> bool {
    let required = std::env::var("AWAKEN_K8S_E2E").as_deref() == Ok("1");
    if !required {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 to require the live Kubernetes suite");
    }
    required
}

/// One disposable namespace per scenario, deleted on drop of the guard.
struct ScratchNamespace {
    client: Client,
    name: String,
}

impl ScratchNamespace {
    async fn create(client: Client, purpose: &str) -> Self {
        let name = format!("awaken-capacity-{purpose}-{}", std::process::id());
        let namespace: Namespace =
            serde_json::from_value(serde_json::json!({"metadata": {"name": name}})).unwrap();
        Api::<Namespace>::all(client.clone())
            .create(&PostParams::default(), &namespace)
            .await
            .expect("create the scratch namespace");
        // The runtime attests the canonical sandbox NetworkPolicy graph before
        // it will create a Pod; install the exact graph production imports.
        let graph = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../deploy/k3d/bases/sandbox-network-policy/resources.yaml"
        ))
        .expect("read the canonical sandbox NetworkPolicy graph");
        let policies = Api::<NetworkPolicy>::namespaced(client.clone(), &name);
        for policy in serde_saphyr::from_multiple::<NetworkPolicy>(&graph)
            .expect("canonical NetworkPolicy graph")
        {
            policies
                .create(&PostParams::default(), &policy)
                .await
                .expect("install the canonical NetworkPolicy");
        }
        Self { client, name }
    }

    async fn delete(self) {
        let _ = Api::<Namespace>::all(self.client.clone())
            .delete(&self.name, &DeleteParams::default())
            .await;
    }

    async fn runtime(&self, unschedulable_wait: Duration) -> K8sRuntime {
        let address = "127.0.0.1:8080".parse().unwrap();
        k8s_common::connect_with_clock(&self.name, address)
            .await
            .expect("AWAKEN_K8S_E2E=1 requires a kube client")
            .with_unschedulable_wait(unschedulable_wait)
    }

    async fn pods(&self) -> Vec<Pod> {
        Api::<Pod>::namespaced(self.client.clone(), &self.name)
            .list(&ListParams::default())
            .await
            .expect("list scratch Pods")
            .items
    }
}

async fn create(
    provider: &ContainerProvider<K8sRuntime>,
    spec: &pc::SandboxSpec,
    fence_round: u64,
) -> Result<(), pc::SandboxError> {
    let fence = effect_fence(&spec.scope, "capacity", "capacity-owner", fence_round);
    ContainerEnvironmentProvider::create_environment_for_effect(
        provider,
        spec,
        &fence,
        &EffectAuthority::new(&fence),
        awaken_container_contract::EnvironmentRealizationIntent::Create,
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn a_namespace_quota_refusal_returns_capacity_and_succeeds_once_raised() {
    // Decision table (live). Causes: C1 the namespace ResourceQuota admits
    // `pods: 0` / `pods: 4`. Effects: E1 Create is refused as
    // CapacityUnavailable with the quota message and no Pod exists; E2 after
    // the quota is raised the same Session creates its Pod.
    if !requested() {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let namespace = ScratchNamespace::create(client.clone(), "quota").await;
    let quotas = Api::<ResourceQuota>::namespaced(client.clone(), &namespace.name);
    let quota: ResourceQuota = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "sandbox-pods"},
        "spec": {"hard": {"pods": "0"}}
    }))
    .unwrap();
    quotas
        .create(&PostParams::default(), &quota)
        .await
        .expect("install the zero-Pod quota");
    let provider = ContainerProvider::new(
        Arc::new(namespace.runtime(Duration::from_secs(60)).await),
        fixture_image(),
    )
    .with_clock(awaken_time::system_clock());
    let scope = format!("sesn_capacity_quota:{}", std::process::id());
    let spec = spec_with_command(&scope, fixture_image(), vec!["sleep".into(), "60".into()]);

    // The quota controller fills `status.used` asynchronously after the quota
    // is created; until then admission may still admit.
    // awaken-allow(wait): quota status is an external Kubernetes fact, read back by the exact quota name.
    for _ in 0..60 {
        let observed = quotas.get("sandbox-pods").await.expect("read quota");
        if observed.status.and_then(|status| status.used).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let refused = create(&provider, &spec, 1).await;
    match refused {
        Err(pc::SandboxError::CapacityUnavailable(detail)) => {
            assert!(detail.contains("exceeded quota"), "E1: {detail}");
        }
        other => panic!("E1 a full namespace is CapacityUnavailable, got {other:?}"),
    }
    assert!(
        namespace.pods().await.is_empty(),
        "E1 a refused Create made no Pod"
    );

    quotas
        .patch(
            "sandbox-pods",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"spec": {"hard": {"pods": "4"}}})),
        )
        .await
        .expect("raise the quota");
    let mut created = Err(pc::SandboxError::not_ready("quota not yet raised"));
    // awaken-allow(wait): the raised quota reaches admission asynchronously; each retry is a fresh exact Create.
    for fence_round in 2..32 {
        created = create(&provider, &spec, fence_round).await;
        if !matches!(created, Err(pc::SandboxError::CapacityUnavailable(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    created.expect("E2 the same Session creates its Pod once the quota is raised");
    assert_eq!(namespace.pods().await.len(), 1, "E2");
    namespace.delete().await;
}

#[tokio::test]
async fn an_unschedulable_pod_is_disposed_and_returns_capacity_after_the_bound() {
    // Decision table (live). Causes: C1 the Pod requests more CPU than any
    // node has, so the scheduler reports Unschedulable; C2 the configured
    // bound elapses. Effects: E1 realization fails as CapacityUnavailable
    // carrying the scheduler's message (not the 120 s start-up timeout); E2 the
    // Pending Pod was deleted by its UID-fenced disposal before the claim
    // would move on.
    if !requested() {
        return;
    }
    let client = Client::try_default().await.expect("kube client");
    let namespace = ScratchNamespace::create(client, "unschedulable").await;
    let provider = ContainerProvider::new(
        Arc::new(namespace.runtime(Duration::from_secs(5)).await),
        fixture_image(),
    )
    .with_clock(awaken_time::system_clock());
    let scope = format!("sesn_capacity_sched:{}", std::process::id());
    let mut spec = spec_with_command(&scope, fixture_image(), vec!["sleep".into(), "60".into()]);
    spec.requests.cpu_millis = Some(1_000_000);
    let started = std::time::Instant::now();
    match create(&provider, &spec, 1).await {
        Err(pc::SandboxError::CapacityUnavailable(detail)) => {
            assert!(
                detail.contains("was not scheduled within 5s"),
                "E1: {detail}"
            );
        }
        other => panic!("E1 an unschedulable Pod is CapacityUnavailable, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(100),
        "E1 the scheduler bound, not the start-up timeout, ended the wait"
    );
    // awaken-allow(wait): Pod deletion finishes asynchronously; read back the namespace's Pods.
    for _ in 0..60 {
        if namespace.pods().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        namespace.pods().await.is_empty(),
        "E2 the Pending Pod was disposed"
    );
    namespace.delete().await;
}
