use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;

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
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut observed = api.get(name).await.expect("read test barrier object");
        let uid = observed
            .meta()
            .uid
            .clone()
            .expect("test barrier object UID");
        for attempt in 0..8 {
            assert_eq!(
                observed.meta().uid.as_ref(),
                Some(&uid),
                "test barrier object was replaced"
            );
            let version = observed
                .meta()
                .resource_version
                .as_ref()
                .expect("test barrier object version");
            let current = observed.meta().finalizers.clone().unwrap_or_default();
            let mut finalizers = current.clone();
            finalizers.retain(|value| value != finalizer);
            if hold {
                finalizers.push(finalizer.into());
            }
            if finalizers == current {
                return;
            }
            match api
                .patch(
                    name,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(serde_json::json!({"metadata": {
                        "uid": uid,
                        "resourceVersion": version,
                        "finalizers": finalizers
                    }})),
                )
                .await
            {
                Ok(_) => return,
                Err(kube::Error::Api(error)) if error.code == 409 && attempt < 7 => {
                    // A storage/Pod controller may publish metadata between GET
                    // and PATCH. Rebase only this barrier onto the same UID;
                    // neither stale finalizer lists nor name reuse are authority.
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    observed = api.get(name).await.expect("reread test barrier object");
                }
                Err(error) => panic!("patch test-owned deletion barrier: {error}"),
            }
        }
    })
    .await
    .expect("test barrier metadata deadline");
}
