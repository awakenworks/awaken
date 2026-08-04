//! Pure projection of neutral bind/resource requirements into Kubernetes objects.

use std::collections::BTreeMap;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{ConfigMap, ResourceRequirements, Secret};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use super::names::{cfg_owner_label, configmap_name, credential_secret_name};
use crate::{BindPlan, ContainerPlan};

const CONFIGMAP_KEY: &str = "content";

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

pub(super) fn pod_resources(limits: &pc::ResourceLimits) -> Option<ResourceRequirements> {
    if !limits.is_set() {
        return None;
    }
    let mut projected = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        projected.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(memory) = limits.memory_bytes {
        projected.insert("memory".to_string(), Quantity(memory.to_string()));
    }
    if let Some(disk) = limits.disk_bytes {
        projected.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    if projected.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        limits: Some(projected),
        ..Default::default()
    })
}

pub(super) fn unenforceable_k8s_limit(limits: &pc::ResourceLimits) -> Option<&'static str> {
    limits.pids.map(|_| "pids")
}
