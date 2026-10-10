// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This source implements the repository-controlled remote worker product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Kubernetes-native realization of immutable Environment package images.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, ConfigMap, ConfigMapVolumeSource, Container, EnvVar, KeyToPath,
    LocalObjectReference, Pod, PodSecurityContext, PodSpec, PodTemplateSpec, SecretVolumeSource,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Preconditions};
use kube::{Api, Client};

use crate::k8s::{
    K8sCreateTransport, backend, brokered_client, create_or_verify_exact,
    install_rustls_crypto_provider, sandbox_network_labels, stamp_realization,
};
use crate::k8s_package_realization::{
    PACKAGE_BUILD_JOB_KIND, PACKAGE_IMAGE_CHECK_JOB_KIND, bind_package_job_to_config,
    exact_package_job_pods, package_build_name, package_config_annotations,
    package_image_check_name, stamp_package_config_map, verified_package_build_result,
    verified_package_image_check_result, verify_exact_package_config_map, verify_exact_package_job,
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
// A completed kubelet check is the exact, API-observed availability proof for
// this immutable image. Retain it across ordinary Session creation instead of
// scheduling another pull one minute after every successful check.
const IMAGE_CHECK_SUCCESS_REUSE_SECS: i32 = 60 * 60;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobPodSnapshotDisposition {
    Pending,
    Verify,
    InvalidCardinality,
}

fn job_pod_snapshot_disposition(
    same_incarnation: bool,
    exact_pod_count: usize,
) -> JobPodSnapshotDisposition {
    if !same_incarnation || exact_pod_count == 0 {
        JobPodSnapshotDisposition::Pending
    } else if exact_pod_count == 1 {
        JobPodSnapshotDisposition::Verify
    } else {
        JobPodSnapshotDisposition::InvalidCardinality
    }
}

fn same_job_incarnation(before: &Job, after: Option<&Job>) -> bool {
    let before_uid = before.metadata.uid.as_deref();
    before_uid.is_some() && before_uid == after.and_then(|job| job.metadata.uid.as_deref())
}

fn exact_owner_pod_count(job: &Job, pods: &[Pod]) -> usize {
    let Some(job_uid) = job.metadata.uid.as_deref() else {
        return 0;
    };
    pods.iter()
        .filter(|pod| {
            pod.metadata.deletion_timestamp.is_none()
                && pod
                    .metadata
                    .owner_references
                    .as_deref()
                    .is_some_and(|owners| {
                        owners.len() == 1
                            && owners[0].api_version == "batch/v1"
                            && owners[0].kind == "Job"
                            && owners[0].uid == job_uid
                            && owners[0].controller == Some(true)
                    })
        })
        .count()
}

fn exact_job_delete_params(job: &Job) -> Result<DeleteParams, RuntimeError> {
    let uid = job
        .metadata
        .uid
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| backend("package Job has no UID for exact deletion"))?;
    let resource_version = job
        .metadata
        .resource_version
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| backend("package Job has no resourceVersion for exact deletion"))?;
    Ok(DeleteParams::background().preconditions(Preconditions {
        uid: Some(uid),
        resource_version: Some(resource_version),
    }))
}

async fn delete_exact_job(jobs: &Api<Job>, job: &Job) -> Result<(), RuntimeError> {
    let name = job
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| backend("package Job has no name for exact deletion"))?;
    match jobs.delete(name, &exact_job_delete_params(job)?).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 || response.code == 409 => Ok(()),
        Err(error) => Err(backend(error)),
    }
}

fn image_check_observation(job: Option<Job>) -> ImageCheckObservation {
    match job {
        Some(job) => ImageCheckObservation::Present(job.status.unwrap_or_default()),
        None => ImageCheckObservation::Missing,
    }
}

