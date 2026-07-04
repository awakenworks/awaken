use std::collections::HashMap;
use std::sync::Arc;

use awaken_file_store::FileStore;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::SandboxError;

// ──────────────────────────────────────────────────────────────────────────────
// Mount source
// ──────────────────────────────────────────────────────────────────────────────

/// Origin of a Kubernetes volume mount.
#[derive(Debug, Clone)]
pub enum K8sMountSource {
    /// Binary blob fetched from the file store.
    ///
    /// The materializer creates a ConfigMap holding the base64-encoded content
    /// and generates an `emptyDir` volume with an `initContainer` that decodes
    /// it before the main containers start.
    Blob { content_id: String },
    /// A Kubernetes Secret that already exists in the cluster.
    Secret { secret_name: String },
    /// A Kubernetes ConfigMap that already exists in the cluster.
    ConfigMap { config_map_name: String },
}

/// A single Kubernetes mount request.
#[derive(Debug, Clone)]
pub struct K8sMount {
    /// Unique name used for the generated volume and related resources.
    ///
    /// Must be a valid Kubernetes DNS label (lowercase, alphanumeric, hyphens).
    pub name: String,
    /// Source of the volume content.
    pub source: K8sMountSource,
    /// Absolute path inside the main container where the volume is mounted.
    pub mount_path: String,
    /// Whether the mount should be read-only.
    pub read_only: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// K8s API types (minimal — sufficient for PodSpec patch and ConfigMap creation)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sObjectMeta {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Kubernetes `ConfigMap` spec produced by [`K8sMountMaterializer`] for blob
/// mounts.  Apply this to the cluster before the pod launches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sConfigMap {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: K8sObjectMeta,
    pub data: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sSecretVolumeSource {
    #[serde(rename = "secretName")]
    pub secret_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sConfigMapVolumeSource {
    pub name: String,
}

/// A Kubernetes `Volume` entry for `PodSpec.volumes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sVolume {
    pub name: String,
    #[serde(rename = "emptyDir", skip_serializing_if = "Option::is_none")]
    pub empty_dir: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<K8sSecretVolumeSource>,
    #[serde(rename = "configMap", skip_serializing_if = "Option::is_none")]
    pub config_map: Option<K8sConfigMapVolumeSource>,
}

/// A Kubernetes `VolumeMount` entry for a container's `volumeMounts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sVolumeMount {
    pub name: String,
    #[serde(rename = "mountPath")]
    pub mount_path: String,
    #[serde(rename = "readOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(rename = "subPath", skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
}

