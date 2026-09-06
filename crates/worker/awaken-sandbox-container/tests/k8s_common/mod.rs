use awaken_provisioning_contract as pc;

pub(crate) fn fixture_image() -> String {
    std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".to_string())
}

pub(crate) fn container_environment(image: String) -> pc::EnvironmentKind {
    pc::EnvironmentKind::Image { reference: image }
}

#[path = "../common/disposal.rs"]
pub(crate) mod disposal;
pub(crate) use disposal::{disposal_authorization, effect_fence};

pub(crate) fn spec_with_command(
    scope: &str,
    image: String,
    command: Vec<String>,
) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: Some(container_environment(image)),
        command,
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        control_services: Default::default(),
        lease_ttl_secs: None,
    }
}

pub(crate) fn container_id(handle: &pc::SandboxHandle) -> String {
    handle
        .container_payload()
        .expect("Kubernetes fixture emits a typed container handle")
        .container_id
        .clone()
}

/// Hold/release only a test-owned finalizer. Preserve production finalizers and
/// fence the patch against concurrent metadata changes; this is never cleanup.
#[allow(dead_code)] // This shared fixture module is compiled by multiple test binaries.
pub(crate) async fn set_test_finalizer<K>(
    api: &kube::Api<K>,
    name: &str,
    finalizer: &str,
    hold: bool,
) where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + kube::Resource<DynamicType = ()>,
{
    let observed = api.get(name).await.expect("read test barrier object");
    let mut finalizers = observed.meta().finalizers.clone().unwrap_or_default();
    finalizers.retain(|value| value != finalizer);
    if hold {
        finalizers.push(finalizer.into());
    }
    api.patch(
        name,
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({"metadata": {
            "resourceVersion": observed.meta().resource_version,
            "finalizers": finalizers
        }})),
    )
    .await
    .expect("patch test-owned deletion barrier");
}
