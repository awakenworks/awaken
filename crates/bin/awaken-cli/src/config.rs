//! Product bootstrap configuration for the `awaken` command.
//!
//! It resolves an explicit `--config` path or the standard
//! `~/.awaken/config.toml`, command-line presentation overrides, and defaults
//! into one [`ResolvedDeployment`]. Process environment is not a deployment,
//! business, model, or credential configuration source. The
//! server, runtime host, worker, diagnostics, and storage commands consume that
//! typed value; they do not independently rediscover the deployment.

use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::path::PathBuf;

#[cfg(test)]
use awaken_session_executor::{ContentRedaction, PackageImageBuilder};
use awaken_session_executor::{DeploymentConfig, DispatchBackend, StoreKind};
mod browser_access;
mod cloud_iam;
mod deployment;
mod file_schema;
mod file_support;
mod local_product;
mod report;
mod resolve;
mod role;
mod runtime_settings;
mod seal_key;
mod service_boundary;
mod worker_bootstrap;
mod workspace_data;

pub use awaken_worker::WorkerBootstrap;
pub use cloud_iam::CloudIamConfig;
pub use deployment::{
    CloudModelMode, ConfigOverrides, CoordinatorStoreConfig, OperatingMode, ResolvedDeployment,
    ResourceStoreBackend,
};
use file_schema::FileConfig;
use file_support::{
    override_port, read_database_url_file, resolve_data_dir, resolve_dispatch_backend,
    resolve_expected_platform_workspace_id, resolve_runtime_database_url, select_store_url,
    validate_suite_hub_url,
};
pub use role::Role;
pub use seal_key::SealKeySource;
pub use service_boundary::{ControlServiceConfig, ExecutableAgentRegistrationConfig};
pub use worker_bootstrap::KubernetesWorkerTransport;
pub(crate) use workspace_data::{WorkspaceDataLeaseGuard, enforce_workspace_data_lease};

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

#[cfg(test)]
pub(in crate::config) fn resolve_test_file(
    overrides: ConfigOverrides,
    home: Option<PathBuf>,
    config_path: PathBuf,
    file: FileConfig,
) -> Result<ResolvedDeployment, String> {
    ResolvedDeployment::resolve_file(
        overrides,
        home,
        config_path,
        file,
        awaken_time::system_clock(),
    )
}

