// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Projects parsed deployment settings into the Runtime configuration. Execution composition owns the checkpoint store and Session continuation owns its use.
use std::path::Path;

use awaken_acp_contract::AcpWorkerProfile;
use awaken_deployment_config::Wake;
use awaken_deployment_config::{
    ContentCaptureSettings, ContentRedaction, SandboxPackageSettingsInput, SandboxSettings,
    SandboxTier,
};
use awaken_session_environment_contract::ContainerHandResidency;
use awaken_session_environment_contract::ContainerMcpRelayExposure;

use super::file_schema::FileConfig;

pub(super) struct RuntimeSettings {
    pub(super) sandbox_tier: SandboxTier,
    pub(super) sandbox: SandboxSettings,
    pub(super) acp: Option<AcpWorkerProfile>,
    pub(super) wake: Wake,
    pub(super) content_capture: ContentCaptureSettings,
}

fn default_sandbox_tier(target_os: &str) -> SandboxTier {
    // Windows has no NamespaceProvider adapter. Selecting Local here is an
    // explicit platform capability decision at the one configuration authority,
    // not a runtime fallback from an isolation request. Linux and macOS retain
    // the fail-closed OS-native isolation default.
    if target_os == "windows" {
        SandboxTier::Local
    } else {
        SandboxTier::Namespace
    }
}

