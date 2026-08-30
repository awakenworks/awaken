//! Pure projection of neutral bind material into Kubernetes objects.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    ConfigMap, EmptyDirVolumeSource, PersistentVolumeClaimVolumeSource, Secret, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::names::{cfg_owner_label, configmap_name, credential_secret_name};
use crate::writable::{WritableRootTopology, writable_root_topology};
use crate::{BindPlan, ContainerPlan};

pub(super) fn build_pod(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    rendezvous: Option<&str>,
    image_pull_secrets: &[String],
) -> k8s_openapi::api::core::v1::Pod {
    super::build_pod_with_continuation(
        id,
        plan,
        owner,
        rendezvous,
        image_pull_secrets,
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
) -> Vec<String> {
    let writable = crate::writable_dirs(plan);
    let mut continuation_subpaths = Vec::new();
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
                continuation_subpaths.push(subpath.clone());
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
            .strip_prefix(crate::cache_volume::PVC_BIND_REF_PREFIX)
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
    continuation_subpaths
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

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract as pc;

    use super::*;
    use crate::{ContainerPlan, NetworkMode, RootfsPlan};

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
            binds: vec![crate::BindPlan {
                source_ref: format!("{}build-cache-v7", crate::cache_volume::PVC_BIND_REF_PREFIX),
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
