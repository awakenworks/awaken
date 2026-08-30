//! Kubernetes-native realization of immutable Environment package images.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, ConfigMap, ConfigMapVolumeSource, Container, EnvVar, KeyToPath,
    LocalObjectReference, Pod, PodSpec, PodTemplateSpec, SecretVolumeSource, SecurityContext,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams};
use kube::{Api, Client};

use crate::k8s::{
    backend, create_or_verify_exact, install_rustls_crypto_provider, sandbox_network_labels,
    stamp_realization,
};
use crate::k8s_package_realization::{
    PACKAGE_BUILD_JOB_KIND, PACKAGE_IMAGE_CHECK_JOB_KIND, bind_package_job_to_config,
    package_build_name, package_config_annotations, package_image_check_name,
    stamp_package_config_map, verified_package_build_result, verified_package_image_check_result,
    verify_exact_package_config_map, verify_exact_package_job,
};
use crate::{ForwardProxy, PackageImageProvisioner, RuntimeError};

pub const DEFAULT_K8S_BUILDKIT_IMAGE: &str = "moby/buildkit:v0.30.0-rootless";
// A cold desktop/multimedia image installs hundreds of Debian packages. On a
// fresh k3d node the package indexes alone can consume most of ten minutes, so
// the former ten-minute deadline killed a healthy BuildKit Job while it was
// downloading archives and the reconciler immediately restarted the same work.
// Keep the build bounded, but give cold package realization its own wider
// window instead of coupling it to the much cheaper image-pull probe below.
const PACKAGE_BUILD_TIMEOUT_SECS: i64 = 30 * 60;
// A desktop/browser Environment image can take several minutes to pull after
// kubelet garbage collection. Keep the availability check alive for the same
// bounded window; terminal pull errors are still detected and returned
// immediately by `terminal_image_pull_reason`. It does not need the package
// build's installation window.
const IMAGE_CHECK_TIMEOUT_SECS: i64 = 10 * 60;
const IMAGE_CHECK_CLIENT_GRACE_SECS: u64 = 10;

fn image_check_client_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(IMAGE_CHECK_TIMEOUT_SECS as u64 + IMAGE_CHECK_CLIENT_GRACE_SECS)
}

