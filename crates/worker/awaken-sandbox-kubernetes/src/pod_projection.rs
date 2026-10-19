// @formal entrypoint sandbox_continuation.distinct_mutable_roots_share_one_retained_claim_without_aliasing
//! Pure projection of neutral bind material into Kubernetes objects.

use std::collections::BTreeMap;

use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec,
    Secret, SecretVolumeSource, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::names::{cfg_owner_label, configmap_name, credential_secret_name, pod_name};
use super::pod_security::{
    egress_label, hardened_security_context, pod_resources, sandbox_network_labels,
};
use super::{
    K8sRepositoryGatewayTrust, K8sSandboxControlForwarder, lifecycle, live_inputs, memory,
    projection, sandbox_control,
};
use awaken_provisioning_contract::SandboxEffectFence;
use awaken_sandbox_container::engine::{WritableRootTopology, writable_root_topology};
use awaken_sandbox_container::{BindPlan, ContainerPlan};

pub(super) fn build_pod(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    rendezvous: Option<&str>,
    image_pull_secrets: &[String],
) -> k8s_openapi::api::core::v1::Pod {
    build_pod_with_continuation(
        id,
        plan,
        owner,
        rendezvous,
        image_pull_secrets,
        None,
        None,
        None,
        None,
    )
}

/// Render the ordinary Pod shape. A demanded control service must use the
/// explicit operator-forwarder projection instead of silently omitting it.
#[must_use]
pub fn pod_for_plan(id: &str, plan: &ContainerPlan) -> k8s_openapi::api::core::v1::Pod {
    assert!(
        plan.control_services.is_empty(),
        "pod_for_plan cannot omit a demanded Sandbox control forwarder"
    );
    build_pod(id, plan, &None, None, &[])
}

impl super::K8sRuntime {
    pub(super) fn pod(&self, id: &str, plan: &ContainerPlan) -> k8s_openapi::api::core::v1::Pod {
        self.pod_for_effect(id, plan, None)
    }
}

pub(super) const CONFIGMAP_KEY: &str = "content";
pub(super) const CONTINUATION_VOLUME: &str = "continuation-state";

pub(super) fn append_writable_and_cache_volumes(
    plan: &ContainerPlan,
    continuation_claim: Option<&str>,
    volumes: &mut Vec<Volume>,
    agent_mounts: &mut Vec<VolumeMount>,
) {
    let writable = awaken_sandbox_container::engine::writable_dirs(plan);
    if let Some(claim_name) = continuation_claim {
        volumes.push(Volume {
            name: CONTINUATION_VOLUME.to_owned(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: claim_name.to_owned(),
                read_only: Some(false),
            }),
            ..Default::default()
        });
    }
    for (index, directory) in writable.into_iter().enumerate() {
        match writable_root_topology(continuation_claim.is_some(), index) {
            WritableRootTopology::Retained {
                claim_slot,
                subpath_slot,
            } => {
                debug_assert_eq!(claim_slot, 0);
                let subpath = format!("root-{subpath_slot}");
                // Kubelet safely creates missing subPath directories using the
                // volume's fsGroup permissions before mounting this container.
                agent_mounts.push(VolumeMount {
                    name: CONTINUATION_VOLUME.to_owned(),
                    mount_path: directory,
                    sub_path: Some(subpath),
                    read_only: Some(false),
                    ..Default::default()
                });
            }
            WritableRootTopology::Ephemeral { volume_slot } => {
                let name = format!("rw-{volume_slot}");
                volumes.push(Volume {
                    name: name.clone(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
                    ..Default::default()
                });
                agent_mounts.push(VolumeMount {
                    name,
                    mount_path: directory,
                    ..Default::default()
                });
            }
        }
    }
    for (index, bind) in plan.binds.iter().enumerate() {
        let Some(claim) = bind
            .source_ref
            .strip_prefix(awaken_sandbox_container::engine::PVC_BIND_REF_PREFIX)
        else {
            continue;
        };
        let name = format!("cache-{index}");
        volumes.push(Volume {
            name: name.clone(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: claim.to_owned(),
                read_only: Some(bind.read_only),
            }),
            ..Default::default()
        });
        agent_mounts.push(VolumeMount {
            name,
            mount_path: bind.mount_path.clone(),
            read_only: Some(bind.read_only),
            ..Default::default()
        });
    }
}

pub(super) fn content_binds(plan: &ContainerPlan) -> Vec<&BindPlan> {
    plan.binds
        .iter()
        .filter(|bind| bind.content.is_some() || bind.content_bytes.is_some())
        .collect()
}

pub(super) fn credential_binds(plan: &ContainerPlan) -> Vec<&BindPlan> {
    plan.binds
        .iter()
        .filter(|bind| bind.secret_content.is_some())
        .collect()
}