pub(super) fn resolve(file: &FileConfig, _data_dir: &Path) -> Result<RuntimeSettings, String> {
    let sandbox_tier = file
        .sandbox_tier
        .as_deref()
        .map(str::parse::<SandboxTier>)
        .transpose()?
        .unwrap_or_else(|| default_sandbox_tier(std::env::consts::OS));
    let acp_ids = file.acp_clis.clone().unwrap_or_default();
    let acp = (!acp_ids.is_empty())
        .then(|| AcpWorkerProfile::new(acp_ids, awaken_acp_catalog::acp_adapter_policy()))
        .transpose()?;
    let wake = match file.dispatch_wake.as_deref() {
        Some("pg-notify") => Wake::PgNotify,
        Some("nats") => Wake::Nats,
        Some("none") | None => Wake::None,
        Some(other) => return Err(format!("invalid dispatch_wake={other:?}")),
    };
    let sandbox_defaults = SandboxSettings::default();
    let container_mcp_relay = match (
        file.container_mcp_relay_bind.as_deref(),
        file.container_mcp_relay_url.as_deref(),
    ) {
        (None, None) => None,
        (Some(bind), Some(url)) => Some(ContainerMcpRelayExposure::new(
            bind.parse()
                .map_err(|error| format!("invalid container_mcp_relay_bind={bind:?}: {error}"))?,
            url,
        )?),
        _ => {
            return Err(
                "container_mcp_relay_bind and container_mcp_relay_url must be configured together"
                    .into(),
            );
        }
    };
    let mut sandbox = SandboxSettings {
        checkpoint_s3: file.sandbox_checkpoint_s3.clone(),
        trusted_host_enabled: file.sandbox_trusted_host_enabled.unwrap_or(false),
        trusted_host_scoped_execution: file.sandbox_trusted_host_scoped_execution.unwrap_or(false),
        allow_local_fallback: file.sandbox_allow_local_fallback.unwrap_or(false),
        warm_pool_size: file.sandbox_warm_pool_size.unwrap_or(0),
        warm_pool_total_size: file
            .sandbox_warm_pool_total_size
            .unwrap_or(sandbox_defaults.warm_pool_total_size),
        warm_pool_idle_ttl_secs: file
            .sandbox_warm_pool_idle_ttl_secs
            .unwrap_or(sandbox_defaults.warm_pool_idle_ttl_secs),
        container_forward_proxy: file.container_forward_proxy.clone(),
        container_mcp_relay,
        k8s_namespace: file
            .k8s_namespace
            .clone()
            .unwrap_or_else(|| "default".to_owned()),
        k8s_unschedulable_wait_secs: sandbox_defaults.k8s_unschedulable_wait_secs,
        k8s_image_pull_secrets: Vec::new(),
        // The operator selects the existing retained-PVC policy. Omission
        // preserves ephemeral roots and cannot advertise retained Session support.
        k8s_continuation_volume: file.k8s_continuation_volume.clone(),
        k8s_image_prewarm: file.k8s_image_prewarm.clone(),
        k8s_pod_bounds: file.k8s_pod_bounds.unwrap_or_default(),
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
        package_image_registry: None,
        package_registry_auth_file: None,
        package_registry_insecure: false,
        package_build_host_network: false,
        k8s_kaniko_image: sandbox_defaults.k8s_kaniko_image.clone(),
        k8s_package_push_secret: None,
        package_image_builder: None,
        package_local_cache_ttl_secs: sandbox_defaults.package_local_cache_ttl_secs,
        inherit_agent_stderr: false,
    };
    SandboxPackageSettingsInput {
        k8s_image_pull_secrets: file.k8s_image_pull_secrets.clone().unwrap_or_default(),
        package_image_registry: file.package_image_registry.clone(),
        package_registry_auth_file: file.package_registry_auth_file.clone(),
        package_registry_insecure: file.package_registry_insecure.unwrap_or(false),
        package_build_host_network: file.package_build_host_network.unwrap_or(false),
        k8s_kaniko_image: file.k8s_kaniko_image.clone(),
        k8s_package_push_secret: file.k8s_package_push_secret.clone(),
        package_image_builder: file.package_image_builder.clone(),
        package_local_cache_ttl_secs: file.package_local_cache_ttl_secs,
        inherit_agent_stderr: file.sandbox_inherit_agent_stderr.unwrap_or(false),
    }
    .apply(sandbox_tier, &mut sandbox)?;
    sandbox.apply_k8s_unschedulable_wait_secs(sandbox_tier, file.k8s_unschedulable_wait_secs)?;
    if file.k8s_pod_bounds.is_some() && sandbox_tier != SandboxTier::K8s {
        return Err("k8s_pod_bounds requires sandbox_tier=k8s".into());
    }
    sandbox.k8s_pod_bounds.validate()?;
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
    if let Some(policy) = sandbox.k8s_continuation_volume.as_ref() {
        policy.validate().map_err(|error| error.to_string())?;
    }
    if let Some(policy) = &sandbox.k8s_image_prewarm {
        policy.validate()?;
        if sandbox_tier != SandboxTier::K8s {
            return Err("k8s_image_prewarm requires sandbox_tier=k8s".into());
        }
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
    fn sandbox_checkpoint_s3_file_configuration_is_opt_in() {
        let data = Path::new("/tmp/awaken-config-test");
        assert!(
            resolve(&FileConfig::default(), data)
                .unwrap()
                .sandbox
                .checkpoint_s3
                .is_none()
        );

        let file: FileConfig = toml::from_str(
            r#"
            sandbox_tier = "local"

            [sandbox_checkpoint_s3]
            bucket = "checkpoint-bucket"
            region = "ap-southeast-1"
            prefix = "awaken/checkpoints"
            "#,
        )
        .unwrap();
        let runtime = resolve(&file, data).unwrap();
        let archive = runtime.sandbox.checkpoint_s3.unwrap();
        assert_eq!(archive.bucket, "checkpoint-bucket");
        assert_eq!(archive.region, "ap-southeast-1");
        assert_eq!(archive.prefix, "awaken/checkpoints");
    }

    #[test]
    fn container_mcp_relay_requires_one_externally_reachable_origin_pair() {
        // Cause/effect table: R1 neither field => no exposure; R2 both fields =>
        // delegate semantic validation to ContainerMcpRelayExposure; R3 exactly
        // one field => reject before Host construction. Constructor tests own
        // origin, address, and credential-free semantics so CLI parsing does not
        // become a parallel policy implementation.
        let data = Path::new("/tmp/awaken-config-test");
        let absent = resolve(&FileConfig::default(), data).unwrap();
        assert!(absent.sandbox.container_mcp_relay.is_none(), "E1");

        let admitted = resolve(
            &FileConfig {
                container_mcp_relay_bind: Some("0.0.0.0:8082".into()),
                container_mcp_relay_url: Some("http://awaken-worker.awaken-cloud.svc:8082".into()),
                ..Default::default()
            },
            data,
        )
        .unwrap();
        assert!(admitted.sandbox.container_mcp_relay.is_some(), "E2");

        for (bind, url) in [
            (Some("0.0.0.0:8082"), None),
            (None, Some("http://relay.example.test:8082")),
        ] {
            let result = resolve(
                &FileConfig {
                    container_mcp_relay_bind: bind.map(str::to_owned),
                    container_mcp_relay_url: url.map(str::to_owned),
                    ..Default::default()
                },
                data,
            );
            assert!(result.is_err(), "E3 bind={bind:?} url={url:?}");
        }
    }

    #[test]
    fn scoped_execution_file_selection_reaches_the_shared_admission_rule() {
        // C1 omitted/false -> unchanged; C2 true+Local or explicit host -> Linux
        // only; C3 true+isolated primary without host consent -> reject. Both
        // file projection and complete CLI resolution consume the same typed
        // DeploymentConfig validator, never another platform allowlist.
        let home = tempfile::tempdir().unwrap();
        for enabled in [None, Some(false), Some(true)] {
            for (tier, trusted) in [("local", false), ("namespace", false), ("namespace", true)] {
                let file = FileConfig {
                    sandbox_tier: Some(tier.into()),
                    sandbox_trusted_host_enabled: Some(trusted),
                    sandbox_trusted_host_scoped_execution: enabled,
                    ..Default::default()
                };
                let projected = resolve(&file, Path::new("/tmp/config-selection")).unwrap();
                assert_eq!(
                    projected.sandbox.trusted_host_scoped_execution,
                    enabled.unwrap_or(false)
                );
                let complete = crate::config::resolve_test_file(
                    Default::default(),
                    Some(home.path().into()),
                    home.path().join("config.toml"),
                    file,
                );
                let allowed = enabled != Some(true)
                    || (cfg!(target_os = "linux") && (tier == "local" || trusted));
                assert_eq!(complete.is_ok(), allowed, "{:?}", complete.as_ref().err());
            }
        }
    }

    #[test]
    fn sandbox_default_follows_the_supported_platform_adapter() {
        /*
         * Cause/effect graph: C1 OS has the existing native Namespace adapter
         * (Linux/macOS); C2 OS lacks it (Windows); C3 operator explicitly authors
         * a tier. Effects: E1 default Namespace and fail closed if its primitive
         * is unavailable; E2 default to the existing host-backed Local provider;
         * E3 preserve the explicit tier on every OS. Rules D1 Linux/macOS=>E1,
         * D2 Windows=>E2, D3 explicit Local/Namespace=>E3. This keeps platform
         * selection in one config authority and never creates an implicit
         * runtime downgrade path.
         */
        assert_eq!(default_sandbox_tier("linux"), SandboxTier::Namespace, "D1");
        assert_eq!(default_sandbox_tier("macos"), SandboxTier::Namespace, "D1");
        assert_eq!(default_sandbox_tier("windows"), SandboxTier::Local, "D2");

        let data = Path::new("/tmp/awaken-config-test");
        for (value, expected) in [
            ("local", SandboxTier::Local),
            ("namespace", SandboxTier::Namespace),
        ] {
            let resolved = resolve(
                &FileConfig {
                    sandbox_tier: Some(value.into()),
                    ..Default::default()
                },
                data,
            )
            .unwrap();
            assert_eq!(resolved.sandbox_tier, expected, "D3 {value}");
        }
    }

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
