use std::path::Path;

use awaken_runtime_host::{
    AcpWorkerProfile, ContentCaptureSettings, ContentRedaction, PackageImageBuilder,
    SandboxSettings, SandboxTier, Wake,
};

use super::file_schema::FileConfig;

pub(super) struct RuntimeSettings {
    pub(super) sandbox_tier: SandboxTier,
    pub(super) sandbox: SandboxSettings,
    pub(super) acp: Option<AcpWorkerProfile>,
    pub(super) wake: Wake,
    pub(super) content_capture: ContentCaptureSettings,
}

pub(super) fn resolve(file: &FileConfig, data_dir: &Path) -> Result<RuntimeSettings, String> {
    let sandbox_tier = match file.sandbox_tier.as_deref() {
        Some("local" | "none") => SandboxTier::Local,
        Some("docker") => SandboxTier::Docker,
        Some("podman") => SandboxTier::Podman,
        Some("k8s" | "kubernetes") => SandboxTier::K8s,
        Some("namespace") | None => SandboxTier::Namespace,
        Some(other) => return Err(format!("invalid sandbox_tier={other:?}")),
    };
    let acp_ids = file.acp_clis.clone().unwrap_or_default();
    let acp = (!acp_ids.is_empty())
        .then(|| AcpWorkerProfile::new(acp_ids, file.acp_default_cli.clone()))
        .transpose()?;
    let wake = match file.dispatch_wake.as_deref() {
        Some("pg-notify") => Wake::PgNotify,
        Some("nats") => Wake::Nats,
        Some("none") | None => Wake::None,
        Some(other) => return Err(format!("invalid dispatch_wake={other:?}")),
    };
    let sandbox_defaults = SandboxSettings::default();
    let sandbox = SandboxSettings {
        allow_local_fallback: file.sandbox_allow_local_fallback.unwrap_or(false),
        warm_pool_size: file.sandbox_warm_pool_size.unwrap_or(0),
        container_forward_proxy: file.container_forward_proxy.clone(),
        k8s_namespace: file
            .k8s_namespace
            .clone()
            .unwrap_or_else(|| "default".to_owned()),
        k8s_image_pull_secrets: file
            .k8s_image_pull_secrets
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect(),
        container_hand_bin: file
            .container_hand_bin
            .clone()
            .unwrap_or_else(|| sandbox_defaults.container_hand_bin.clone()),
        podman_bin: file
            .podman_bin
            .clone()
            .unwrap_or_else(|| sandbox_defaults.podman_bin.clone()),
        package_image_registry: file
            .package_image_registry
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/').to_owned()),
        package_registry_auth_file: file.package_registry_auth_file.clone(),
        package_image_builder: file
            .package_image_builder
            .as_deref()
            .map(|value| match value {
                "docker" => Ok(PackageImageBuilder::Docker),
                "podman" => Ok(PackageImageBuilder::Podman),
                other => Err(format!(
                    "invalid package_image_builder={other:?}: expected docker or podman"
                )),
            })
            .transpose()?,
        package_artifact_dir: Some(
            file.package_artifact_dir
                .clone()
                .unwrap_or_else(|| data_dir.join("package-images")),
        ),
        package_build_lease_secs: file
            .package_build_lease_secs
            .unwrap_or(sandbox_defaults.package_build_lease_secs),
        package_build_wait_secs: file
            .package_build_wait_secs
            .unwrap_or(sandbox_defaults.package_build_wait_secs),
        package_failure_retry_secs: file
            .package_failure_retry_secs
            .unwrap_or(sandbox_defaults.package_failure_retry_secs),
        package_state_ttl_secs: file
            .package_state_ttl_secs
            .unwrap_or(sandbox_defaults.package_state_ttl_secs),
        package_local_cache_ttl_secs: file
            .package_local_cache_ttl_secs
            .unwrap_or(sandbox_defaults.package_local_cache_ttl_secs),
        inherit_agent_stderr: file.sandbox_inherit_agent_stderr.unwrap_or(false),
        reaper_enabled: file.sandbox_reaper_enabled.unwrap_or(true),
        reaper_interval_secs: file
            .sandbox_reaper_interval_secs
            .unwrap_or(sandbox_defaults.reaper_interval_secs),
        reaper_max_age_secs: file
            .sandbox_reaper_max_age_secs
            .unwrap_or(sandbox_defaults.reaper_max_age_secs),
    };
    if sandbox.k8s_namespace.trim().is_empty()
        || sandbox.container_hand_bin.trim().is_empty()
        || sandbox.podman_bin.trim().is_empty()
    {
        return Err(
            "k8s_namespace, container_hand_bin and podman_bin must not be empty".to_owned(),
        );
    }
    if sandbox.package_image_builder.is_some() && sandbox.package_image_registry.is_none() {
        return Err("package_image_builder requires a non-empty package_image_registry".to_owned());
    }
    if sandbox.package_registry_auth_file.is_some() && sandbox.package_image_registry.is_none() {
        return Err(
            "package_registry_auth_file requires a non-empty package_image_registry".to_owned(),
        );
    }
    if sandbox_tier == SandboxTier::K8s
        && (sandbox.package_image_builder.is_some() ^ sandbox.package_image_registry.is_some())
    {
        return Err(
            "Kubernetes package provisioning requires package_image_builder and package_image_registry together"
                .to_owned(),
        );
    }
    if sandbox.reaper_enabled
        && (sandbox.reaper_interval_secs == 0 || sandbox.reaper_max_age_secs == 0)
    {
        return Err("sandbox reaper interval and max age must be non-zero when enabled".to_owned());
    }
    if sandbox.package_build_lease_secs == 0
        || sandbox.package_build_wait_secs < sandbox.package_build_lease_secs
        || sandbox.package_failure_retry_secs == 0
        || sandbox.package_state_ttl_secs == 0
        || sandbox.package_local_cache_ttl_secs == 0
    {
        return Err(
            "package build lease/retry/TTL must be non-zero and wait must be at least the lease"
                .to_owned(),
        );
    }
    let content_capture = ContentCaptureSettings {
        level: match file.content_capture.as_deref() {
            Some("off") => awaken_runtime_contract::ContentCapture::Off,
            Some("structured") | None => awaken_runtime_contract::ContentCapture::Structured,
            Some("full") => awaken_runtime_contract::ContentCapture::Full,
            Some(other) => return Err(format!("invalid content_capture={other:?}")),
        },
        redaction: match file.content_redaction.as_deref() {
            Some("none") | None => ContentRedaction::None,
            Some("regex") => ContentRedaction::Regex,
            Some(other) => return Err(format!("invalid content_redaction={other:?}")),
        },
    };

    Ok(RuntimeSettings {
        sandbox_tier,
        sandbox,
        acp,
        wake,
        content_capture,
    })
}
