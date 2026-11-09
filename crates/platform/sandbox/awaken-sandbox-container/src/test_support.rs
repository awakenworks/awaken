// @formal surface exclude=test_only
// @formal owner crates/platform/sandbox/awaken-sandbox-container/src/lib.rs
// @formal owner crates/platform/sandbox/awaken-sandbox-kubernetes/Cargo.toml
// @formal reason: Static owner: one canonical Session spec and one aggregate-authorized disposal fixture
// @formal reason: shared by the provider's and the Kubernetes engine's tests. Compiled only by
// @formal reason: cfg(any(test, feature = test-support)); it mints no production authority.
//! Fixtures shared by this crate's tests and by the container engine crates
//! that build on it (`test-support` feature): one canonical Session spec and
//! the aggregate-authorized disposal edge a provider test needs. Engine adapters
//! share the original plan fixture here as well.

use awaken_provisioning_contract as pc;

pub fn disposal_authorization(
    prepared: &pc::SandboxEffectFence,
) -> pc::SandboxDisposalAuthorization {
    let preparation_fingerprint = format!("test-preparation:{}", prepared.operation_id());
    let preparation =
        pc::SandboxDisposalPreparation::new(prepared.clone(), preparation_fingerprint)
            .expect("test preparation is complete");
    let operation_id = preparation
        .operation_id()
        .expect("stable disposal identity");
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        prepared.owner(),
        prepared.runtime_incarnation(),
        prepared.epoch(),
        prepared.expires_at_unix_ms(),
    )
    .expect("test authorization successor is valid");
    preparation
        .authorize(successor)
        .expect("test authorization is aggregate-authorized")
}

pub fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["claude".into(), "--acp".into()],
        deny_tool_egress: false,
        mounts: vec![
            pc::MountRequirement {
                mount_id: "in".into(),
                source: pc::MountSource::File {
                    file_id: "file-1".into(),
                    content_hash: None,
                },
                mount_path: "/data/in.txt".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            },
            pc::MountRequirement {
                mount_id: "work".into(),
                source: pc::MountSource::Resource {
                    resource_id: "res-9".into(),
                    content_hash: None,
                },
                mount_path: "/work".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::Session,
                required: true,
            },
        ],
        env: vec![
            pc::EnvVar {
                name: "TZ".into(),
                value: pc::EnvValue::Inline {
                    value: "UTC".into(),
                },
                visibility: pc::EnvVisibility::Process,
            },
            pc::EnvVar {
                name: "API_KEY".into(),
                value: pc::EnvValue::Secret {
                    reference: "broker://k".into(),
                },
                visibility: pc::EnvVisibility::Process,
            },
        ],
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(1 << 30),
            pids: Some(256),
            disk_bytes: None,
        },
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: Some(60),
        control_services: Default::default(),
    }
}

/// The original engine-adapter plan fixture, shared by Podman and Docker.
pub fn container_plan() -> crate::ContainerPlan {
    crate::ContainerPlan {
        image: "img:latest".into(),
        command: vec!["/agent".into()],
        env: vec![],
        control_services: Default::default(),
        packages: Default::default(),
        binds: vec![],
        outputs_volume: "/out".into(),
        network: crate::NetworkMode::None,
        egress_identity: crate::EgressRealizationIdentity {
            network: crate::NetworkMode::None,
            proxy_endpoint: None,
            capability_ttl_secs: None,
            issuer_revision: None,
            ephemeral_capability: false,
        },
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: vec![],
        rootfs: crate::RootfsPlan::Image("img:latest".into()),
    }
}
