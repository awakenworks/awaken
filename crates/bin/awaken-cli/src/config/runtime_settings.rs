use std::path::Path;

use awaken_runtime_host::{
    AcpWorkerProfile, ContainerHandResidency, ContentCaptureSettings, ContentRedaction,
    PackageImageBuilder, SandboxSettings, SandboxTier, Wake,
};

use super::file_schema::FileConfig;

pub(super) struct RuntimeSettings {
    pub(super) sandbox_tier: SandboxTier,
    pub(super) sandbox: SandboxSettings,
    pub(super) acp: Option<AcpWorkerProfile>,
    pub(super) wake: Wake,
    pub(super) content_capture: ContentCaptureSettings,
}

pub(super) fn resolve(file: &FileConfig, _data_dir: &Path) -> Result<RuntimeSettings, String> {
    let sandbox_tier = file
        .sandbox_tier
        .as_deref()
        .unwrap_or("namespace")
        .parse::<SandboxTier>()?;
    let acp_ids = file.acp_clis.clone().unwrap_or_default();
    let acp = (!acp_ids.is_empty())
        .then(|| AcpWorkerProfile::new(acp_ids))
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
        warm_pool_total_size: file
            .sandbox_warm_pool_total_size
            .unwrap_or(sandbox_defaults.warm_pool_total_size),
        warm_pool_idle_ttl_secs: file
            .sandbox_warm_pool_idle_ttl_secs
            .unwrap_or(sandbox_defaults.warm_pool_idle_ttl_secs),
        container_forward_proxy: file.container_forward_proxy.clone(),
        // A no-bypass allowlist requires a deployment-mounted signing key and
        // an attested gateway; standalone configuration cannot synthesize it.
        container_allowlist_proxy: None,
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
        // Standalone CLI keeps ephemeral Kubernetes writable roots. Hosted
        // composition explicitly injects its retained-volume policy.
        k8s_continuation_volume: None,
        container_hand_bin: file
            .container_hand_bin
            .clone()
            .unwrap_or_else(|| sandbox_defaults.container_hand_bin.clone()),
        container_hand_residency: file
            .container_hand_residency
            .as_deref()
            .unwrap_or("attached_exec")
            .parse::<ContainerHandResidency>()?,
        container_hand_idle_secs: file
            .container_hand_idle_secs
            .unwrap_or(sandbox_defaults.container_hand_idle_secs),
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
        package_registry_insecure: file.package_registry_insecure.unwrap_or(false),
        k8s_buildkit_image: file
            .k8s_buildkit_image
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&sandbox_defaults.k8s_buildkit_image)
            .to_owned(),
        package_image_builder: file
            .package_image_builder
            .as_deref()
            .map(|value| match value {
                "docker" => Ok(PackageImageBuilder::Docker),
                "podman" => Ok(PackageImageBuilder::Podman),
                "k8s" => Ok(PackageImageBuilder::Kubernetes),
                other => Err(format!(
                    "invalid package_image_builder={other:?}: expected docker, podman or k8s"
                )),
            })
            .transpose()?,
        package_local_cache_ttl_secs: file
            .package_local_cache_ttl_secs
            .unwrap_or(sandbox_defaults.package_local_cache_ttl_secs),
        inherit_agent_stderr: file.sandbox_inherit_agent_stderr.unwrap_or(false),
    };
    if sandbox.container_hand_residency == ContainerHandResidency::Resident
        && sandbox_tier != SandboxTier::K8s
    {
        return Err("container_hand_residency=resident currently requires sandbox_tier=k8s".into());
    }
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
    if sandbox.package_registry_insecure && sandbox.package_image_registry.is_none() {
        return Err("package_registry_insecure requires a non-empty package_image_registry".into());
    }
    if sandbox_tier == SandboxTier::K8s
        && (sandbox.package_image_builder.is_some() ^ sandbox.package_image_registry.is_some())
    {
        return Err(
            "Kubernetes package provisioning requires package_image_builder and package_image_registry together"
                .to_owned(),
        );
    }
    if sandbox.package_local_cache_ttl_secs == 0 {
        return Err("package image local cache TTL must be non-zero".to_owned());
    }
    if sandbox.warm_pool_size > 0
        && (sandbox.warm_pool_total_size < sandbox.warm_pool_size
            || sandbox.warm_pool_idle_ttl_secs == 0)
    {
        return Err(
            "warm pool total size must cover one shape and idle TTL must be non-zero".to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_hand_configuration_fails_closed_outside_kubernetes() {
        /*
         * Configuration cause/effect table and FMECA control.
         * C1 residency omitted => AttachedExec/default; C2 resident+K8s =>
         * accept; C3 resident+non-K8s => reject; C4 unknown value => reject.
         * Effects: one explicit placement strategy reaches Host construction;
         * invalid/unsupported combinations create no environment. FMECA:
         * silently enabling Resident on a runtime without a private Pod channel
         * would strand Sessions (severity 4); parse-time rejection is the
         * preventive control and this table is its detection test.
         */
        let data = Path::new("/tmp/awaken-config-test");
        let defaults = resolve(&FileConfig::default(), data).unwrap();
        assert_eq!(
            defaults.sandbox.container_hand_residency,
            ContainerHandResidency::AttachedExec,
            "C1"
        );

        let k8s = resolve(
            &FileConfig {
                sandbox_tier: Some("k8s".into()),
                container_hand_residency: Some("resident".into()),
                ..Default::default()
            },
            data,
        )
        .unwrap();
        assert_eq!(
            k8s.sandbox.container_hand_residency,
            ContainerHandResidency::Resident,
            "C2"
        );

        for tier in ["namespace", "docker", "podman"] {
            let error = match resolve(
                &FileConfig {
                    sandbox_tier: Some(tier.into()),
                    container_hand_residency: Some("resident".into()),
                    ..Default::default()
                },
                data,
            ) {
                Ok(_) => panic!("C3 must reject resident Hand outside Kubernetes"),
                Err(error) => error,
            };
            assert!(
                error.contains("requires sandbox_tier=k8s"),
                "{tier}: {error}"
            );
        }

        let error = match resolve(
            &FileConfig {
                sandbox_tier: Some("k8s".into()),
                container_hand_residency: Some("sidecar".into()),
                ..Default::default()
            },
            data,
        ) {
            Ok(_) => panic!("C4 must reject an unknown residency"),
            Err(error) => error,
        };
        assert!(error.contains("expected attached_exec or resident"));
    }
}
