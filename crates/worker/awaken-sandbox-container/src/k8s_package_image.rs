//! Kubernetes-native realization of immutable Environment package images.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EnvVar, KeyToPath, LocalObjectReference, Pod,
    PodSpec, PodTemplateSpec, SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};

use crate::k8s::{api_conflict, backend};
use crate::{PackageImageProvisioner, RuntimeError};

const DEFAULT_BUILDKIT_IMAGE: &str = "moby/buildkit:v0.30.0-rootless";
const PACKAGE_BUILD_TIMEOUT_SECS: i64 = 10 * 60;

/// A short-lived rootless BuildKit Job builds one deterministic destination and
/// pushes it to the shared Registry. Coordinator's database remains the sole
/// job and lease authority.
pub struct K8sPackageImageProvisioner {
    client: Client,
    namespace: String,
    registry: String,
    buildkit_image: String,
    image_pull_secrets: Vec<String>,
    registry_insecure: bool,
}

impl K8sPackageImageProvisioner {
    pub async fn connect(
        namespace: impl Into<String>,
        registry: impl Into<String>,
        image_pull_secrets: Vec<String>,
        registry_insecure: bool,
    ) -> Result<Self, RuntimeError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::try_default().await.map_err(backend)?;
        Self::new(
            client,
            namespace,
            registry,
            image_pull_secrets,
            registry_insecure,
        )
    }

    pub(crate) fn new(
        client: Client,
        namespace: impl Into<String>,
        registry: impl Into<String>,
        image_pull_secrets: Vec<String>,
        registry_insecure: bool,
    ) -> Result<Self, RuntimeError> {
        let namespace = namespace.into();
        let registry = registry.into().trim_end_matches('/').to_owned();
        if namespace.trim().is_empty() || registry.trim().is_empty() {
            return Err(backend(
                "Kubernetes package builder requires a namespace and Registry repository prefix",
            ));
        }
        if !registry.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '/')
        }) {
            return Err(backend(
                "Kubernetes package Registry prefix contains unsupported characters",
            ));
        }
        Ok(Self {
            client,
            namespace,
            registry,
            buildkit_image: DEFAULT_BUILDKIT_IMAGE.into(),
            image_pull_secrets,
            registry_insecure,
        })
    }

    pub(crate) fn build_objects(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
    ) -> Result<(ConfigMap, Job, String), RuntimeError> {
        let (containerfile, fingerprint) =
            crate::packages::package_image_recipe(base_image, "", packages)?;
        let name = format!("awaken-package-{}", &fingerprint[..24]);
        let destination = format!("{}/awaken-packages:{fingerprint}", self.registry);
        let registry_host = self
            .registry
            .split('/')
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| backend("package Registry prefix has no host"))?;
        let buildkitd = self
            .registry_insecure
            .then(|| format!("[registry.\"{registry_host}\"]\n  http = true\n  insecure = true\n"));
        let mut data = BTreeMap::from([("Dockerfile".into(), containerfile)]);
        if let Some(buildkitd) = &buildkitd {
            data.insert("buildkitd.toml".into(), buildkitd.clone());
        }
        let config = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };
        let script = r#"
set -eu
cp /input/Dockerfile /workspace/Dockerfile
buildctl-daemonless.sh build \
  --frontend dockerfile.v0 \
  --local context=/workspace \
  --local dockerfile=/workspace \
  --output type=image,name="$DESTINATION",push=true \
  --metadata-file /tmp/build-metadata.json