#[cfg(test)]
pub(crate) fn local_test_deployment(data_dir: PathBuf) -> ResolvedDeployment {
    crate::config::resolve_test_file(
        ConfigOverrides {
            data_dir: Some(data_dir),
            ..Default::default()
        },
        Some(PathBuf::from("/home/test")),
        PathBuf::from("/home/test/.awaken/config.toml"),
        FileConfig::default(),
    )
    .expect("test local deployment")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn resolve(file: FileConfig, overrides: ConfigOverrides) -> ResolvedDeployment {
        crate::config::resolve_test_file(
            overrides,
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file.clone(),
        )
        .unwrap()
    }

    fn acp_observation(
        id: &str,
        detection: awaken_acp_application::AcpDetectionState,
    ) -> awaken_acp_application::AcpHostObservation {
        awaken_acp_application::AcpHostObservation {
            cli_id: id.to_string(),
            display_name: id.to_string(),
            detection,
            version: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then(|| "1.0".to_string()),
            credential_state: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then_some(awaken_runtime_contract::CredentialObservationState::Available),
            reason_code: Some("fixture".to_string()),
            capability_state: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then_some(awaken_acp_application::AcpCapabilityState::Verified),
            capability_fingerprint: (detection
                == awaken_acp_application::AcpDetectionState::Detected)
                .then(|| "fixture".to_string()),
            capability_reason_code: None,
        }
    }

    #[test]
    fn image_prewarm_follow_one_typed_configuration_boundary() {
        // Causes C1 omitted/valid/invalid policy; C2 k8s/other backend. Rules
        // C1 omitted => legacy disabled; valid+k8s => exact typed policy;
        // invalid or configured non-k8s => reject before backend effects.
        assert!(
            resolve(FileConfig::default(), ConfigOverrides::default())
                .runtime
                .sandbox
                .k8s_image_prewarm
                .is_none()
        );
        for (tier, policy, accepts) in [
            (
                "k8s",
                "{ max_images=8, node_selector={pool='sandbox'} }",
                true,
            ),
            ("k8s", "{ max_images=0 }", false),
            ("k8s", "{ max_images=257 }", false),
            ("docker", "{}", false),
        ] {
            let file: FileConfig = toml::from_str(&format!(
                "sandbox_tier='{tier}'\nk8s_image_prewarm={policy}\n"
            ))
            .unwrap();
            let result = crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            );
            assert_eq!(result.is_ok(), accepts, "{tier}/{policy}");
            if let Ok(config) = result {
                assert_eq!(
                    config.runtime.sandbox.k8s_image_prewarm.unwrap().max_images,
                    8
                );
            }
        }
        assert!(toml::from_str::<FileConfig>("k8s_image_prewarm={unknown=true}").is_err());
    }

    #[test]
    fn observability_is_explicit_typed_deployment_policy() {
        // Cause/effect graph:
        // config.toml observability fields -> one ResolvedDeployment value ->
        // subscriber/tracer/meter initialization; no observability library reads
        // ambient process configuration.
        //
        // Decision table:
        // | log/protocol/timing input | result |
        // | absent | text + info + disabled OTLP + 10s/60s defaults |
        // | valid explicit JSON/HTTP/timings/headers | exact typed projection |
        // | invalid log/protocol, empty filter/header, zero timing | reject |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(defaults.observability, Default::default());

        let explicit = resolve(
            FileConfig {
                log_filter: Some("awaken=debug".to_owned()),
                log_format: Some("json".to_owned()),
                trace_file: Some(PathBuf::from("/tmp/trace.jsonl")),
                otlp_endpoint: Some("http://collector:4318".to_owned()),
                otlp_protocol: Some("http/protobuf".to_owned()),
                otlp_headers: Some(BTreeMap::from([(
                    "authorization".to_owned(),
                    "Bearer token".to_owned(),
                )])),
                otlp_timeout_ms: Some(2500),
                otel_metric_export_interval_ms: Some(5000),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            explicit.observability.log_format,
            awaken_observability::LogFormat::Json
        );
        assert_eq!(
            explicit.observability.otel.metric_export_interval,
            std::time::Duration::from_secs(5)
        );

        for file in [
            FileConfig {
                log_format: Some("yaml".to_owned()),
                ..Default::default()
            },
            FileConfig {
                log_filter: Some(" ".to_owned()),
                ..Default::default()
            },
            FileConfig {
                otlp_protocol: Some("udp".to_owned()),
                ..Default::default()
            },
            FileConfig {
                otlp_timeout_ms: Some(0),
                ..Default::default()
            },
            FileConfig {
                otlp_headers: Some(BTreeMap::from([(" ".to_owned(), "x".to_owned())])),
                ..Default::default()
            },
        ] {
            assert!(
                crate::config::resolve_test_file(
                    ConfigOverrides::default(),
                    Some(PathBuf::from("/home/dev")),
                    PathBuf::from("/home/dev/.awaken/config.toml"),
                    file,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn local_discovery_is_the_only_source_of_exact_worker_routes() {
        // Cause graph:
        // The ACP application service supplies the already filtered route ids;
        // this startup method only projects them into AcpWorkerProfile. Agent
        // publication selects an exact `acp:<cli>` separately; the Worker profile
        // never owns a default CLI.
        //
        // Decision table:
        // A0 explicit empty                -> discovery/acquisition disabled
        // A1 unconfigured + none detected -> no ACP Worker
        // A2 unconfigured + one detected  -> that exact CLI route
        // A3 unconfigured + many detected -> all exact CLI routes, no selection
        // A4 explicit subset              -> only detected configured CLIs
        // A5 available + capability failure -> diagnostic only, no route
        let missing = acp_observation("claude", awaken_acp_application::AcpDetectionState::Missing);
        let codex = acp_observation("codex", awaken_acp_application::AcpDetectionState::Detected);
        let claude = acp_observation(
            "claude",
            awaken_acp_application::AcpDetectionState::Detected,
        );

        let disabled = resolve(
            FileConfig {
                acp_clis: Some(Vec::new()),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(disabled.configured_acp_clis, Some(Vec::new()), "A0");
        assert!(disabled.runtime.acp.is_none(), "A0");

        let mut none = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(none.configured_acp_clis, None, "A1");
        none.apply_local_acp_observations(vec![missing.clone()], vec![])
            .unwrap();
        assert!(none.runtime.acp.is_none(), "A1");

        let mut one = resolve(FileConfig::default(), ConfigOverrides::default());
        one.apply_local_acp_observations(vec![codex.clone(), missing], vec!["codex".into()])
            .unwrap();
        let one = one.runtime.acp.unwrap();
        assert_eq!(one.cli_ids().collect::<Vec<_>>(), ["codex"], "A2");

        let mut many = resolve(FileConfig::default(), ConfigOverrides::default());
        many.apply_local_acp_observations(
            vec![codex.clone(), claude.clone()],
            vec!["claude".into(), "codex".into()],
        )
        .unwrap();
        let many = many.runtime.acp.unwrap();
        assert_eq!(
            many.cli_ids().collect::<Vec<_>>(),
            ["claude", "codex"],
            "A3"
        );

        let mut incompatible = codex.clone();
        incompatible.capability_state =
            Some(awaken_acp_application::AcpCapabilityState::ProbeFailed);
        incompatible.capability_fingerprint = None;
        let mut failed = resolve(FileConfig::default(), ConfigOverrides::default());
        failed
            .apply_local_acp_observations(vec![incompatible], vec![])
            .unwrap();
        assert!(failed.runtime.acp.is_none(), "A5");

        let mut explicit = resolve(
            FileConfig {
                acp_clis: Some(vec!["claude".into()]),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            explicit.configured_acp_clis.as_deref(),
            Some(["claude".to_owned()].as_slice()),
            "A4"
        );
        explicit
            .apply_local_acp_observations(vec![codex, claude], vec!["claude".into()])
            .unwrap();
        let explicit = explicit.runtime.acp.unwrap();
        assert_eq!(explicit.cli_ids().collect::<Vec<_>>(), ["claude"], "A4");
    }

    #[test]
    fn local_defaults_are_durable_and_single_rooted() {
        let config = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(config.data_dir, PathBuf::from("/home/dev/.awaken"));
        assert_eq!(
            config.runtime.storage_dir.as_deref(),
            Some(config.data_dir.as_path())
        );
        assert!(config.runtime.durable);
        assert!(matches!(config.seal_key, SealKeySource::LocalFile(_)));
        assert_eq!(config.role, Role::AllInOne);
    }

    #[test]
    fn expected_platform_workspace_is_one_optional_deployment_resume_fence() {
        /* Cause/effect graph: C1 expected identity omitted/present; C2 authored
         * value empty/non-empty. Effects: E1 omission preserves ordinary
         * initialize-or-resume startup; E2 a non-empty value is normalized into
         * the one ResolvedDeployment authority; E3 empty input fails before any
         * filesystem probe. Decision table: WI1 !C1=>None; WI2 C1+non-empty=>
         * exact trimmed identity; WI3 C1+empty=>configuration error. */
        assert_eq!(
            resolve(FileConfig::default(), ConfigOverrides::default())
                .expected_platform_workspace_id,
            None,
            "WI1"
        );
        let configured = resolve(
            FileConfig {
                expected_platform_workspace_id: Some(" workspace-deployment ".into()),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            configured.expected_platform_workspace_id.as_deref(),
            Some("workspace-deployment"),
            "WI2"
        );
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                expected_platform_workspace_id: Some("  ".into()),
                ..Default::default()
            },
        )
        .expect_err("WI3");
        assert!(
            error.contains("expected_platform_workspace_id"),
            "WI3: {error}"
        );
    }

    #[test]
    fn postgres_pool_size_is_resolved_once_as_a_non_zero_deployment_value() {
        // Cause/effect graph:
        // C1 the author omits postgres_max_connections -> E1 the startup
        // root derives one host-aware non-zero default; C2 the author supplies
        // a positive value -> E2 that exact value is retained; C3 the author
        // supplies zero -> E3 configuration fails before any store is built.
        //
        // Decision table:
        // R1 !configured       => E1
        // R2 configured && > 0 => E2
        // R3 configured && = 0 => E3
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert!(defaulted.runtime.postgres_max_connections.get() > 0, "R1");

        let explicit = resolve(
            FileConfig {
                postgres_max_connections: Some(23),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(explicit.runtime.postgres_max_connections.get(), 23, "R2");

        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                postgres_max_connections: Some(0),
                ..Default::default()
            },
        )
        .expect_err("R3");
        assert!(error.contains("postgres_max_connections"), "R3: {error}");
    }

    #[test]
    fn dispatch_fallback_poll_is_resolved_once_and_bounded() {
        // Causes: C1 TOML value absent/present; C2 present value is zero,
        // within 1..=60000, or above 60000. Effects: E1 omission retains the
        // dispatch owner's existing 500ms default; E2 a valid value reaches the
        // sole DeploymentConfig; E3 either invalid bound fails before startup;
        // E4 an ordinary CLI process keeps its built-in depth sampler enabled.
        // Decision table: P1=!C1 -> E1; P2=C1+valid -> E2;
        // P3=C1+(zero|above maximum) -> E3. Local pool and coordinator-only
        // reconciler consume the same later deployment projection.
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(
            defaulted.runtime.dispatch_poll_interval,
            awaken_dispatch::DispatchServiceConfig::default().poll_interval,
            "P1"
        );
        assert!(defaulted.runtime.sample_dispatch_queue_depth, "P1/E4");
        let configured = resolve(
            toml::from_str("dispatch_poll_interval_ms = 750")
                .expect("P2 TOML key reaches the existing file schema"),
            ConfigOverrides::default(),
        );
        assert_eq!(
            configured.runtime.dispatch_poll_interval,
            std::time::Duration::from_millis(750),
            "P2"
        );
        for invalid in [0, 60_001] {
            let error = crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    dispatch_poll_interval_ms: Some(invalid),
                    ..Default::default()
                },
            )
            .expect_err("P3 rejects invalid fallback interval");
            assert!(error.contains("dispatch_poll_interval_ms"), "P3: {error}");
        }
    }

    #[test]
    fn worker_identity_is_authored_only_at_the_typed_product_boundary() {
        // Cause/effect graph:
        // C1 worker_id omitted -> E1 stable product default; C2 non-empty
        // worker_id supplied -> E2 exact identity retained for WorkerUpstream;
        // C3 empty identity supplied -> E3 configuration fails before startup.
        //
        // Decision table: R1 omitted=>default, R2 non-empty=>exact,
        // R3 empty=>error.
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(defaulted.worker.worker_id, "awaken-worker", "R1");

        let explicit = resolve(
            FileConfig {
                worker_id: Some("worker-shanghai-1".to_owned()),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(explicit.worker.worker_id, "worker-shanghai-1", "R2");

        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                worker_id: Some(" ".to_owned()),
                ..Default::default()
            },
        )
        .expect_err("R3");
        assert!(error.contains("worker_id"), "R3: {error}");
    }

    #[test]
    fn local_realization_reuses_the_canonical_dispatch_owner() {
        // Local restart identity cause graph: C1 dispatch_owner is omitted,
        // explicit, or invalid; C2 this is the all-in-one topology. Effects are
        // E1 one stable embedded logical owner, E2 the normalized configured
        // owner, or E3 startup rejection before a Session lease can be written.
        //
        // | Rule | Owner input | Effect |
        // |---|---|---|
        // | O1 | omitted | E1 embedded-worker |
        // | O2 | non-empty | E2 exact normalized owner |
        // | O3 | whitespace | E3 configuration error |
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(defaulted.runtime.dispatch_owner, "embedded-worker", "O1/E1");

        let explicit = resolve(
            FileConfig {
                dispatch_owner: Some(" local-node-a ".into()),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(explicit.runtime.dispatch_owner, "local-node-a", "O2/E2");

        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                dispatch_owner: Some(" ".into()),
                ..Default::default()
            },
        )
        .expect_err("O3 invalid owner");
        assert!(error.contains("dispatch_owner"), "O3/E3: {error}");
    }

    #[test]
    fn kubernetes_retained_policy_uses_the_existing_runtime_type() {
        // KP graph: C1 policy omitted/present; C2 size nonempty/blank/missing;
        // C3 class omitted/explicit; C4 unknown nested key. KP1 omitted =>
        // retain the existing None default; KP2 present+valid C2, either C3 =>
        // exact Runtime policy, no new PVC registry; KP3 blank/missing/unknown
        // => reject before provider construction. API quantity/class admission
        // remains Kubernetes-owned and is exercised by the live PVC tests.
        for (policy, accepts, expected) in [
            (None, true, None),
            (Some("{ size = '1Gi' }"), true, Some((None, "1Gi"))),
            (
                Some("{ size = '2Gi', storage_class_name = 'retained' }"),
                true,
                Some((Some("retained"), "2Gi")),
            ),
            (Some("{ size = ' ' }"), false, None),
            (Some("{ storage_class_name = 'retained' }"), false, None),
            (Some("{ size = '1Gi', unknown = true }"), false, None),
        ] {
            let mut source = "sandbox_tier = 'k8s'\n".to_owned();
            if let Some(policy) = policy {
                source.push_str(&format!("k8s_continuation_volume = {policy}\n"));
            }
            let result = toml::from_str::<FileConfig>(&source)
                .map_err(|error| error.to_string())
                .and_then(|file| {
                    crate::config::resolve_test_file(
                        ConfigOverrides::default(),
                        Some(PathBuf::from("/home/dev")),
                        PathBuf::from("/home/dev/.awaken/config.toml"),
                        file,
                    )
                });
            assert_eq!(result.is_ok(), accepts, "KP policy={policy:?}: {result:?}");
            if let Ok(resolved) = result {
                assert_eq!(
                    resolved.runtime.sandbox.k8s_continuation_volume,
                    expected.map(
                        |(class, size)| awaken_sandbox_container::K8sContinuationVolume {
                            storage_class_name: class.map(str::to_owned),
                            size: size.into(),
                        }
                    ),
                    "KP exact policy"
                );
            }
        }
    }

    #[test]
    fn sandbox_operator_policy_is_projected_once_from_typed_config() {
        // Cause/effect graph: each explicit file value selects one field on the
        // authoritative DeploymentConfig; omission selects SandboxSettings defaults.
        // No runtime adapter may add environment precedence.
        //
        // | Rule | file values | Effect |
        // |---|---|---|
        // | S1 | all omitted | trusted host disabled, fail-closed fallback, no pool/proxy, default namespace/tools |
        // | S2 | all explicit valid | exact typed values projected losslessly |
        // | S3 | Hand idle seconds = 0 | disable Worker-local idle hibernation |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default())
            .runtime
            .sandbox;
        assert!(!defaults.trusted_host_enabled, "S1");
        assert!(!defaults.trusted_host_scoped_execution, "S1");
        assert!(!defaults.allow_local_fallback, "S1");
        assert_eq!(defaults.warm_pool_size, 0, "S1");
        assert_eq!(defaults.warm_pool_total_size, 16, "S1");
        assert_eq!(defaults.warm_pool_idle_ttl_secs, 300, "S1");
        assert_eq!(defaults.container_forward_proxy, None, "S1");
        assert_eq!(defaults.k8s_namespace, "default", "S1");
        assert!(defaults.k8s_image_pull_secrets.is_empty(), "S1");
        assert_eq!(
            defaults.container_hand_bin, "/usr/local/bin/awaken-sandbox",
            "S1"
        );
        assert_eq!(defaults.container_hand_idle_secs, 300, "S1");
        assert_eq!(defaults.podman_bin, "podman", "S1");
        assert_eq!(defaults.package_image_registry, None, "S1");
        assert_eq!(defaults.package_registry_auth_file, None, "S1");
        assert!(!defaults.package_registry_insecure, "S1");
        assert!(
            !defaults.package_build_host_network,
            "S1 default build network is unchanged"
        );
        assert_eq!(
            defaults.k8s_buildkit_image, "moby/buildkit:v0.30.0-rootless",
            "S1"
        );
        assert_eq!(defaults.package_image_builder, None, "S1");
        assert!(!defaults.inherit_agent_stderr, "S1");

        let selected = resolve(
            FileConfig {
                sandbox_trusted_host_enabled: Some(true),
                sandbox_allow_local_fallback: Some(true),
                sandbox_warm_pool_size: Some(3),
                sandbox_warm_pool_total_size: Some(7),
                sandbox_warm_pool_idle_ttl_secs: Some(41),
                container_forward_proxy: Some("http://proxy.internal:8080".into()),
                k8s_namespace: Some("agents".into()),
                k8s_image_pull_secrets: Some(vec![" registry-pull ".into(), String::new()]),
                container_hand_bin: Some("/opt/awaken/bin/hand".into()),
                container_hand_idle_secs: Some(73),
                podman_bin: Some("/opt/podman/bin/podman".into()),
                package_image_registry: Some("registry.internal/agents/".into()),
                package_registry_auth_file: Some(PathBuf::from("/run/secrets/registry.json")),
                package_registry_insecure: Some(true),
                package_build_host_network: Some(true),
                k8s_buildkit_image: Some(
                    "registry.internal/system/buildkit:v0.30.0-rootless".into(),
                ),
                package_image_builder: Some("k8s".into()),
                package_local_cache_ttl_secs: Some(300),
                sandbox_inherit_agent_stderr: Some(true),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        )
        .runtime
        .sandbox;
        assert!(selected.trusted_host_enabled, "S2");
        assert!(
            selected.package_build_host_network,
            "S2 explicit build-only operator opt-in"
        );
        assert!(selected.allow_local_fallback, "S2");
        assert_eq!(selected.warm_pool_size, 3, "S2");
        assert_eq!(selected.warm_pool_total_size, 7, "S2");
        assert_eq!(selected.warm_pool_idle_ttl_secs, 41, "S2");
        assert_eq!(
            selected.container_forward_proxy.as_deref(),
            Some("http://proxy.internal:8080"),
            "S2"
        );
        assert_eq!(selected.k8s_namespace, "agents", "S2");
        assert_eq!(selected.k8s_image_pull_secrets, ["registry-pull"], "S2");
        assert_eq!(selected.container_hand_bin, "/opt/awaken/bin/hand", "S2");
        assert_eq!(selected.container_hand_idle_secs, 73, "S2");
        assert_eq!(selected.podman_bin, "/opt/podman/bin/podman", "S2");
        assert_eq!(
            selected.package_image_registry.as_deref(),
            Some("registry.internal/agents"),
            "S2"
        );
        assert_eq!(
            selected.package_image_builder,
            Some(PackageImageBuilder::Kubernetes),
            "S2"
        );
        assert_eq!(
            selected.k8s_buildkit_image, "registry.internal/system/buildkit:v0.30.0-rootless",
            "S2"
        );
        assert_eq!(
            selected.package_registry_auth_file.as_deref(),
            Some(Path::new("/run/secrets/registry.json")),
            "S2"
        );
        assert!(selected.package_registry_insecure, "S2");
        assert_eq!(selected.package_local_cache_ttl_secs, 300, "S2");
        assert!(selected.inherit_agent_stderr, "S2");

        let disabled = resolve(
            FileConfig {
                container_hand_idle_secs: Some(0),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        )
        .runtime
        .sandbox;
        assert_eq!(disabled.container_hand_idle_secs, 0, "S3");
    }

    #[test]
    fn sandbox_operator_policy_rejects_invalid_active_values() {
        // Cause/effect graph: substrate executables and namespace must be
        // non-empty; package-registry modifiers require a configured registry.
        //
        // | Rule | required string | registry | modifier | Effect |
        // |---|---|---|---|---|
        // | V1 | empty | any | any | reject |
        // | V4 | valid | absent | builder/auth/insecure | reject |
        let resolve_error = |file: FileConfig| {
            crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            )
            .unwrap_err()
        };
        assert!(
            resolve_error(FileConfig {
                k8s_namespace: Some(" ".into()),
                ..FileConfig::default()
            })
            .contains("k8s_namespace"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                container_hand_bin: Some(String::new()),
                ..FileConfig::default()
            })
            .contains("container_hand_bin"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                podman_bin: Some(String::new()),
                ..FileConfig::default()
            })
            .contains("podman_bin"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                package_image_builder: Some("docker".into()),
                ..FileConfig::default()
            })
            .contains("package_image_registry"),
            "V4"
        );
        assert!(
            resolve_error(FileConfig {
                package_registry_auth_file: Some(PathBuf::from("/run/secrets/registry.json")),
                ..FileConfig::default()
            })
            .contains("package_image_registry"),
            "V4"
        );
        assert!(
            resolve_error(FileConfig {
                package_registry_insecure: Some(true),
                ..FileConfig::default()
            })
            .contains("package_image_registry"),
            "V4"
        );
    }

    #[test]
    fn content_capture_policy_has_one_typed_authoring_boundary() {
        // Cause/effect graph: omitted values select the privacy-preserving
        // Structured/None defaults; known strings project to typed values;
        // unknown strings fail before Host construction.
        //
        // | Rule | capture | redaction | Effect |
        // |---|---|---|---|
        // | P1 | omitted | omitted | Structured + None |
        // | P2 | Full | Regex | exact typed policy |
        // | P3 | unknown | either | reject |
        // | P4 | either | unknown | reject |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default())
            .runtime
            .content_capture;
        assert_eq!(
            defaults.level,
            awaken_runtime_contract::ContentCapture::Structured,
            "P1"
        );
        assert_eq!(defaults.redaction, ContentRedaction::None, "P1");

        let selected = resolve(
            FileConfig {
                content_capture: Some("full".into()),
                content_redaction: Some("regex".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        )
        .runtime
        .content_capture;
        assert_eq!(
            selected.level,
            awaken_runtime_contract::ContentCapture::Full,
            "P2"
        );
        assert_eq!(selected.redaction, ContentRedaction::Regex, "P2");

        let resolve_error = |file: FileConfig| {
            crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            )
            .unwrap_err()
        };
        assert!(
            resolve_error(FileConfig {
                content_capture: Some("everything".into()),
                ..FileConfig::default()
            })
            .contains("content_capture"),
            "P3"
        );
        assert!(
            resolve_error(FileConfig {
                content_redaction: Some("external".into()),
                ..FileConfig::default()
            })
            .contains("content_redaction"),
            "P4"
        );
    }

    #[test]
    fn precedence_is_cli_then_file_then_default() {
        let config = resolve(
            FileConfig {
                data_dir: Some(PathBuf::from("/file")),
                bind: Some("127.0.0.1:7000".to_owned()),
                ..FileConfig::default()
            },
            ConfigOverrides {
                data_dir: Some(PathBuf::from("/cli")),
                port: Some(9100),
                ..ConfigOverrides::default()
            },
        );
        assert_eq!(config.data_dir, PathBuf::from("/cli"));
        assert_eq!(config.bind, "127.0.0.1:9100");
    }

    #[test]
    fn cloud_inference_url_is_independent_from_identity_url() {
        let config = resolve(
            FileConfig {
                cloud_api_url: Some("https://file.cloud.example".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            config.cloud_iam.inference_base_url,
            "https://file.cloud.example"
        );
        assert_eq!(
            resolve(FileConfig::default(), ConfigOverrides::default())
                .cloud_iam
                .inference_base_url,
            "https://api.awakenworks.com"
        );
    }

    #[test]
    fn suite_hub_is_optional_exact_and_transport_safe() {
        // Cause graph: deployment mode + exact absolute URL -> browser exit;
        // absent value -> standalone console; remote cleartext or URL-carried
        // credentials/query/fragment -> fail before the listener starts.
        //
        // | rule | mode | value | effect |
        // | S1 | any | absent | standalone projection |
        // | S2 | server | HTTPS path | exact hub retained |
        // | S3 | local | loopback HTTP | exact dev hub retained |
        // | S4 | server | HTTP | reject |
        // | S5 | any | credential/query/fragment | reject |
        let standalone = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(standalone.suite_hub_url, None, "S1");

        let hosted = resolve(
            FileConfig {
                mode: Some("server".into()),
                control_seal_key: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                suite_hub_url: Some("https://cloud.example/products".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            hosted.suite_hub_url.as_deref(),
            Some("https://cloud.example/products"),
            "S2"
        );

        let local = resolve(
            FileConfig {
                suite_hub_url: Some("http://127.0.0.1:17878/products".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            local.suite_hub_url.as_deref(),
            Some("http://127.0.0.1:17878/products"),
            "S3"
        );

        let error_for = |value: &str| {
            crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    mode: Some("server".into()),
                    control_seal_key: Some(
                        "0000000000000000000000000000000000000000000000000000000000000000".into(),
                    ),
                    suite_hub_url: Some(value.into()),
                    ..FileConfig::default()
                },
            )
            .unwrap_err()
        };
        assert!(
            error_for("http://cloud.example/products").contains("HTTPS"),
            "S4"
        );
        for unsafe_url in [
            "https://user@cloud.example/products",
            "https://cloud.example/products?tenant=forbidden",
            "https://cloud.example/products#fragment",
        ] {
            assert!(error_for(unsafe_url).contains("must not contain"), "S5");
        }
    }

    #[test]
    fn cloud_login_and_cloud_model_supply_are_independent_and_fail_closed() {
        // Cause graph: C1 is interactive all-in-one; C2 explicitly selects an
        // identity; C3 explicitly selects Cloud models. Effects are the
        // product preset, an explicit local bypass, BYOK-only Cloud login, or a
        // fail-closed incompatible pair.
        //
        // | Rule | product | explicit identity | explicit supply | Result |
        // |---|---|---|---|---|
        // | F1 | interactive | absent | absent | Cloud login + brokered supply |
        // | F2 | interactive | no-login/self-managed | absent | local/BYOK only |
        // | F3 | interactive | awaken-cloud | disabled | Cloud login + local/BYOK only |
        // | F4 | any | non-Cloud | enabled | startup configuration error |
        // | F5 | server/split | absent | absent | non-interactive existing defaults |
        let local = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(
            local.identity_mode,
            awaken_control::ManagementIdentityMode::AwakenCloud,
            "F1"
        );
        assert_eq!(local.cloud_models, CloudModelMode::Enabled, "F1");

        for identity in ["no-login", "self-managed"] {
            let bypass = resolve(
                FileConfig {
                    identity_mode: Some(identity.into()),
                    ..FileConfig::default()
                },
                ConfigOverrides::default(),
            );
            assert_eq!(bypass.cloud_models, CloudModelMode::Disabled, "F2");
        }

        let login_only = resolve(
            FileConfig {
                identity_mode: Some("awaken-cloud".into()),
                cloud_models: Some("disabled".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(login_only.cloud_models, CloudModelMode::Disabled, "F3");

        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                identity_mode: Some("no-login".into()),
                cloud_models: Some("enabled".into()),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(
            error.contains("cloud_models_require_awaken_cloud_identity"),
            "F4"
        );

        let server = resolve(
            FileConfig {
                mode: Some("server".into()),
                control_seal_key: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            server.identity_mode,
            awaken_control::ManagementIdentityMode::SelfManaged,
            "F5"
        );
        assert_eq!(server.cloud_models, CloudModelMode::Disabled, "F5");
    }

    #[test]
    fn command_line_cloud_modes_override_the_config_file() {
        let config = resolve(
            FileConfig {
                identity_mode: Some("no-login".into()),
                cloud_models: Some("disabled".into()),
                ..FileConfig::default()
            },
            ConfigOverrides {
                identity_mode: Some(awaken_control::ManagementIdentityMode::AwakenCloud),
                cloud_models: Some(CloudModelMode::Enabled),
                ..ConfigOverrides::default()
            },
        );
        assert_eq!(
            config.identity_mode,
            awaken_control::ManagementIdentityMode::AwakenCloud
        );
        assert_eq!(config.cloud_models, CloudModelMode::Enabled);
    }

    #[test]
    fn cloud_login_diagnostics_show_coordinates_and_redact_credentials() {
        // Diagnostic cause/effect rules: public OAuth coordinates are visible
        // for configuration diagnosis; credential presence is classified by
        // source; cleartext token material is absent from both renderings.
        let config = resolve(
            FileConfig {
                cloud_iam_issuer: Some("https://accounts.example".into()),
                cloud_oauth_client_id: Some("desktop-example".into()),
                cloud_oauth_redirect_uri: Some("http://127.0.0.1:34115/callback".into()),
                cloud_access_token: Some("must-not-render".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        for report in [config.report(false), config.report(true)] {
            assert!(report.contains("https://accounts.example"));
            assert!(report.contains("desktop-example"));
            assert!(report.contains("explicit access token"));
            assert!(!report.contains("must-not-render"));
        }
    }

    #[test]
    fn worker_requires_an_explicit_server() {
        let error = crate::config::resolve_test_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig::default(),
        )
        .unwrap_err();
        assert!(error.contains("--server"));
    }

    include!("config/local_pool_policy_tests.rs");

    #[test]
    fn coordinator_database_file_is_one_execution_store_coordinate() {
        // Causes: C1 role=Coordinator; C2 the runtime database is supplied by a
        // projected file; C3 no per-store execution URLs exist. Effects: E1 the
        // file is read once as Postgres; E2 Runtime, Session, captured content,
        // and Resources use that exact coordinate; E3 no SQLite authority is
        // opened. Decision rule D1=C1+C2+C3 -> E1+E2+E3.
        let dir = tempfile::tempdir().unwrap();
        let database_file = dir.path().join("database-url");
        std::fs::write(&database_file, "postgres://cloud/execution\n").unwrap();
        let deployment = crate::config::resolve_test_file(
            ConfigOverrides {
                role: Some(Role::Coordinator),
                ..Default::default()
            },
            Some(dir.path().to_path_buf()),
            dir.path().join("config.toml"),
            FileConfig {
                internal_bind: Some("127.0.0.1:8081".into()),
                runtime_database_url_file: Some(database_file),
                control_internal_url: Some("http://control:3000".into()),
                control_service_token_file: Some("/run/control-service-token".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            deployment.runtime.database_url.as_deref(),
            Some("postgres://cloud/execution"),
            "D1"
        );
        for store in [
            &deployment.coordinator.sessions,
            &deployment.coordinator.captured_content,
        ] {
            assert!(
                matches!(store, awaken_control::StoreBackend::Postgres(url) if url == "postgres://cloud/execution"),
                "D1"
            );
        }
        assert!(
            matches!(
                deployment.resources,
                ResourceStoreBackend::Postgres(ref url) if url == "postgres://cloud/execution"
            ),
            "D1"
        );
    }

    #[test]
    fn environment_database_belongs_only_to_control() {
        // Cause/effect decision table:
        // R1 Environment DB on Control -> accepted as static authoring authority;
        // R2/R3 Session/captured-content DB on Control -> rejected;
        // R4 Environment DB on Coordinator -> rejected before store acquisition.
        let control = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                role: Some("control".to_owned()),
                internal_bind: Some("127.0.0.1:8081".into()),
                environment_db: Some("postgres://control/environments".to_owned()),
                control_service_token_file: Some("/run/control-service-token".into()),
                ..Default::default()
            },
        )
        .expect("R1");
        assert!(
            matches!(
                control.control.environment,
                awaken_control::StoreBackend::Postgres(_)
            ),
            "R1"
        );

        for (rule, field, file) in [
            (
                "R2",
                "sessions_db",
                FileConfig {
                    sessions_db: Some("postgres://shared/sessions".to_owned()),
                    ..Default::default()
                },
            ),
            (
                "R3",
                "captured_content_db",
                FileConfig {
                    captured_content_db: Some("postgres://shared/capture".to_owned()),
                    ..Default::default()
                },
            ),
        ] {
            let error = crate::config::resolve_test_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    role: Some("control".to_owned()),
                    control_service_token_file: Some("/run/control-service-token".into()),
                    ..file
                },
            )
            .unwrap_err();
            assert!(error.contains(field), "{rule}: {error}");
        }

        let coordinator_error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                role: Some("coordinator".to_owned()),
                environment_db: Some("postgres://control/environments".to_owned()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(coordinator_error.contains("environment_db"), "R4");
    }

    #[test]
    fn worker_credential_observation_window_fails_closed() {
        let error = crate::config::resolve_test_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                worker_server: Some("http://control".to_owned()),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                worker_credential_probe_interval_secs: Some(10),
                worker_credential_observation_ttl_secs: Some(10),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("TTL"));
    }

    #[test]
    fn reports_never_include_database_credentials() {
        // Cause/effect decision table: R1 Local mode -> report automatic
        // startup migration; R2 Server mode -> report explicit migration and
        // verify-only startup; R3 either mode with credential-bearing URLs ->
        // redact credentials and schemes. The report must not promise the Local
        // lifecycle for a distributed deployment.
        let config = resolve(
            FileConfig {
                runtime_database_url: Some("postgres://user:secret@db/awaken".to_owned()),
                resource_database_url: Some("postgres://user:secret@db/resources".to_owned()),
                admin_db: Some("postgres://user:secret@db/admin".to_owned()),
                ..FileConfig::default()
            },
            Default::default(),
        );
        for report in [config.report(false), config.report(true)] {
            assert!(report.contains("automatic at startup"), "R1: {report}");
            assert!(!report.contains("user:secret"), "{report}");
            assert!(!report.contains("postgres://"), "{report}");
        }
        let server = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                mode: Some("server".to_owned()),
                runtime_database_url: Some("postgres://db/awaken".to_owned()),
                resource_database_url: Some("postgres://db/awaken".to_owned()),
                admin_db: Some("postgres://db/awaken".to_owned()),
                control_seal_key: Some(
                    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_owned(),
                ),
                ..FileConfig::default()
            },
        )
        .unwrap();
        for report in [server.report(false), server.report(true)] {
            assert!(report.contains("awaken database migrate"), "R2: {report}");
            assert!(report.contains("startup verifies"), "R2: {report}");
            assert!(!report.contains("postgres://"), "R3: {report}");
        }
    }

    #[test]
    fn reports_only_role_owned_database_groups() {
        // Cause/effect decision table:
        // R1 split Control -> report only Control including Data Subject and
        // static Environment definitions; R2 split Coordinator -> report only
        // Coordinator stores plus Resources/runtime (the executable Environment
        // projection is part of the runtime migration scope); R3 either JSON
        // report ->
        // the unowned database group is empty. Resolved compatibility defaults
        // therefore never appear as authority granted to another process.
        let control = resolve(
            FileConfig {
                internal_bind: Some("127.0.0.1:8081".into()),
                control_service_token_file: Some("/run/control-service-token".into()),
                ..Default::default()
            },
            ConfigOverrides {
                role: Some(Role::Control),
                ..Default::default()
            },
        );
        let control_text = control.report(false);
        assert!(
            control_text.contains("Control databases"),
            "R1: {control_text}"
        );
        assert!(control_text.contains("data_subject"), "R1");
        assert!(control_text.contains("environments"), "R1");
        assert!(!control_text.contains("Coordinator databases"), "R1");
        assert!(
            control_text.contains("Resources backend    not owned"),
            "R1"
        );
        let control_json: serde_json::Value = serde_json::from_str(&control.report(true)).unwrap();
        assert_eq!(
            control_json["coordinator_databases"],
            serde_json::json!({}),
            "R3"
        );

        let coordinator = resolve(
            FileConfig {
                internal_bind: Some("127.0.0.1:8081".into()),
                runtime_database_url: Some("postgres://coordinator/runtime".to_owned()),
                resource_database_url: Some("postgres://resources/content".to_owned()),
                control_internal_url: Some("http://control:3000".to_owned()),
                control_service_token_file: Some("/run/control-service-token".into()),
                ..Default::default()
            },
            ConfigOverrides {
                role: Some(Role::Coordinator),
                ..Default::default()
            },
        );
        let coordinator_text = coordinator.report(false);
        assert!(
            coordinator_text.contains("Coordinator databases"),
            "R2: {coordinator_text}"
        );
        assert!(!coordinator_text.contains("Control databases"), "R2");
        assert!(!coordinator_text.contains("catalog"), "R2");
        assert!(coordinator_text.contains("captured_content"), "R2");
        assert!(
            coordinator_text.contains("runtime dispatch     postgres"),
            "R2"
        );
        let coordinator_json: serde_json::Value =
            serde_json::from_str(&coordinator.report(true)).unwrap();
        assert_eq!(
            coordinator_json["control_databases"],
            serde_json::json!({}),
            "R3"
        );
    }

    #[test]
    fn projected_management_database_url_is_one_shared_secret_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database-url");
        fs::write(&path, "postgres://user:secret@database/awaken\n").unwrap();
        let config = resolve(
            FileConfig {
                management_database_url_file: Some(path),
                ..FileConfig::default()
            },
            Default::default(),
        );
        for backend in [
            &config.control.catalog,
            &config.control.credential,
            &config.control.config,
            &config.control.admin,
            &config.control.data_subject,
            &config.control.environment,
            &config.coordinator.sessions,
            &config.coordinator.captured_content,
        ] {
            assert!(matches!(backend, awaken_control::StoreBackend::Postgres(_)));
        }
        assert!(config.resources.is_shared());
        assert_eq!(
            config
                .origins
                .get("management_database")
                .map(String::as_str),
            Some("management_database_url_file")
        );
        assert!(!config.report(false).contains("user:secret"));
    }

    #[test]
    fn projected_management_database_url_rejects_ambiguous_or_invalid_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database-url");
        fs::write(&path, "postgres://database/awaken").unwrap();
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                management_database_url_file: Some(path.clone()),
                catalog_db: Some("postgres://other/catalog".to_owned()),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("cannot be combined"));

        fs::write(&path, "https://not-a-database.example").unwrap();
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                management_database_url_file: Some(path),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("must contain a postgres:// URL"));
    }

    #[test]
    fn local_seal_key_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-seal.key");
        let source = SealKeySource::LocalFile(path.clone());
        assert_eq!(
            source.load_or_create().unwrap(),
            source.load_or_create().unwrap()
        );
        assert_eq!(fs::read_to_string(&path).unwrap().trim().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn hosted_cloud_identity_requires_one_projected_service_credential() {
        let mut file = FileConfig {
            mode: Some("server".to_owned()),
            identity_mode: Some("awaken-cloud".to_owned()),
            control_seal_key: Some(
                "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            ),
            ..FileConfig::default()
        };
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file.clone(),
        )
        .unwrap_err();
        assert!(error.contains("cloud_iam_service_token_file"));

        file.cloud_iam_service_token_file =
            Some(PathBuf::from("/var/run/awaken/management-iam/token"));
        let config = resolve(file.clone(), ConfigOverrides::default());
        assert_eq!(
            config.cloud_iam.service_token_file.as_deref(),
            Some(Path::new("/var/run/awaken/management-iam/token"))
        );
        assert!(config.cloud_iam.service_token.is_none());

        file.cloud_iam_service_token = Some("inline-is-forbidden".to_owned());
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file.clone(),
        )
        .unwrap_err();
        assert!(error.contains("configure exactly one"));

        file.cloud_iam_service_token = None;
        file.cloud_access_token = Some("inline-user-token-is-forbidden".to_owned());
        let error = crate::config::resolve_test_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file,
        )
        .unwrap_err();
        assert!(error.contains("forbids inline Cloud credentials"));
    }
}