fn immutable_registry_identity(identity: Option<String>) -> Option<String> {
    identity.filter(|reference| crate::k8s_package_realization::immutable_image(reference).is_ok())
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
        Self::connect_with_create_transport(
            namespace.into(),
            registry.into(),
            image_pull_secrets,
            registry_insecure,
            None,
        )
        .await
    }

    /// Worker-owned package/image-check Jobs use the same signed Create port
    /// as Session Pods. The Coordinator's separate build-loop adapter calls
    /// `connect` and retains its existing durable build-row authority.
    pub async fn connect_with_create_transport(
        namespace: impl Into<String>,
        registry: impl Into<String>,
        image_pull_secrets: Vec<String>,
        registry_insecure: bool,
        transport: Option<Arc<dyn K8sCreateTransport>>,
    ) -> Result<Self, RuntimeError> {
        install_rustls_crypto_provider();
        let client = Client::try_default().await.map_err(backend)?;
        let client = match transport {
            Some(broker) => brokered_client(client, broker),
            None => client,
        };
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
                        mode: Some(0o440),
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
                        security_context: Some(PodSecurityContext {
                            // Secret volumes are root-owned. Give only the
                            // rootless BuildKit primary group read access; no
                            // service-account token or world-readable copy is
                            // introduced.
                            fs_group: Some(1000),
                            fs_group_change_policy: Some("OnRootMismatch".into()),
                            ..Default::default()
                        }),
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
        let deadline = awaken_time::mono::MonoDeadline::after(std::time::Duration::from_secs(
            PACKAGE_BUILD_TIMEOUT_SECS as u64 + 30,
        ));
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
            if deadline.has_passed() {
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
                ttl_seconds_after_finished: Some(IMAGE_CHECK_SUCCESS_REUSE_SECS),
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
        let deadline = awaken_time::mono::MonoDeadline::after(image_check_client_timeout());
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
                deadline.has_passed(),
            ) {
                ImageCheckDisposition::Available => {
                    let observed_job = observed.as_ref().expect("present image-check observation");
                    let exact_package_pods = if package {
                        Some(
                            exact_package_job_pods(observed_job, &listed.items, &self.namespace)
                                .map_err(|error| backend(error.to_string()))?,
                        )
                    } else {
                        None
                    };
                    let exact_pod_count = exact_package_pods.as_ref().map_or_else(
                        || exact_owner_pod_count(observed_job, &listed.items),
                        Vec::len,
                    );
                    let refreshed = jobs.get_opt(&name).await.map_err(backend)?;
                    match job_pod_snapshot_disposition(
                        same_job_incarnation(observed_job, refreshed.as_ref()),
                        exact_pod_count,
                    ) {
                        JobPodSnapshotDisposition::Pending => {
                            // GET Job and LIST Pods are not one Kubernetes snapshot.
                            // TTL collection or same-name recreation may occur
                            // between them. Absence cannot become either success or
                            // permanent failure; observe the current incarnation.
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            continue;
                        }
                        JobPodSnapshotDisposition::Verify => {}
                        JobPodSnapshotDisposition::InvalidCardinality => {
                            return Err(backend(format!(
                                "Kubernetes image check `{name}` has multiple Pods for one Job incarnation"
                            )));
                        }
                    }
                    let identity = if package {
                        Some(verified_package_image_check_result(
                            observed_job,
                            exact_package_pods
                                .as_deref()
                                .expect("package image-check exact Pod set"),
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
                    if let Some(observed) = observed.as_ref() {
                        delete_exact_job(&jobs, observed).await?;
                    }
                    return Ok(None);
                }
                ImageCheckDisposition::Unavailable(reason) => {
                    if let Some(observed) = observed.as_ref() {
                        delete_exact_job(&jobs, observed).await?;
                    }
                    return Err(backend(format!("Kubernetes image check `{name}` {reason}")));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[async_trait]
impl PackageImageProvisioner for K8sPackageImageProvisioner {
    /// The BuildKit Jobs this builder creates live in its namespace; reading
    /// one page proves the API answers and the identity may list them.
    async fn probe_builder(&self) -> Result<(), RuntimeError> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        jobs.list(&ListParams::default().limit(1))
            .await
            .map(|_| ())
            .map_err(crate::runtime::RuntimeFailure::into_runtime_error)
    }

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
    use std::sync::{Arc, Mutex};

    use super::{
        IMAGE_CHECK_TIMEOUT_SECS, ImageCheckDisposition, ImageCheckObservation,
        JobPodSnapshotDisposition, K8sPackageImageProvisioner, exact_job_delete_params,
        exact_owner_pod_count, image_check_client_timeout, image_check_disposition,
        image_check_identity, image_check_observation, immutable_registry_identity,
        job_pod_snapshot_disposition, package_image_matches_stored_identity, same_job_incarnation,
        terminal_image_pull_reason,
    };
    use crate::k8s_package_realization::{
        K8S_PACKAGE_REALIZATION_CONTRACT_VERSION, PACKAGE_BUILD_JOB_KIND,
        PACKAGE_CONFIG_MAP_UID_ANNOTATION, PACKAGE_IMAGE_CHECK_JOB_KIND,
        PACKAGE_IMAGE_DESTINATION_ANNOTATION, PACKAGE_JOB_KIND_ANNOTATION,
        PACKAGE_REALIZATION_CONTRACT_ANNOTATION, PACKAGE_RECIPE_FINGERPRINT_ANNOTATION,
    };
    use k8s_openapi::api::batch::v1::Job;
    use k8s_openapi::api::core::v1::Pod;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
    use kube::client::Body;

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

    fn observed_job(uid: &str, resource_version: &str) -> Job {
        Job {
            metadata: ObjectMeta {
                name: Some("shared-check".into()),
                uid: Some(uid.into()),
                resource_version: Some(resource_version.into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn owned_pod(job_uid: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                owner_references: Some(vec![OwnerReference {
                    api_version: "batch/v1".into(),
                    kind: "Job".into(),
                    name: "shared-check".into(),
                    uid: job_uid.into(),
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                }]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn job_pod_snapshot_decision_table_retries_incoherent_reads() {
        /* Causes: C1 Job incarnation changes between GET and re-GET; C2 the
         * same Job has zero exact Pods during controller creation or TTL
         * deletion; C3 it has one exact Pod; C4 it has multiple exact Pods.
         * Effects: E1 retry without success/failure; E2 verify the exact pair;
         * E3 reject ambiguous evidence. Rules R1=C1=>E1; R2=!C1+C2=>E1;
         * R3=!C1+C3=>E2; R4=!C1+C4=>E3. */
        assert_eq!(
            job_pod_snapshot_disposition(false, 1),
            JobPodSnapshotDisposition::Pending,
            "R1"
        );
        assert_eq!(
            job_pod_snapshot_disposition(true, 0),
            JobPodSnapshotDisposition::Pending,
            "R2"
        );
        assert_eq!(
            job_pod_snapshot_disposition(true, 1),
            JobPodSnapshotDisposition::Verify,
            "R3"
        );
        assert_eq!(
            job_pod_snapshot_disposition(true, 2),
            JobPodSnapshotDisposition::InvalidCardinality,
            "R4"
        );
    }

    #[test]
    fn exact_job_identity_ignores_same_name_pods_from_another_incarnation() {
        /* Causes: C1 current Job UID A and Pod owner A; C2 same-name Pod owner
         * B; C3 re-read Job remains A or becomes B. Effects: E1 count only A;
         * E2 reject cross-incarnation snapshots. */
        let job_a = observed_job("uid-a", "1");
        let job_b = observed_job("uid-b", "2");
        assert_eq!(exact_owner_pod_count(&job_a, &[owned_pod("uid-a")]), 1);
        assert_eq!(exact_owner_pod_count(&job_a, &[owned_pod("uid-b")]), 0);
        assert!(same_job_incarnation(&job_a, Some(&job_a)));
        assert!(!same_job_incarnation(&job_a, Some(&job_b)));
        assert!(!same_job_incarnation(&job_a, None));
    }

    #[test]
    fn package_job_cleanup_is_fenced_by_uid_and_resource_version() {
        /* Causes: C1 an exact observed Job has UID and resourceVersion; C2
         * either coordinate is absent. Effects: E1 emit both Kubernetes delete
         * preconditions; E2 refuse to express an unfenced deletion. */
        let params = exact_job_delete_params(&observed_job("uid-a", "7")).unwrap();
        let preconditions = params.preconditions.expect("exact delete preconditions");
        assert_eq!(preconditions.uid.as_deref(), Some("uid-a"));
        assert_eq!(preconditions.resource_version.as_deref(), Some("7"));
        assert!(exact_job_delete_params(&observed_job("", "7")).is_err());
        assert!(exact_job_delete_params(&observed_job("uid-a", "")).is_err());
    }

    #[tokio::test]
    async fn image_check_retries_zero_pod_snapshot_across_same_name_recreation() {
        /* End-to-end observation rule. Causes: C1 create/reuse returns Job A;
         * C2 GET observes A succeeded; C3 LIST races TTL and returns zero Pods;
         * C4 re-GET observes same-name Job B; C5 the next observation returns
         * B plus its exact successful Pod. Effects: E1 do not return the former
         * invalid-evidence 503 at C3; E2 never combine A with B's Pod; E3
         * return B's immutable kubelet digest. */
        let image = format!("registry.local/base@sha256:{}", "a".repeat(64));
        let builder = test_builder();
        let (_, desired) = builder.image_check_job(&image, None).unwrap();
        let mut job_a = desired.clone();
        job_a.metadata.uid = Some("job-a".into());
        job_a.metadata.resource_version = Some("1".into());
        job_a.status = Some(k8s_openapi::api::batch::v1::JobStatus {
            succeeded: Some(1),
            failed: Some(0),
            active: Some(0),
            ..Default::default()
        });
        let mut job_b = job_a.clone();
        job_b.metadata.uid = Some("job-b".into());
        job_b.metadata.resource_version = Some("2".into());
        let mut pod_b = owned_pod("job-b");
        pod_b.metadata.name = Some("shared-check-pod-b".into());
        pod_b.status = Some(k8s_openapi::api::core::v1::PodStatus {
            container_statuses: Some(vec![k8s_openapi::api::core::v1::ContainerStatus {
                name: "verify".into(),
                image: image.clone(),
                image_id: format!("containerd://{image}"),
                ready: false,
                restart_count: 0,
                started: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        });

        let calls = Arc::new(Mutex::new(0usize));
        let service = tower::service_fn({
            let calls = Arc::clone(&calls);
            move |_request: http::Request<Body>| {
                let call = {
                    let mut calls = calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                let value = match call {
                    1 | 2 => serde_json::to_value(&job_a).unwrap(),
                    3 => {
                        serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[]})
                    }
                    4 | 5 | 7 => serde_json::to_value(&job_b).unwrap(),
                    6 => {
                        serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[pod_b]})
                    }
                    other => panic!("unexpected Kubernetes request {other}"),
                };
                async move {
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(200)
                            .body(Body::from(serde_json::to_vec(&value).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        let client = kube::Client::new(service, "default");
        let provisioner = K8sPackageImageProvisioner::new(
            client,
            "awaken-system",
            "registry.local:5000/environments",
            Vec::new(),
            true,
        )
        .unwrap();
        assert_eq!(
            provisioner
                .resolve_image_identity(&image, None)
                .await
                .unwrap(),
            Some(image),
            "C1-C5/E1-E3"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            7,
            "the incoherent snapshot must be retried once"
        );
    }

    #[tokio::test]
    async fn package_build_objects_expose_only_secret_free_release_correlation() {
        /* Release-correlation cause/effect table — R1:
         * C1 one deterministic recipe is pushed to one generated destination;
         * C2 the same Job carries private recipe, proxy, Registry auth, and base
         * image inputs; C3 package realization has already admitted only an
         * unrestricted build; C4 rootless BuildKit must read the projected
         * Registry config without running as root. Effects: E1 Job and Pod template expose the exact
         * recipe fingerprint and destination; E2 no C2 material enters
         * annotations; E3 both objects select the canonical sandbox default-deny
         * plus open-egress policies; E4 the Pod gives GID 1000 read-only access
         * to the 0440 Secret file. Rule R1=C1+C2+C3+C4=>E1+E2+E3+E4. The termination
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
        let pod_spec = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        let pod_security = pod_spec.security_context.as_ref().unwrap();
        assert_eq!(pod_security.fs_group, Some(1000), "R1/E4");
        assert_eq!(
            pod_security.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch"),
            "R1/E4"
        );
        let credential_mode = pod_spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|volume| volume.name == "registry-auth")
            .and_then(|volume| volume.secret.as_ref())
            .and_then(|secret| secret.items.as_ref())
            .and_then(|items| items.first())
            .and_then(|item| item.mode);
        assert_eq!(credential_mode, Some(0o440), "R1/E4");

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

    #[tokio::test]
    async fn successful_image_checks_outlive_an_ordinary_session_creation_window() {
        // Causes: C1 a generic base-image check succeeds; C2 a package-image
        // check succeeds; C3 the Job is pending, failed, or has passed its TTL.
        // Effects: E1 both exact successful Job/Pod observations remain reusable
        // for an hour across Sessions; E2 the existing lifecycle still handles
        // pending, failure, and TTL expiry without another cache. Decision rules:
        // R1=C1=>E1, R2=C2=>E1, R3=C3=>E2. The retention exceeds the bounded
        // probe window, so a normal later Session does not schedule a new pull.
        let builder = test_builder();
        let base = "registry.local/base@sha256:exact";
        let (mut config, _, destination) = builder
            .build_objects(base, &package_requirements())
            .unwrap();
        config.metadata.uid = Some("config-uid".into());
        for (image, package_config) in [(base, None), (destination.as_str(), Some(&config))] {
            let (_, job) = builder.image_check_job(image, package_config).unwrap();
            let spec = job.spec.unwrap();
            assert_eq!(spec.ttl_seconds_after_finished, Some(60 * 60), "R1/R2:E1");
            assert!(
                i64::from(spec.ttl_seconds_after_finished.unwrap())
                    > spec.active_deadline_seconds.unwrap(),
                "R1/R2:E1 proof survives the bounded pull window"
            );
        }
        assert_eq!(
            image_check_disposition(false, false, false, false),
            ImageCheckDisposition::Continue,
            "R3:E2 pending check is not proof"
        );
        assert!(
            matches!(
                image_check_disposition(false, false, true, false),
                ImageCheckDisposition::Unavailable(_)
            ),
            "R3:E2 failed check is not proof"
        );
        assert!(
            matches!(
                image_check_observation(None),
                ImageCheckObservation::Missing
            ),
            "R3:E2 TTL expiry returns to the same probe path"
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
    fn immutable_identity_uses_the_shared_strict_realization_parser() {
        // IP1 canonical digest => accepted by the existing realization owner;
        // IP2 absent repository, extra @, uppercase digest or control bytes =>
        // no reusable identity. Both image availability and node preparation
        // use this owner; no looser parallel digest recognition remains.
        let digest = "a".repeat(64);
        for reference in [
            format!("@sha256:{digest}"),
            format!("repo@other@sha256:{digest}"),
            format!("repo@sha256:{}", "A".repeat(64)),
            format!("repo\n@sha256:{digest}"),
        ] {
            assert!(immutable_registry_identity(Some(reference)).is_none());
        }
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