digest=$(sed -n 's/.*"containerimage.digest"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' /tmp/build-metadata.json | head -n 1)
test -n "$digest"
printf '%s@%s' "${DESTINATION%:*}" "$digest" > /dev/termination-log
"#;
        let config_args = if buildkitd.is_some() {
            " --config /input/buildkitd.toml"
        } else {
            ""
        };
        let mut volume_mounts = vec![VolumeMount {
            name: "build-input".into(),
            mount_path: "/input".into(),
            read_only: Some(true),
            ..Default::default()
        }];
        let mut volumes = vec![Volume {
            name: "build-input".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: name.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }];
        if let Some(secret_name) = self.image_pull_secrets.first() {
            volume_mounts.push(VolumeMount {
                name: "registry-auth".into(),
                mount_path: "/home/user/.docker".into(),
                read_only: Some(true),
                ..Default::default()
            });
            volumes.push(Volume {
                name: "registry-auth".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(secret_name.clone()),
                    items: Some(vec![KeyToPath {
                        key: ".dockerconfigjson".into(),
                        path: "config.json".into(),
                        mode: Some(0o400),
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let container = Container {
            name: "buildkit".into(),
            image: Some(self.buildkit_image.clone()),
            image_pull_policy: Some("IfNotPresent".into()),
            command: Some(vec!["/bin/sh".into(), "-ceu".into()]),
            args: Some(vec![script.into()]),
            env: Some(vec![
                EnvVar {
                    name: "DESTINATION".into(),
                    value: Some(destination.clone()),
                    ..Default::default()
                },
                EnvVar {
                    name: "BUILDKITD_FLAGS".into(),
                    value: Some(format!("--oci-worker-no-process-sandbox{config_args}")),
                    ..Default::default()
                },
            ]),
            security_context: Some(SecurityContext {
                allow_privilege_escalation: Some(false),
                run_as_non_root: Some(true),
                run_as_user: Some(1000),
                run_as_group: Some(1000),
                seccomp_profile: Some(k8s_openapi::api::core::v1::SeccompProfile {
                    type_: "Unconfined".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            volume_mounts: Some(volume_mounts),
            ..Default::default()
        };
        let labels = BTreeMap::from([(
            "app.kubernetes.io/managed-by".into(),
            "awaken-environment-builder".into(),
        )]);
        let job = Job {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(JobSpec {
                active_deadline_seconds: Some(PACKAGE_BUILD_TIMEOUT_SECS),
                backoff_limit: Some(0),
                ttl_seconds_after_finished: Some(300),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        automount_service_account_token: Some(false),
                        containers: vec![container],
                        image_pull_secrets: Some(
                            self.image_pull_secrets
                                .iter()
                                .cloned()
                                .map(|name| LocalObjectReference { name })
                                .collect(),
                        ),
                        restart_policy: Some("Never".into()),
                        volumes: Some(volumes),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok((config, job, destination))
    }

    async fn run_job(&self, config: ConfigMap, job: Job) -> Result<String, RuntimeError> {
        let name = job
            .metadata
            .name
            .clone()
            .ok_or_else(|| backend("package build Job has no name"))?;
        let configs: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Err(error) = configs.create(&PostParams::default(), &config).await
            && !api_conflict(&error)
        {
            return Err(backend(error));
        }
        if let Err(error) = jobs.create(&PostParams::default(), &job).await
            && !api_conflict(&error)
        {
            return Err(backend(error));
        }
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(PACKAGE_BUILD_TIMEOUT_SECS as u64 + 30);
        loop {
            let status = jobs
                .get(&name)
                .await
                .map_err(backend)?
                .status
                .unwrap_or_default();
            if status.succeeded.unwrap_or_default() > 0 {
                let listed = pods
                    .list(&ListParams::default().labels(&format!("job-name={name}")))
                    .await
                    .map_err(backend)?;
                let image = listed.items.into_iter().find_map(|pod| {
                    pod.status?
                        .container_statuses?
                        .into_iter()
                        .find(|status| status.name == "buildkit")?
                        .state?
                        .terminated?
                        .message
                });
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                let _ = configs.delete(&name, &DeleteParams::background()).await;
                return image.ok_or_else(|| backend("BuildKit Job returned no immutable digest"));
            }
            if status.failed.unwrap_or_default() > 0 {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                let _ = configs.delete(&name, &DeleteParams::background()).await;
                return Err(backend(format!("BuildKit Job `{name}` failed")));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                let _ = configs.delete(&name, &DeleteParams::background()).await;
                return Err(backend(format!("BuildKit Job `{name}` timed out")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn resolve_image_identity(&self, image: &str) -> Result<Option<String>, RuntimeError> {
        // A kubelet pull is the shared Registry truth for base identity and
        // persisted Ready verification; builder-local cache is never trusted.
        let fingerprint = blake3::hash(image.as_bytes()).to_hex().to_string();
        let name = format!("awaken-image-check-{}", &fingerprint[..20]);
        let job = Job {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: Some(JobSpec {
                active_deadline_seconds: Some(60),
                backoff_limit: Some(0),
                ttl_seconds_after_finished: Some(60),
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        automount_service_account_token: Some(false),
                        containers: vec![Container {
                            name: "verify".into(),
                            image: Some(image.into()),
                            image_pull_policy: Some("Always".into()),
                            command: Some(vec!["/usr/bin/env".into(), "true".into()]),
                            ..Default::default()
                        }],
                        image_pull_secrets: Some(
                            self.image_pull_secrets
                                .iter()
                                .cloned()
                                .map(|name| LocalObjectReference { name })
                                .collect(),
                        ),
                        restart_policy: Some("Never".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Err(error) = jobs.create(&PostParams::default(), &job).await
            && !api_conflict(&error)
        {
            return Err(backend(error));
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(70);
        loop {
            let status = jobs
                .get(&name)
                .await
                .map_err(backend)?
                .status
                .unwrap_or_default();
            if status.succeeded.unwrap_or_default() > 0 {
                let listed = pods
                    .list(&ListParams::default().labels(&format!("job-name={name}")))
                    .await
                    .map_err(backend)?;
                let identity = listed.items.into_iter().find_map(|pod| {
                    pod.status?
                        .container_statuses?
                        .into_iter()
                        .find(|status| status.name == "verify")
                        .map(|status| status.image_id)
                });
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                return Ok(identity.map(|identity| {
                    identity
                        .split_once("://")
                        .map_or(identity.clone(), |(_, reference)| reference.to_owned())
                }));
            }
            if status.failed.unwrap_or_default() > 0 || tokio::time::Instant::now() >= deadline {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                return Ok(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[async_trait]
impl PackageImageProvisioner for K8sPackageImageProvisioner {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        self.resolve_image_identity(reference)
            .await?
            .filter(|identity| identity.contains("@sha256:"))
            .ok_or_else(|| backend("kubelet returned no immutable base-image digest"))
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            return Ok(base_image.to_owned());
        }
        if network.is_restricted() {
            return Err(backend(
                "Kubernetes package builder cannot prove a no-bypass restricted build network",
            ));
        }
        let (config, job, _) = self.build_objects(base_image, packages)?;
        self.run_job(config, job).await
    }

    async fn package_image_available(&self, image: &str) -> Result<bool, RuntimeError> {
        Ok(self.resolve_image_identity(image).await?.is_some())
    }
}