enum ImageCheckObservation {
    Missing,
    Present(k8s_openapi::api::batch::v1::JobStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageCheckDisposition {
    Available,
    Continue,
    Missing,
    Unavailable(&'static str),
}

fn image_check_observation(job: Option<Job>) -> ImageCheckObservation {
    match job {
        Some(job) => ImageCheckObservation::Present(job.status.unwrap_or_default()),
        None => ImageCheckObservation::Missing,
    }
}

fn immutable_registry_identity(identity: Option<String>) -> Option<String> {
    identity.filter(|reference| {
        reference
            .rsplit_once("@sha256:")
            .is_some_and(|(_, digest)| {
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
            })
    })
}

fn package_image_matches_stored_identity(observed: Option<String>, stored: &str) -> bool {
    immutable_registry_identity(Some(stored.to_owned())).is_some()
        && immutable_registry_identity(observed).as_deref() == Some(stored)
}

fn require_unrestricted_package_build(network: &pc::NetworkPolicy) -> Result<(), RuntimeError> {
    if network.is_restricted() {
        return Err(backend(
            "Kubernetes package builder cannot prove a no-bypass restricted build network",
        ));
    }
    Ok(())
}

fn image_check_identity(pods: Vec<Pod>) -> Option<String> {
    pods.into_iter()
        .find_map(|pod| {
            pod.status?
                .container_statuses?
                .into_iter()
                .find(|status| status.name == "verify")
                .map(|status| status.image_id)
        })
        .map(|identity| {
            identity
                .split_once("://")
                .map_or(identity.clone(), |(_, reference)| reference.to_owned())
        })
}

fn terminal_image_pull_reason(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("ErrImagePull" | "ImagePullBackOff" | "InvalidImageName")
    )
}

fn image_check_disposition(
    job_succeeded: bool,
    pull_failed: bool,
    job_failed: bool,
    timed_out: bool,
) -> ImageCheckDisposition {
    if job_succeeded {
        ImageCheckDisposition::Available
    } else if pull_failed {
        ImageCheckDisposition::Missing
    } else if job_failed {
        ImageCheckDisposition::Unavailable("Job failed before kubelet reported image availability")
    } else if timed_out {
        ImageCheckDisposition::Unavailable("timed out before kubelet reported image availability")
    } else {
        ImageCheckDisposition::Continue
    }
}

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
    forward_proxy: Option<ForwardProxy>,
}

impl K8sPackageImageProvisioner {
    pub async fn connect(
        namespace: impl Into<String>,
        registry: impl Into<String>,
        image_pull_secrets: Vec<String>,
        registry_insecure: bool,
    ) -> Result<Self, RuntimeError> {
        install_rustls_crypto_provider();
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
            buildkit_image: DEFAULT_K8S_BUILDKIT_IMAGE.into(),
            image_pull_secrets,
            registry_insecure,
            forward_proxy: None,
        })
    }

    /// Override the rootless BuildKit image with an operator-managed mirror.
    /// Air-gapped and rate-limited clusters must not depend on an implicit
    /// Docker Hub pull at Environment materialization time.
    pub fn with_buildkit_image(mut self, image: impl Into<String>) -> Result<Self, RuntimeError> {
        let image = image.into();
        if image.trim().is_empty() || image.chars().any(char::is_whitespace) {
            return Err(backend(
                "Kubernetes BuildKit image must be one non-empty OCI reference",
            ));
        }
        self.buildkit_image = image;
        Ok(self)
    }

    /// Reuse the deployment's cooperative container proxy while realizing an
    /// Environment image. BuildKit itself receives the proxy for remote image
    /// access and predefined build args carry it into package-manager RUN
    /// steps without persisting it in the resulting image.
    pub fn with_forward_proxy(mut self, proxy: ForwardProxy) -> Result<Self, RuntimeError> {
        if proxy.url.trim().is_empty()
            || proxy.url.chars().any(char::is_whitespace)
            || !(proxy.url.starts_with("http://") || proxy.url.starts_with("https://"))
        {
            return Err(backend(
                "Kubernetes package builder forward proxy must be one HTTP(S) URL",
            ));
        }
        self.forward_proxy = Some(proxy);
        Ok(self)
    }

    pub(crate) fn build_objects(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
    ) -> Result<(ConfigMap, Job, String), RuntimeError> {
        let (containerfile, fingerprint) =
            crate::packages::package_image_recipe(base_image, "", packages)?;
        let name = package_build_name(&fingerprint)?;
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
        let mut config = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                annotations: Some(package_config_annotations(&fingerprint, &destination)?),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };
        stamp_package_config_map(&mut config)?;
        stamp_realization(&mut config)?;
        let script = r#"
set -eu
mkdir -p /tmp/workspace
cp /input/Dockerfile /tmp/workspace/Dockerfile
if [ -n "${FORWARD_PROXY:-}" ]; then
  set -- \
    --opt "build-arg:HTTP_PROXY=$FORWARD_PROXY" \
    --opt "build-arg:HTTPS_PROXY=$FORWARD_PROXY" \
    --opt "build-arg:http_proxy=$FORWARD_PROXY" \
    --opt "build-arg:https_proxy=$FORWARD_PROXY" \
    --opt "build-arg:NO_PROXY=$NO_PROXY" \
    --opt "build-arg:no_proxy=$NO_PROXY"
else
  set --
