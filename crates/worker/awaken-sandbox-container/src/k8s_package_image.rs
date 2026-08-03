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

use crate::k8s::{api_conflict, backend, install_rustls_crypto_provider};
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

fn terminal_image_pull_reason(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("ErrImagePull" | "ImagePullBackOff" | "InvalidImageName")
    )
}

fn image_check_disposition(
    pull_failed: bool,
    job_failed: bool,
    timed_out: bool,
) -> ImageCheckDisposition {
    if pull_failed {
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
                active_deadline_seconds: Some(IMAGE_CHECK_TIMEOUT_SECS),
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
        let deadline = tokio::time::Instant::now() + image_check_client_timeout();
        loop {
            let status = match image_check_observation(jobs.get_opt(&name).await.map_err(backend)?)
            {
                ImageCheckObservation::Present(status) => status,
                ImageCheckObservation::Missing => {
                    // Image checks deliberately use a deterministic name so concurrent
                    // callers share the kubelet pull. A peer can observe the terminal
                    // Job and delete it between this caller's polls. Recreate that
                    // disposable observation instead of surfacing a false 404 to the
                    // Environment realization retry path.
                    if let Err(error) = jobs.create(&PostParams::default(), &job).await
                        && !api_conflict(&error)
                    {
                        return Err(backend(error));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
            };
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
                pull_failed,
                status.failed.unwrap_or_default() > 0,
                tokio::time::Instant::now() >= deadline,
            ) {
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
        let (config, job, destination) = self.build_objects(base_image, packages)?;
        // The destination tag is a content fingerprint.  Coordinator build
        // records can be rebuilt after restart, but the shared Registry is the
        // cross-process source of truth.  Reuse its immutable digest instead of
        // downloading and reinstalling the same package set on every restart.
        if let Some(image) =
            immutable_registry_identity(self.resolve_image_identity(&destination).await?)
        {
            return Ok(image);
        }
        self.run_job(config, job).await
    }

    async fn package_image_available(&self, image: &str) -> Result<bool, RuntimeError> {
        Ok(self.resolve_image_identity(image).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IMAGE_CHECK_TIMEOUT_SECS, ImageCheckDisposition, ImageCheckObservation,
        image_check_client_timeout, image_check_disposition, image_check_observation,
        immutable_registry_identity, terminal_image_pull_reason,
    };

    #[test]
    fn a_concurrently_deleted_image_check_is_recreated_instead_of_becoming_a_404() {
        assert!(matches!(
            image_check_observation(None),
            ImageCheckObservation::Missing
        ));
    }

    #[test]
    fn only_an_exact_registry_digest_can_short_circuit_a_package_build() {
        let digest = "a".repeat(64);
        let exact = format!("registry.local/environments/awaken-packages@sha256:{digest}");
        assert_eq!(
            immutable_registry_identity(Some(exact.clone())),
            Some(exact),
            "a kubelet-confirmed immutable digest is reusable across Coordinator restarts"
        );
        assert_eq!(
            immutable_registry_identity(Some(
                "registry.local/environments/awaken-packages:mutable".into()
            )),
            None,
            "a mutable tag must still execute the BuildKit path"
        );
        assert_eq!(
            immutable_registry_identity(Some(
                "registry.local/environments/awaken-packages@sha256:short".into()
            )),
            None,
            "a malformed digest must fail closed"
        );
    }

    #[test]
    fn only_a_terminal_pull_result_means_the_registry_image_is_missing() {
        assert_eq!(
            image_check_disposition(true, false, false),
            ImageCheckDisposition::Missing
        );
        assert!(matches!(
            image_check_disposition(false, true, false),
            ImageCheckDisposition::Unavailable(_)
        ));
        assert!(matches!(
            image_check_disposition(false, false, true),
            ImageCheckDisposition::Unavailable(_)
        ));
        assert_eq!(
            image_check_disposition(false, false, false),
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
        assert!(IMAGE_CHECK_TIMEOUT_SECS >= 10 * 60);
        assert!(
            image_check_client_timeout()
                > std::time::Duration::from_secs(IMAGE_CHECK_TIMEOUT_SECS as u64),
            "the observer must outlive the Kubernetes Job deadline"
        );
    }

    #[test]
    fn a_cold_desktop_package_build_outlives_the_observed_ten_minute_failure() {
        assert!(super::PACKAGE_BUILD_TIMEOUT_SECS >= 30 * 60);
        assert!(super::PACKAGE_BUILD_TIMEOUT_SECS > IMAGE_CHECK_TIMEOUT_SECS);
    }
}
