// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The existing package adapter renders one credential-isolated
// @formal reason: Kubernetes Job; this classifies the surface and makes no proof claim.
//! One serial Kaniko Job: trusted prefetch, credential-free RUN, trusted push.
use super::K8sPackageImageProvisioner;
use crate::package_realization::{
    KANIKO_BUILD_CONTAINER, KANIKO_PREFETCH_CONTAINER, KANIKO_PUSH_CONTAINER,
};
use awaken_provisioning_contract as pc;
use awaken_worker_fleet_kubernetes::pod_resources::{byte_quantity, pod_resources};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, KeyToPath,
    LocalObjectReference, PodSecurityContext, PodSpec, SeccompProfile, SecretVolumeSource,
    SecurityContext, Volume, VolumeMount,
};

const BUILD_SCRIPT: &str = r#"
set -eu
/busybox/mkdir -p /workspace
/busybox/cp -L /input/Dockerfile /workspace/Dockerfile
exec /kaniko/executor "$@"
"#;

// This log belongs only to the trusted push process. RUN output is in a
// different init container, and cannot write this private receipt volume.
pub(super) const PUSH_SCRIPT: &str = r#"
set -eu
test -f /output/image.tar
test ! -L /output/image.tar
if /kaniko/executor push /output/image.tar "$@" > /receipt/push.log 2>&1; then
  /busybox/cat /receipt/push.log
else
  code=$?
  /busybox/cat /receipt/push.log
  exit "$code"