/// A Kubernetes `EnvVar` referencing a ConfigMap key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sEnvVar {
    pub name: String,
    #[serde(rename = "valueFrom")]
    pub value_from: K8sEnvVarSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sEnvVarSource {
    #[serde(rename = "configMapKeyRef")]
    pub config_map_key_ref: K8sConfigMapKeyRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sConfigMapKeyRef {
    pub name: String,
    pub key: String,
}

/// A Kubernetes init-container spec produced for blob mounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sInitContainer {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<K8sEnvVar>,
    #[serde(rename = "volumeMounts")]
    pub volume_mounts: Vec<K8sVolumeMount>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Materializer output
// ──────────────────────────────────────────────────────────────────────────────

/// All Kubernetes objects and PodSpec patch fields produced by
/// [`K8sMountMaterializer::prepare`].
///
/// Callers must:
/// 1. Apply each [`K8sConfigMap`] in `config_maps` to the cluster before
///    creating the pod.
/// 2. Merge `volumes` into `PodSpec.volumes`.
/// 3. Append `volume_mounts` to each main container's `volumeMounts`.
/// 4. Append `init_containers` to `PodSpec.initContainers`.
#[derive(Debug, Clone)]
pub struct K8sMountSpec {
    /// ConfigMaps to create in the cluster (one per blob mount).
    pub config_maps: Vec<K8sConfigMap>,
    /// Volumes for `PodSpec.volumes`.
    pub volumes: Vec<K8sVolume>,
    /// VolumeMounts for the main container's `volumeMounts`.
    pub volume_mounts: Vec<K8sVolumeMount>,
    /// InitContainers for `PodSpec.initContainers` (one per blob mount).
    pub init_containers: Vec<K8sInitContainer>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Materializer
// ──────────────────────────────────────────────────────────────────────────────

/// Prepares Kubernetes volume specs for a list of [`K8sMount`]s.
///
/// For blob mounts the content is fetched from the file store, base64-encoded,
/// and placed in a `ConfigMap` that an `initContainer` reads at pod start time.
/// Secret and ConfigMap mounts produce direct volume references with no
/// additional cluster resources.
pub struct K8sMountMaterializer {
    store: Arc<FileStore>,
}

impl K8sMountMaterializer {
    pub fn new(store: Arc<FileStore>) -> Self {
        Self { store }
    }

    /// Prepare Kubernetes specs for `mounts`.
    pub async fn prepare(&self, mounts: &[K8sMount]) -> Result<K8sMountSpec, SandboxError> {
        let mut config_maps = Vec::new();
        let mut volumes = Vec::new();
        let mut volume_mounts = Vec::new();
        let mut init_containers = Vec::new();

        for mount in mounts {
            let vol_name = format!("awaken-vol-{}", mount.name);

            match &mount.source {
                K8sMountSource::Blob { content_id } => {
                    let blob = self.store.get(content_id).await?;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
                    let cm_name = format!("awaken-cfg-{}", mount.name);
                    let init_name = format!("awaken-init-{}", mount.name);
                    let cfg_vol_name = format!("awaken-cfgvol-{}", mount.name);

                    // ConfigMap holding the base64-encoded blob.
                    let mut data = HashMap::new();
                    data.insert("content".to_owned(), encoded);
                    config_maps.push(K8sConfigMap {
                        api_version: "v1".to_owned(),
                        kind: "ConfigMap".to_owned(),
                        metadata: K8sObjectMeta {
                            name: cm_name.clone(),
                            namespace: None,
                        },
                        data,
                    });

                    // emptyDir to receive the decoded blob.
                    volumes.push(K8sVolume {
                        name: vol_name.clone(),
                        empty_dir: Some(serde_json::json!({})),
                        secret: None,
                        config_map: None,
                    });

                    // ConfigMap volume for the initContainer to read from.
                    volumes.push(K8sVolume {
                        name: cfg_vol_name.clone(),
                        empty_dir: None,
                        secret: None,
                        config_map: Some(K8sConfigMapVolumeSource {
                            name: cm_name.clone(),
                        }),
                    });

                    // initContainer: decode ConfigMap content into emptyDir.
                    init_containers.push(K8sInitContainer {
                        name: init_name,
                        image: "busybox:stable".to_owned(),
                        command: vec![
                            "sh".to_owned(),
                            "-c".to_owned(),
                            "base64 -d /awaken-cfg/content > /awaken-data/content".to_owned(),
                        ],
                        env: vec![],
                        volume_mounts: vec![
                            K8sVolumeMount {
                                name: cfg_vol_name,
                                mount_path: "/awaken-cfg".to_owned(),
                                read_only: Some(true),
                                sub_path: None,
                            },
                            K8sVolumeMount {
                                name: vol_name.clone(),
                                mount_path: "/awaken-data".to_owned(),
                                read_only: None,
                                sub_path: None,
                            },
                        ],
                    });

                    // Main container mounts the emptyDir, using subPath so the
                    // blob appears at the exact mount_path (file mount).
                    volume_mounts.push(K8sVolumeMount {
                        name: vol_name,
                        mount_path: mount.mount_path.clone(),
                        read_only: if mount.read_only { Some(true) } else { None },
                        sub_path: Some("content".to_owned()),
                    });
                }

                K8sMountSource::Secret { secret_name } => {
                    volumes.push(K8sVolume {
                        name: vol_name.clone(),
                        empty_dir: None,
                        secret: Some(K8sSecretVolumeSource {
                            secret_name: secret_name.clone(),
                        }),
                        config_map: None,
                    });
                    volume_mounts.push(K8sVolumeMount {
                        name: vol_name,
                        mount_path: mount.mount_path.clone(),
                        read_only: if mount.read_only { Some(true) } else { None },
                        sub_path: None,
                    });
                }

                K8sMountSource::ConfigMap { config_map_name } => {
                    volumes.push(K8sVolume {
                        name: vol_name.clone(),
                        empty_dir: None,
                        secret: None,
                        config_map: Some(K8sConfigMapVolumeSource {
                            name: config_map_name.clone(),
                        }),
                    });
                    volume_mounts.push(K8sVolumeMount {
                        name: vol_name,
                        mount_path: mount.mount_path.clone(),
                        read_only: if mount.read_only { Some(true) } else { None },
                        sub_path: None,
                    });
                }
            }
        }

        Ok(K8sMountSpec {
            config_maps,
            volumes,
            volume_mounts,
            init_containers,
        })
    }
}