fi
buildctl-daemonless.sh build \
  --frontend dockerfile.v0 \
  --local context=/tmp/workspace \
  --local dockerfile=/tmp/workspace \
  "$@" \
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
        let mut volume_mounts = vec![
            VolumeMount {
                name: "build-input".into(),
                mount_path: "/input".into(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: "buildkit-state".into(),
                mount_path: "/home/user/.local/share/buildkit".into(),
                ..Default::default()
            },
        ];
        let mut volumes = vec![
            Volume {
                name: "build-input".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Volume {
                name: "buildkit-state".into(),
                empty_dir: Some(Default::default()),
                ..Default::default()
            },
        ];
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
        let mut environment = vec![
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
        ];
        if let Some(proxy) = &self.forward_proxy {
            let no_proxy = format!("localhost,127.0.0.1,{registry_host},.svc,.cluster.local");
            environment.push(EnvVar {
                name: "FORWARD_PROXY".into(),
                value: Some(proxy.url.clone()),
                ..Default::default()
            });
            for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                environment.push(EnvVar {
                    name: name.into(),
                    value: Some(proxy.url.clone()),
                    ..Default::default()
                });
            }
            for name in ["NO_PROXY", "no_proxy"] {
                environment.push(EnvVar {
                    name: name.into(),
                    value: Some(no_proxy.clone()),
                    ..Default::default()
                });
            }
        }
        let container = Container {
            name: "buildkit".into(),
            image: Some(self.buildkit_image.clone()),
            image_pull_policy: Some("IfNotPresent".into()),
            command: Some(vec!["/bin/sh".into(), "-ceu".into()]),
            args: Some(vec![script.into()]),
            env: Some(environment),
            security_context: Some(SecurityContext {
                // RootlessKit enters a subordinate user namespace through the
                // image's setuid newuidmap/newgidmap helpers. no_new_privs would
                // disable those helpers and make every official rootless
                // BuildKit image fail before the build starts. The process is
                // still launched as the unprivileged uid/gid below and receives
                // no Kubernetes service-account token.
                allow_privilege_escalation: Some(true),
                run_as_non_root: Some(true),
                run_as_user: Some(1000),
                run_as_group: Some(1000),
                seccomp_profile: Some(k8s_openapi::api::core::v1::SeccompProfile {
                    type_: "Unconfined".into(),
                    ..Default::default()
                }),
                app_armor_profile: Some(AppArmorProfile {
                    type_: "Unconfined".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            volume_mounts: Some(volume_mounts),
            ..Default::default()
        };
        let mut labels = sandbox_network_labels(Some("open"));
        labels.insert(
            "app.kubernetes.io/managed-by".into(),
            "awaken-environment-builder".into(),
        );
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

    async fn ensure_package_config(&self, desired: &ConfigMap) -> Result<ConfigMap, RuntimeError> {
        let configs: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        create_or_verify_exact(&configs, desired, verify_exact_package_config_map).await
    }

    fn bind_package_job(
        &self,
        config: &ConfigMap,
        mut job: Job,
        kind: &str,
    ) -> Result<Job, RuntimeError> {
        bind_package_job_to_config(&mut job, config, kind)?;
        stamp_realization(&mut job)?;
        Ok(job)
    }

    async fn run_job(&self, job: Job) -> Result<String, RuntimeError> {
        let name = job
            .metadata
            .name
            .clone()
            .ok_or_else(|| backend("package build Job has no name"))?;
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        create_or_verify_exact(&jobs, &job, verify_exact_package_job).await?;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(PACKAGE_BUILD_TIMEOUT_SECS as u64 + 30);
        loop {
            let observed = jobs.get(&name).await.map_err(backend)?;
            let status = observed.status.as_ref().cloned().unwrap_or_default();
            if status.succeeded.unwrap_or_default() > 0 {
                let listed = pods
                    .list(&ListParams::default().labels(&format!("job-name={name}")))
                    .await
                    .map_err(backend)?;
                // The Job TTL retains the API-observed UID and controller-owned
                // Pod long enough for the composing release observer. Deleting
                // either object here would make a successful build unverifiable.
                return verified_package_build_result(&observed, &listed.items, &self.namespace);
            }
            if status.failed.unwrap_or_default() > 0 {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                return Err(backend(format!("BuildKit Job `{name}` failed")));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                return Err(backend(format!("BuildKit Job `{name}` timed out")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    pub(crate) fn image_check_job(
        &self,
        image: &str,
        package_config: Option<&ConfigMap>,
    ) -> Result<(String, Job), RuntimeError> {
        // A kubelet pull is the shared Registry truth for base identity and
        // persisted Ready verification; builder-local cache is never trusted.
        let name = package_image_check_name(image)?;
        let labels = sandbox_network_labels(None);
        let mut job = Job {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(JobSpec {
                active_deadline_seconds: Some(IMAGE_CHECK_TIMEOUT_SECS),
                backoff_limit: Some(0),
                ttl_seconds_after_finished: Some(60),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels),
                        ..Default::default()
                    }),
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
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Some(config) = package_config {
            bind_package_job_to_config(&mut job, config, PACKAGE_IMAGE_CHECK_JOB_KIND)?;
        }
        stamp_realization(&mut job)?;
        Ok((name, job))
    }

    async fn resolve_image_identity(
        &self,
        image: &str,
        package_config: Option<&ConfigMap>,
    ) -> Result<Option<String>, RuntimeError> {
        let (name, job) = self.image_check_job(image, package_config)?;
        let package = package_config.is_some();
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        create_or_verify_exact(&jobs, &job, |desired, observed| {
            if package {
                verify_exact_package_job(desired, observed)
            } else {
                Ok(())
            }
        })
        .await?;
        let deadline = tokio::time::Instant::now() + image_check_client_timeout();
        loop {
            let observed = jobs.get_opt(&name).await.map_err(backend)?;
            let status = match image_check_observation(observed.clone()) {
                ImageCheckObservation::Present(status) => status,
                ImageCheckObservation::Missing => {
                    // Image checks deliberately use a deterministic name so concurrent
                    // callers share the kubelet pull. A peer can observe the terminal
                    // Job and delete it between this caller's polls. Recreate that
                    // disposable observation instead of surfacing a false 404 to the
                    // Environment realization retry path.
                    create_or_verify_exact(&jobs, &job, |desired, observed| {
                        if package {
                            verify_exact_package_job(desired, observed)
                        } else {
                            Ok(())
                        }
                    })
                    .await?;
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
            let listed = pods
                .list(&ListParams::default().labels(&format!("job-name={name}")))
                .await
                .map_err(backend)?;
            let pull_failed = listed.items.iter().any(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|status| status.container_statuses.as_ref())
                    .into_iter()
                    .flatten()
                    .filter(|status| status.name == "verify")
                    .any(|status| {
                        terminal_image_pull_reason(
                            status
                                .state
                                .as_ref()
                                .and_then(|state| state.waiting.as_ref())
                                .and_then(|waiting| waiting.reason.as_deref()),
                        )
                    })
            });
            match image_check_disposition(
                status.succeeded.unwrap_or_default() > 0,
                pull_failed,
                status.failed.unwrap_or_default() > 0,
                tokio::time::Instant::now() >= deadline,
            ) {
                ImageCheckDisposition::Available => {
                    let identity = if package {
                        Some(verified_package_image_check_result(
                            observed.as_ref().expect("present image-check observation"),
                            &listed.items,
                            &self.namespace,
                        )?)
                    } else {
                        image_check_identity(listed.items)
                    };
                    // The Job's TTL is the cleanup owner. Retaining a successful
                    // deterministic observation lets concurrent readiness,
                    // warmup, and registration callers share one kubelet result
                    // instead of deleting and recreating the same Job in a loop.
                    return Ok(identity);
                }
                ImageCheckDisposition::Continue => {}
                ImageCheckDisposition::Missing => {
                    let _ = jobs.delete(&name, &DeleteParams::background()).await;
                    return Ok(None);
                }
                ImageCheckDisposition::Unavailable(reason) => {
                    let _ = jobs.delete(&name, &DeleteParams::background()).await;
                    return Err(backend(format!("Kubernetes image check `{name}` {reason}")));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[async_trait]
impl PackageImageProvisioner for K8sPackageImageProvisioner {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        // An operator-supplied digest is already the immutable identity required
        // by durable demand. Probing it again would create a second availability
        // path beside the BuildKit pull and Session readiness checks.
        if let Some(identity) = immutable_registry_identity(Some(reference.to_owned())) {
            return Ok(identity);
        }
        self.resolve_image_identity(reference, None)
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
        require_unrestricted_package_build(network)?;
        let (desired_config, job, destination) = self.build_objects(base_image, packages)?;
        let config = self.ensure_package_config(&desired_config).await?;
        let job = self.bind_package_job(&config, job, PACKAGE_BUILD_JOB_KIND)?;
        // The destination tag is a content fingerprint.  Coordinator build
        // records can be rebuilt after restart, but the shared Registry is the
        // cross-process source of truth.  Reuse its immutable digest instead of
        // downloading and reinstalling the same package set on every restart.
        if let Some(image) = immutable_registry_identity(
            self.resolve_image_identity(&destination, Some(&config))
                .await?,
        ) {
            return Ok(image);
        }
        let built = self.run_job(job).await?;
        let checked = self
            .resolve_image_identity(&destination, Some(&config))
            .await?
            .ok_or_else(|| backend("built package image is absent from the shared Registry"))?;
        if checked != built {
            return Err(backend(
                "BuildKit result differs from the mandatory kubelet image check",
            ));
        }
        Ok(checked)
    }

    async fn package_image_available(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
        image: &str,
    ) -> Result<bool, RuntimeError> {
        if packages.is_empty() {
            return Ok(self.resolve_image_identity(image, None).await?.is_some());
        }
        require_unrestricted_package_build(network)?;
        let (desired_config, _, destination) = self.build_objects(base_image, packages)?;
        let name = desired_config
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| backend("package ConfigMap has no name"))?;
        let configs: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(config) = configs.get_opt(name).await.map_err(backend)? else {
            return Ok(false);
        };
        verify_exact_package_config_map(&desired_config, &config)?;
        let observed = self
            .resolve_image_identity(&destination, Some(&config))
            .await?;
        Ok(package_image_matches_stored_identity(observed, image))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        IMAGE_CHECK_TIMEOUT_SECS, ImageCheckDisposition, ImageCheckObservation,
        K8sPackageImageProvisioner, image_check_client_timeout, image_check_disposition,
        image_check_identity, image_check_observation, immutable_registry_identity,
        package_image_matches_stored_identity, terminal_image_pull_reason,
    };
    use crate::k8s_package_realization::{
        K8S_PACKAGE_REALIZATION_CONTRACT_VERSION, PACKAGE_BUILD_JOB_KIND,
        PACKAGE_CONFIG_MAP_UID_ANNOTATION, PACKAGE_IMAGE_CHECK_JOB_KIND,
        PACKAGE_IMAGE_DESTINATION_ANNOTATION, PACKAGE_JOB_KIND_ANNOTATION,
        PACKAGE_REALIZATION_CONTRACT_ANNOTATION, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
    };

    fn test_builder() -> K8sPackageImageProvisioner {
        crate::k8s::install_rustls_crypto_provider();
        let config = kube::Config::new("http://127.0.0.1:1/".parse().unwrap());
        let client = kube::Client::try_from(config).unwrap();
        K8sPackageImageProvisioner::new(
            client,
            "awaken-system",
            "registry.local:5000/environments",
            vec!["registry-auth".into()],
            true,
        )
        .unwrap()
        .with_forward_proxy(crate::ForwardProxy {
            url: "http://proxy.internal:8080".into(),
        })
        .unwrap()
    }

    fn package_requirements() -> awaken_provisioning_contract::PackageRequirements {
        awaken_provisioning_contract::PackageRequirements {
            managers: [("npm".into(), vec!["@playwright/mcp@latest".into()])]
                .into_iter()
                .collect(),
            resolution_id: Some("env-browser:3".into()),
        }
    }

    #[tokio::test]
    async fn package_build_objects_expose_only_secret_free_release_correlation() {
        /* Release-correlation cause/effect table — R1:
         * C1 one deterministic recipe is pushed to one generated destination;
         * C2 the same Job carries private recipe, proxy, Registry auth, and base
         * image inputs; C3 package realization has already admitted only an
         * unrestricted build. Effects: E1 Job and Pod template expose the exact
         * recipe fingerprint and destination; E2 no C2 material enters
         * annotations; E3 both objects select the canonical sandbox default-deny
         * plus open-egress policies. Rule R1=C1+C2+C3=>E1+E2+E3. The termination
         * message remains the immutable
         * build result; these two fields are correlation only, not another build
         * record or completion authority.
         */
        let builder = test_builder();
        let packages = package_requirements();
        let (mut config, job, destination) = builder
            .build_objects("registry.local/base@sha256:exact", &packages)
            .unwrap();
        config.metadata.uid = Some("config-uid".into());
        let job = builder
            .bind_package_job(&config, job, PACKAGE_BUILD_JOB_KIND)
            .unwrap();
        let fingerprint = destination
            .rsplit_once(':')
            .map(|(_, fingerprint)| fingerprint)
            .unwrap();
        let job_annotations = job.metadata.annotations.as_ref().unwrap();
        let pod_annotations = job
            .spec
            .as_ref()
            .and_then(|spec| spec.template.metadata.as_ref())
            .and_then(|metadata| metadata.annotations.as_ref())
            .unwrap();
        let expected_labels = BTreeMap::from([
            ("app".to_owned(), "awaken-sandbox".to_owned()),
            ("awaken-egress".to_owned(), "open".to_owned()),
            (
                "app.kubernetes.io/managed-by".to_owned(),
                "awaken-environment-builder".to_owned(),
            ),
        ]);
        let pod_labels = job
            .spec
            .as_ref()
            .and_then(|spec| spec.template.metadata.as_ref())
            .and_then(|metadata| metadata.labels.as_ref());
        assert_eq!(
            job.metadata.labels.as_ref(),
            Some(&expected_labels),
            "R1/E3 Job"
        );
        assert_eq!(pod_labels, Some(&expected_labels), "R1/E3 Pod");

        for annotations in [job_annotations, pod_annotations] {
            assert_eq!(
                annotations
                    .get(PACKAGE_RECIPE_FINGERPRINT_ANNOTATION)
                    .map(String::as_str),
                Some(fingerprint),
                "R1/E1 exact deterministic recipe identity"
            );
            assert_eq!(
                annotations
                    .get(PACKAGE_IMAGE_DESTINATION_ANNOTATION)
                    .map(String::as_str),
                Some(destination.as_str()),
                "R1/E1 exact BuildKit push target"
            );
            assert_eq!(
                annotations
                    .get(PACKAGE_CONFIG_MAP_UID_ANNOTATION)
                    .map(String::as_str),
                Some("config-uid"),
                "R1/E1 API-observed ConfigMap incarnation"
            );
            assert_eq!(
                annotations
                    .get(PACKAGE_JOB_KIND_ANNOTATION)
                    .map(String::as_str),
                Some(PACKAGE_BUILD_JOB_KIND),
                "R1/E1 exact Job role"
            );
            assert_eq!(
                annotations
                    .get(PACKAGE_REALIZATION_CONTRACT_ANNOTATION)
                    .map(String::as_str),
                Some(K8S_PACKAGE_REALIZATION_CONTRACT_VERSION),
                "R1/E1 versioned contract"
            );
            assert!(
                annotations.values().all(|value| {
                    !value.contains("@playwright/mcp")
                        && !value.contains("proxy.internal")
                        && !value.contains("registry-auth")
                        && !value.contains("registry.local/base")
                }),
                "R1/E2 recipes, proxy coordinates, auth Secret names, and base inputs stay private"
            );
        }
    }

    #[tokio::test]
    async fn package_destination_retry_probe_preserves_release_correlation_and_digest() {
        /* Release-correlation cause/effect table — R2/R3:
         * C1 the destination is missing, so the existing BuildKit Job runs;
         * C2 BuildKit pushed it but the process crashed before its durable
         * receipt, so the retry reaches the existing destination image-check;
         * C3 a base/general image is checked outside package realization; C4 a
         * pulled image can execute untrusted entrypoint code during verification.
         * Effects: E1 the Build Job/Pod owns the recipe+destination pair; E2 the
         * check Job/Pod reuses that exact pair and kubelet imageID recovers the
         * immutable digest; E3 a generic check carries no package provenance;
         * E4 every check Job/Pod selects the canonical default-deny policy and
         * no open-egress policy. Rules: R2=C1=>E1; R3=C2+C4=>E2+E4;
         * R4=C3+C4=>E3+E4.
         */
        let builder = test_builder();
        let packages = package_requirements();
        let (mut config, build_job, destination) = builder
            .build_objects("registry.local/base@sha256:exact", &packages)
            .unwrap();
        config.metadata.uid = Some("config-uid".into());
        let build_job = builder
            .bind_package_job(&config, build_job, PACKAGE_BUILD_JOB_KIND)
            .unwrap();
        let build_annotations = build_job.metadata.annotations.as_ref().unwrap();
        let (_, retry_check) = builder
            .image_check_job(&destination, Some(&config))
            .unwrap();
        let retry_annotations = retry_check.metadata.annotations.as_ref().unwrap();
        let retry_pod_annotations = retry_check
            .spec
            .as_ref()
            .and_then(|spec| spec.template.metadata.as_ref())
            .and_then(|metadata| metadata.annotations.as_ref())
            .unwrap();
        let retry_image = retry_check
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| spec.containers.first())
            .and_then(|container| container.image.as_deref());

        for key in [
            PACKAGE_REALIZATION_CONTRACT_ANNOTATION,
            PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
            PACKAGE_IMAGE_DESTINATION_ANNOTATION,
            PACKAGE_CONFIG_MAP_UID_ANNOTATION,
        ] {
            assert_eq!(
                retry_annotations.get(key),
                build_annotations.get(key),
                "R3/E2 Job join key `{key}`"
            );
            assert_eq!(
                retry_pod_annotations.get(key),
                build_annotations.get(key),
                "R3/E2 Pod join key `{key}`"
            );
        }
        assert_eq!(
            retry_annotations
                .get(PACKAGE_JOB_KIND_ANNOTATION)
                .map(String::as_str),
            Some(PACKAGE_IMAGE_CHECK_JOB_KIND),
            "R3/E2 exact check role"
        );
        assert_eq!(retry_image, Some(destination.as_str()), "R3/E2 target");
        let deny_labels = BTreeMap::from([("app".to_owned(), "awaken-sandbox".to_owned())]);
        assert_eq!(
            retry_check.metadata.labels.as_ref(),
            Some(&deny_labels),
            "R3/E4 Job"
        );
        assert_eq!(
            retry_check
                .spec
                .as_ref()
                .and_then(|spec| spec.template.metadata.as_ref())
                .and_then(|metadata| metadata.labels.as_ref()),
            Some(&deny_labels),
            "R3/E4 Pod"
        );

        let digest = "a".repeat(64);
        let immutable = format!("registry.local:5000/environments/awaken-packages@sha256:{digest}");
        let observed = image_check_identity(vec![k8s_openapi::api::core::v1::Pod {
            status: Some(k8s_openapi::api::core::v1::PodStatus {
                container_statuses: Some(vec![k8s_openapi::api::core::v1::ContainerStatus {
                    image: destination.clone(),
                    image_id: format!("containerd://{immutable}"),
                    name: "verify".into(),
                    ready: false,
                    restart_count: 0,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        assert_eq!(
            immutable_registry_identity(observed),
            Some(immutable),
            "R3/E2 exact kubelet digest"
        );

        let (_, generic_check) = builder
            .image_check_job("registry.local/base:mutable", None)
            .unwrap();
        assert!(
            !generic_check
                .metadata
                .annotations
                .as_ref()
                .is_some_and(|annotations| {
                    annotations.contains_key(PACKAGE_REALIZATION_CONTRACT_ANNOTATION)
                }),
            "R4/E3 Job"
        );
        assert_eq!(
            generic_check.metadata.labels.as_ref(),
            Some(&deny_labels),
            "R4/E4 Job"
        );
        let generic_pod_metadata = generic_check
            .spec
            .as_ref()
            .and_then(|spec| spec.template.metadata.as_ref())
            .expect("R4 image-check Pod metadata");
        assert!(generic_pod_metadata.annotations.is_none(), "R4/E3 Pod");
        assert_eq!(
            generic_pod_metadata.labels.as_ref(),
            Some(&deny_labels),
            "R4/E4 Pod"
        );
    }

    #[test]
    fn image_check_lifecycle_has_one_shared_success_and_bounded_retry_path() {
        // Causes: C1 a deterministic check is absent after TTL/peer cleanup; C2
        // its Job succeeds; C3 kubelet reports a terminal pull miss; C4 the Job
        // fails or exceeds its deadline. Effects: E1 recreate the same named
        // observation; E2 retain successful observation for TTL sharing; E3
        // delete and report missing so BuildKit may run; E4 delete and report a
        // bounded unavailable error. Rules: R1 C1=>E1; R2 C2=>E2; R3 C3=>E3;
        // R4 C4=>E4. No rule creates a parallel cache or durable Job authority.
        assert!(
            matches!(
                image_check_observation(None),
                ImageCheckObservation::Missing
            ),
            "R1"
        );
        assert_eq!(
            image_check_disposition(true, false, false, false),
            ImageCheckDisposition::Available,
            "R2"
        );
        assert_eq!(
            image_check_disposition(false, true, false, false),
            ImageCheckDisposition::Missing,
            "R3"
        );
        assert!(
            matches!(
                image_check_disposition(false, false, true, false),
                ImageCheckDisposition::Unavailable(_)
            ),
            "R4"
        );
    }

    #[test]
    fn only_an_exact_registry_digest_can_short_circuit_a_package_build() {
        // Causes: C1 an operator base or cached destination carries one exact
        // sha256 digest; C2 it is mutable or malformed. Effects: E1 reuse that
        // identity without a redundant kubelet probe; E2 fail closed and keep
        // the authoritative pull/build path. Rules: R1 C1=>E1; R2 C2=>E2.
        let digest = "a".repeat(64);
        let exact = format!("registry.local/environments/awaken-packages@sha256:{digest}");
        assert_eq!(
            immutable_registry_identity(Some(exact.clone())),
            Some(exact),
            "R1: an immutable base/destination is reusable across Coordinator restarts"
        );
        assert_eq!(
            immutable_registry_identity(Some(
                "registry.local/environments/awaken-packages:mutable".into()
            )),
            None,
            "R2: a mutable tag must still execute the authoritative probe/build path"
        );
        assert_eq!(
            immutable_registry_identity(Some(
                "registry.local/environments/awaken-packages@sha256:short".into()
            )),
            None,
            "R2: a malformed digest must fail closed"
        );
    }

    #[test]
    fn demand_aware_availability_accepts_only_the_exact_stored_digest() {
        // Cause/effect decision table for the side-effect-free availability
        // decision after the annotated destination probe: R1 exact immutable
        // kubelet imageID equals the Ready row -> available; R2 destination is
        // missing -> unavailable; R3 another digest is observed -> unavailable;
        // R4 the stored identity is mutable/malformed -> unavailable. The
        // Coordinator test owns the resulting Ready invalidation and proves no
        // build occurs outside the claim/lease worker; build_objects tests own
        // the exact recipe/destination annotations supplied to the probe.
        let digest = "a".repeat(64);
        let stored = format!("registry.local/environments/awaken-packages@sha256:{digest}");
        assert!(
            package_image_matches_stored_identity(Some(stored.clone()), &stored),
            "R1 exact destination digest"
        );
        assert!(
            !package_image_matches_stored_identity(None, &stored),
            "R2 missing destination"
        );
        let drifted = format!(
            "registry.local/environments/awaken-packages@sha256:{}",
            "b".repeat(64)
        );
        assert!(
            !package_image_matches_stored_identity(Some(drifted), &stored),
            "R3 digest drift"
        );
        assert!(
            !package_image_matches_stored_identity(
                Some("registry.local/environments/awaken-packages:mutable".into()),
                "registry.local/environments/awaken-packages:mutable",
            ),
            "R4 mutable stored identity"
        );
    }

    #[test]
    fn only_a_terminal_pull_result_means_the_registry_image_is_missing() {
        assert_eq!(
            image_check_disposition(false, true, false, false),
            ImageCheckDisposition::Missing
        );
        assert!(matches!(
            image_check_disposition(false, false, true, false),
            ImageCheckDisposition::Unavailable(_)
        ));
        assert!(matches!(
            image_check_disposition(false, false, false, true),
            ImageCheckDisposition::Unavailable(_)
        ));
        assert_eq!(
            image_check_disposition(false, false, false, false),
            ImageCheckDisposition::Continue
        );
    }

    #[test]
    fn a_terminal_kubelet_pull_failure_falls_through_to_build_without_waiting_for_job_timeout() {
        for reason in ["ErrImagePull", "ImagePullBackOff", "InvalidImageName"] {
            assert!(terminal_image_pull_reason(Some(reason)), "{reason}");
        }
        for reason in [None, Some("ContainerCreating"), Some("PodInitializing")] {
            assert!(!terminal_image_pull_reason(reason), "{reason:?}");
        }
    }

    #[test]
    fn a_large_uncached_environment_image_gets_a_bounded_multi_minute_pull_window() {
        const { assert!(IMAGE_CHECK_TIMEOUT_SECS >= 10 * 60) };
        assert!(
            image_check_client_timeout()
                > std::time::Duration::from_secs(IMAGE_CHECK_TIMEOUT_SECS as u64),
            "the observer must outlive the Kubernetes Job deadline"
        );
    }

    #[test]
    fn a_cold_desktop_package_build_outlives_the_observed_ten_minute_failure() {
        const { assert!(super::PACKAGE_BUILD_TIMEOUT_SECS >= 30 * 60) };
        const { assert!(super::PACKAGE_BUILD_TIMEOUT_SECS > IMAGE_CHECK_TIMEOUT_SECS) };
    }
}
