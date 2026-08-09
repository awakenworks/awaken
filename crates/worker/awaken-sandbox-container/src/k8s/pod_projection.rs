//! Pure projection of neutral bind material into Kubernetes objects.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    ConfigMap, EmptyDirVolumeSource, PersistentVolumeClaimVolumeSource, Secret, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::names::{cfg_owner_label, configmap_name, credential_secret_name};
use crate::{BindPlan, ContainerPlan};

pub(super) const CONFIGMAP_KEY: &str = "content";

pub(super) fn append_writable_and_cache_volumes(
    plan: &ContainerPlan,
    volumes: &mut Vec<Volume>,
    agent_mounts: &mut Vec<VolumeMount>,
) {
    for (index, directory) in crate::writable_dirs(plan).into_iter().enumerate() {
        let name = format!("rw-{index}");
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