fi
image=$(/busybox/sed -n 's/.*Pushed \([^[:space:]]*@sha256:[0-9a-f]\{64\}\).*/\1/p' /receipt/push.log)
test -n "$image"
test "$(printf '%s\n' "$image" | /busybox/wc -l)" -eq 1
case "$image" in "${DESTINATION%:*}@sha256:"*) ;; *) exit 1 ;; esac
printf '%s' "$image" > /dev/termination-log
"#;

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.into(),
        mount_path: path.into(),
        read_only: Some(read_only),
        ..Default::default()
    }
}
fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}
fn security(trusted: bool) -> SecurityContext {
    SecurityContext {
        privileged: Some(false),
        allow_privilege_escalation: Some(false),
        run_as_non_root: Some(trusted),
        run_as_user: Some(if trusted { 10001 } else { 0 }),
        run_as_group: Some(if trusted { 10001 } else { 0 }),
        read_only_root_filesystem: Some(trusted),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".into(),
            ..Default::default()
        }),
        capabilities: trusted.then(|| Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(super) fn build_pod(
    p: &K8sPackageImageProvisioner,
    base: &str,
    input: &str,
    destination: &str,
    host: &str,
) -> PodSpec {
    let budget = pod_resources(
        &pc::ResourceRequests {
            cpu_millis: Some(100),
            memory_bytes: Some(256 * 1024 * 1024),
            disk_bytes: Some(1024 * 1024 * 1024),
        },
        &pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(2 * 1024 * 1024 * 1024),
            disk_bytes: Some(8 * 1024 * 1024 * 1024),
            pids: None,
        },
    );
    let mut volumes = vec![Volume {
        name: "build-input".into(),
        config_map: Some(ConfigMapVolumeSource {
            name: input.into(),
            ..Default::default()
        }),
        ..Default::default()
    }];
    for (name, size) in [
        ("build-context", 1024 * 1024_u64),
        ("base-cache", 4 * 1024 * 1024 * 1024),
        ("build-output", 4 * 1024 * 1024 * 1024),
        ("push-receipt", 1024 * 1024),
    ] {
        volumes.push(Volume {
            name: name.into(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(byte_quantity(size)),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let mut auth = Vec::new();
    if let Some(secret) = &p.push_secret {
        volumes.push(Volume {
            name: "registry-auth".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.clone()),
                items: Some(vec![KeyToPath {
                    key: ".dockerconfigjson".into(),
                    path: "config.json".into(),
                    mode: Some(0o440),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
        auth.push(mount("registry-auth", "/kaniko/.docker", true));
    }
    let mut common_env = Vec::new();
    let mut build_args = vec![
        BUILD_SCRIPT.into(),
        "--".into(),
        "--context=dir:///workspace".into(),
        "--dockerfile=/workspace/Dockerfile".into(),
        format!("--destination={destination}"),
        "--no-push".into(),
        "--no-push-cache".into(),
        "--tar-path=/output/image.tar".into(),
        "--snapshot-mode=full".into(),
        "--cache=true".into(),
        "--cache-dir=/base-cache".into(),
        "--cache-run-layers=false".into(),
        "--cache-copy-layers=false".into(),
        "--credential-helpers=".into(),
    ];
    if let Some(proxy) = &p.forward_proxy {
        let bypass = format!("localhost,127.0.0.1,{host},.svc,.cluster.local");
        for (names, value) in [
            (
                &["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"][..],
                proxy.url.as_str(),
            ),
            (&["NO_PROXY", "no_proxy"][..], bypass.as_str()),
        ] {
            for name in names {
                common_env.push(env(name, value));
                build_args.push(format!("--build-arg={name}={value}"));
            }
        }
    }
    let mut prefetch_args = vec![
        format!("--image={base}"),
        "--cache-dir=/base-cache".into(),
        "--require-digest".into(),
        "--log-format=text".into(),
    ];
    let mut push_args = vec![
        PUSH_SCRIPT.into(),
        "--".into(),
        format!("--destination={destination}"),
        "--push-retry=1".into(),
        "--credential-helpers=".into(),
    ];
    if p.registry_insecure {
        let flag = format!("--insecure-registry={host}");
        prefetch_args.push(flag.clone());
        build_args.push(flag.clone());
        push_args.push(flag);
    }
    let container = |name: &str,
                     command: Vec<String>,
                     args: Vec<String>,
                     mounts: Vec<VolumeMount>,
                     environment: Vec<EnvVar>,
                     trusted| Container {
        name: name.into(),
        image: Some(p.kaniko_image.clone()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(command),
        args: Some(args),
        env: Some(environment),
        volume_mounts: Some(mounts),
        resources: budget.clone(),
        security_context: Some(security(trusted)),
        ..Default::default()
    };
    let mut trusted_env = common_env.clone();
    trusted_env.push(env("DOCKER_CONFIG", "/kaniko/.docker"));
    let mut prefetch_mounts = auth.clone();
    prefetch_mounts.push(mount("base-cache", "/base-cache", false));
    let prefetch = container(
        KANIKO_PREFETCH_CONTAINER,
        vec!["/kaniko/warmer".into()],
        prefetch_args,
        prefetch_mounts,
        trusted_env.clone(),
        true,
    );
    let mut build_env = common_env;
    build_env.push(env("DOCKER_CONFIG", "/kaniko/no-auth"));
    let build = container(
        KANIKO_BUILD_CONTAINER,
        vec!["/busybox/sh".into(), "-ceu".into()],
        build_args,
        vec![
            mount("build-input", "/input", true),
            mount("build-context", "/workspace", false),
            mount("base-cache", "/base-cache", true),
            mount("build-output", "/output", false),
        ],
        build_env,
        false,
    );
    let mut push_mounts = auth;
    push_mounts.extend([
        mount("build-output", "/output", true),
        mount("push-receipt", "/receipt", false),
    ]);
    trusted_env.push(env("DESTINATION", destination));
    let push = container(
        KANIKO_PUSH_CONTAINER,
        vec!["/busybox/sh".into(), "-ceu".into()],
        push_args,
        push_mounts,
        trusted_env,
        true,
    );
    PodSpec {
        automount_service_account_token: Some(false),
        share_process_namespace: Some(false),
        restart_policy: Some("Never".into()),
        init_containers: Some(vec![prefetch, build]),
        containers: vec![push],
        volumes: Some(volumes),
        security_context: Some(PodSecurityContext {
            fs_group: Some(10001),
            fs_group_change_policy: Some("OnRootMismatch".into()),
            ..Default::default()
        }),
        image_pull_secrets: Some(
            p.image_pull_secrets
                .iter()
                .cloned()
                .map(|name| LocalObjectReference { name })
                .collect(),
        ),
        ..Default::default()
    }
}