pub(super) fn credential_key(bind: &BindPlan) -> &str {
    bind.credential_file_path
        .as_deref()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(CONFIGMAP_KEY)
}

pub(super) fn build_configmap(
    id: &str,
    index: usize,
    content: Option<&str>,
    content_bytes: Option<&[u8]>,
    owner: &Option<OwnerReference>,
) -> ConfigMap {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "awaken-sandbox".to_string());
    labels.insert("awaken-cfg-owner".to_string(), cfg_owner_label(id));
    let (data, binary_data) = match (content, content_bytes) {
        (Some(text), _) => (
            Some(BTreeMap::from([(
                CONFIGMAP_KEY.to_string(),
                text.to_string(),
            )])),
            None,
        ),
        (None, Some(bytes)) => (
            None,
            Some(BTreeMap::from([(
                CONFIGMAP_KEY.to_string(),
                k8s_openapi::ByteString(bytes.to_vec()),
            )])),
        ),
        (None, None) => (Some(BTreeMap::new()), None),
    };
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(configmap_name(id, index)),
            labels: Some(labels),
            owner_references: owner.clone().map(|reference| vec![reference]),
            ..Default::default()
        },
        data,
        binary_data,
        immutable: Some(true),
    }
}

pub(super) fn build_credential_secret(
    id: &str,
    index: usize,
    key: &str,
    bytes: &[u8],
    owner: &Option<OwnerReference>,
) -> Secret {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "awaken-sandbox".to_string());
    labels.insert("awaken-cfg-owner".to_string(), cfg_owner_label(id));
    Secret {
        metadata: ObjectMeta {
            name: Some(credential_secret_name(id, index)),
            labels: Some(labels),
            owner_references: owner.clone().map(|reference| vec![reference]),
            ..Default::default()
        },
        immutable: Some(true),
        data: Some(BTreeMap::from([(
            key.to_string(),
            k8s_openapi::ByteString(bytes.to_vec()),
        )])),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_pod_with_continuation(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    rendezvous: Option<&str>,
    image_pull_secrets: &[String],
    continuation_claim: Option<&str>,
    effect_fence: Option<&SandboxEffectFence>,
    sandbox_control_forwarder: Option<&K8sSandboxControlForwarder>,
    repository_gateway_trust: Option<&K8sRepositoryGatewayTrust>,
) -> Pod {
    assert!(
        plan.control_services.is_empty() || sandbox_control_forwarder.is_some(),
        "a demanded Sandbox control service requires an installed trusted forwarder"
    );
    {
        let mut agent_env: Vec<EnvVar> = plan
            .env
            .iter()
            .map(|(k, v)| EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                value_from: None,
            })
            .collect();
        if let Some(trust) = repository_gateway_trust
            && plan.env.iter().any(|(key, _)| key == "HTTPS_PROXY")
        {
            agent_env.retain(|variable| variable.name != "NO_PROXY");
            agent_env.push(EnvVar {
                name: "NO_PROXY".into(),
                value: Some(format!("localhost,127.0.0.1,{}", trust.host)),
                value_from: None,
            });
        }
        if let Some(addr) = rendezvous {
            agent_env.push(EnvVar {
                name: "AWAKEN_ACP_RENDEZVOUS".into(),
                value: Some(addr.to_string()),
                value_from: None,
            });
        }

        // Each Memory store uses one authoritative host-side mounter. Its bounded
        // snapshot is streamed by a runtime-only projector into a pod-scoped
        // emptyDir before create returns; disposal reads that exact tree back and
        // lets the retained mounter perform the canonical CAS harvest. The Pod
        // receives neither a second Memory database nor Resource-authority credentials.
        let mut volumes: Vec<Volume> = Vec::new();
        let mut agent_mounts: Vec<VolumeMount> = Vec::new();
        if let Some(trust) = repository_gateway_trust {
            volumes.push(Volume {
                name: "repository-gateway-trust".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(trust.ca_secret_name.clone()),
                    items: Some(vec![k8s_openapi::api::core::v1::KeyToPath {
                        key: trust.ca_secret_key.clone(),
                        path: "ca.crt".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: "repository-gateway-trust".into(),
                mount_path: awaken_container_contract::REPOSITORY_GATEWAY_CA_PATH
                    .strip_suffix("/ca.crt")
                    .expect("Repository CA path has one file component")
                    .into(),
                read_only: Some(true),
                ..Default::default()
            });
        }
        let mut sidecars: Vec<Container> = Vec::new();
        let mut init_containers: Vec<Container> = Vec::new();
        let mut memory_projector_mounts = Vec::new();
        for (i, mm) in plan.memory_mounts.iter().enumerate() {
            let vol = format!("mem-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol.clone(),
                mount_path: mm.mount_path.clone(),
                read_only: Some(mm.access == pc::MountAccess::ReadOnly),
                ..Default::default()
            });
            memory_projector_mounts.push(VolumeMount {
                name: vol,
                mount_path: format!("/memory/{i}"),
                ..Default::default()
            });
        }
        if !memory_projector_mounts.is_empty() {
            let effect_fence = effect_fence.expect(
                "canonical Kubernetes Memory projection requires an Environment effect fence",
            );
            sidecars.push(Container {
                name: memory::PROJECTOR.into(),
                image: Some(plan.image.clone()),
                command: Some(awaken_sandbox_container::engine::environment_keepalive_command()),
                env: Some(vec![EnvVar {
                    name: projection::PROJECTION_FENCE_ENV.into(),
                    value: Some(projection::projection_fence_value(effect_fence)),
                    value_from: None,
                }]),
                volume_mounts: Some(memory_projector_mounts),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
        }

        append_writable_and_cache_volumes(
            plan,
            continuation_claim,
            &mut volumes,
            &mut agent_mounts,
        );
        live_inputs::append_projection(
            plan,
            effect_fence,
            &mut volumes,
            &mut agent_mounts,
            &mut sidecars,
        );
        if plan
            .control_services
            .contains(&SandboxControlServiceKind::RepositoryGitCredential)
            && let Some(forwarder) = sandbox_control_forwarder
        {
            let (volume, agent_mount, forwarder_container) = sandbox_control::projection(forwarder);
            volumes.push(volume);
            agent_mounts.push(agent_mount);
            sidecars.push(forwarder_container);
        }

        // Inline content has no host path a Pod can bind, so every item is backed by
        // a ConfigMap created alongside the Pod, except Managed Files whose initial
        // and later generations share the runtime-owned live projector. Other paths
        // keep the exact read-only subPath projection used by bwrap parity.
        for (i, bind) in content_binds(plan).iter().enumerate() {
            if awaken_sandbox_container::engine::manages_live_input(bind) {
                continue;
            }
            let vol = format!("cfg-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                config_map: Some(ConfigMapVolumeSource {
                    name: configmap_name(id, i),
                    ..Default::default()
                }),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol,
                mount_path: bind.mount_path.clone(),
                sub_path: Some(CONFIGMAP_KEY.to_string()),
                read_only: Some(bind.read_only),
                ..Default::default()
            });
        }

        // A Kubernetes Secret volume is immutable/read-only. Seed each native OAuth
        // credential into a pod-scoped writable emptyDir in an init container, then
        // mount that config directory at the CLI's native home. The worker harvests it via the
        // exec subresource before terminating the agent and writes it back to the broker.
        for (i, bind) in credential_binds(plan).iter().enumerate() {
            let seed_vol = format!("credential-seed-{i}");
            if bind.credential_file_path.is_none() {
                volumes.push(Volume {
                    name: seed_vol.clone(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(credential_secret_name(id, i)),
                        default_mode: Some(0o440),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                agent_mounts.push(VolumeMount {
                    name: seed_vol,
                    mount_path: bind.mount_path.clone(),
                    sub_path: Some(credential_key(bind).to_string()),
                    read_only: Some(true),
                    ..Default::default()
                });
                continue;
            }
            let writable_vol = format!("credential-rw-{i}");
            volumes.push(Volume {
                name: seed_vol.clone(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(credential_secret_name(id, i)),
                    default_mode: Some(0o400),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volumes.push(Volume {
                name: writable_vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            init_containers.push(Container {
                name: format!("credential-init-{i}"),
                image: Some(plan.image.clone()),
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cp -a /seed/. /writable/ && chmod 600 /writable/*".into(),
                ]),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: seed_vol,
                        mount_path: "/seed".into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: writable_vol.clone(),
                        mount_path: "/writable".into(),
                        ..Default::default()
                    },
                ]),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: writable_vol,
                mount_path: bind.mount_path.clone(),
                read_only: Some(false),
                ..Default::default()
            });
        }

        let agent_command = effect_fence.map_or_else(
            || plan.command.clone(),
            |effect_fence| projection::gated_agent_command(&plan.command, effect_fence),
        );
        let mut containers = vec![Container {
            name: "agent".into(),
            image: Some(plan.image.clone()),
            // Session environment: PID 1 keeps the namespaces alive; attempt agents
            // are attached exec processes created by `spawn_agent`.
            command: Some(agent_command),
            env: Some(agent_env),
            // Enforce the advertised resource caps as the container's limits.
            resources: pod_resources(&plan.requests, &plan.limits),
            volume_mounts: (!agent_mounts.is_empty()).then_some(agent_mounts),
            // Harden the untrusted agent: no privilege escalation, all caps dropped.
            security_context: Some(hardened_security_context()),
            ..Default::default()
        }];
        containers.extend(sidecars);

        // Label the Pod with its egress posture so a platform-managed NetworkPolicy
        // selects and enforces it (deny-all-except-proxy/DNS for `restricted`). The
        // per-cluster NetworkPolicy holds the real proxy/DNS addresses; the adapter
        // only declares the posture — it does not invent IPs it doesn't have.
        let labels = sandbox_network_labels(Some(egress_label(&plan.network)));

        Pod {
            metadata: ObjectMeta {
                name: Some(pod_name(id)),
                labels: Some(labels),
                // native GC: the platform reaps this Pod when the owner is deleted.
                owner_references: owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                init_containers: (!init_containers.is_empty()).then_some(init_containers),
                containers,
                volumes: (!volumes.is_empty()).then_some(volumes),
                security_context: Some(PodSecurityContext {
                    fs_group: Some(awaken_sandbox_container::engine::SANDBOX_EXECUTION_ID),
                    ..Default::default()
                }),
                // a finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                // Disposable child/probe environments have no continuity to flush.
                // Make their canonical Pod deletion complete inside the bounded
                // provider-disposal contract; retained Sessions keep Kubernetes'
                // normal graceful-termination default.
                termination_grace_period_seconds: lifecycle::termination_grace_period(
                    plan.filesystem_continuity,
                ),
                // The untrusted agent must NOT reach the kube API (no SA token): its
                // only control-plane channel is the ACP data channel, nothing else.
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                image_pull_secrets: (!image_pull_secrets.is_empty()).then(|| {
                    image_pull_secrets
                        .iter()
                        .map(|name| LocalObjectReference { name: name.clone() })
                        .collect()
                }),
                host_aliases: repository_gateway_trust.map(|trust| {
                    vec![k8s_openapi::api::core::v1::HostAlias {
                        hostnames: Some(vec![trust.host.clone()]),
                        ip: trust.address.to_string(),
                    }]
                }),
                ..Default::default()
            }),
            status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract as pc;

    use super::*;
    use awaken_sandbox_container::{ContainerPlan, NetworkMode, RootfsPlan};

    #[test]
    fn cache_volume_projects_the_exact_existing_pvc() {
        // FMECA: F1 K8s ignores a CacheVolume bind (S7 O5 D3, RPN105);
        // F2 node hostPath is substituted for a portable claim (S9 O3 D4,
        // RPN108); F3 read-only intent is lost (S8 O2 D3, RPN48). The provider
        // resolves CacheVolume to a namespaced PVC reference before this pure
        // Pod projection; no second cache implementation exists here.
        // Cause graph: C1=PVC reference present; C2=read-only; C3=ordinary bind.
        // Effects: E1=PVC volume+mount; E2=read-only preserved; E3=no PVC.
        // | Rule | C1 | C2 | C3 | Effect |
        // | K1   | 1  | 1  | 0  | E1,E2  |
        // | K2   | 0  | -  | 1  | E3     |
        let plan = ContainerPlan {
            image: "agent:1".into(),
            command: vec!["sleep".into(), "30".into()],
            env: Vec::new(),
            control_services: Default::default(),
            packages: Default::default(),
            binds: vec![awaken_sandbox_container::BindPlan {
                source_ref: format!(
                    "{}build-cache-v7",
                    awaken_sandbox_container::engine::PVC_BIND_REF_PREFIX
                ),
                mount_path: "/workspace/.cache/build".into(),
                read_only: true,
                content: None,
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            }],
            outputs_volume: "/mnt/session/outputs".into(),
            network: NetworkMode::Open,
            egress_identity: Default::default(),
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: RootfsPlan::HostUserland,
        };
        let mut volumes = Vec::new();
        let mut mounts = Vec::new();
        append_writable_and_cache_volumes(&plan, None, &mut volumes, &mut mounts);
        let volume = volumes
            .iter()
            .find(|volume| volume.persistent_volume_claim.is_some())
            .expect("K1 PVC volume");
        assert_eq!(
            volume.persistent_volume_claim.as_ref().unwrap().claim_name,
            "build-cache-v7",
            "K1"
        );
        let mount = mounts
            .iter()
            .find(|mount| mount.name == volume.name)
            .expect("K1 PVC mount");
        assert_eq!(mount.mount_path, "/workspace/.cache/build", "K1");
        assert_eq!(mount.read_only, Some(true), "K1/K2");
    }
}
