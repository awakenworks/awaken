//! Deployment-driven selection of the local Session environment tiers:
//! Workdir, Namespace, and the operator-enabled TrustedHost backend.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_session_executor::{DeploymentConfig, SandboxTier, SharedHost};
use awaken_session_environment::SandboxEnvironmentProvider;
use awaken_session_environment_contract::HandExecutorFactory;

/// Resolve the effective sandbox tier at startup, probing the OS-native launcher
/// once for the `Namespace` tier so an unsupported host gets a clear startup
/// decision instead of an opaque per-run spawn error. An unavailable provider
/// fails closed (`Err`) by default; only when typed deployment policy opts in
/// does it degrade to the UNSANDBOXED `Local` tier with a loud notice. Every
/// other tier passes through unchanged.
pub async fn resolve_sandbox_tier(
    tier: SandboxTier,
    allow_local_fallback: bool,
    namespace_base: &Path,
) -> Result<SandboxTier, String> {
    use awaken_provisioning_contract::SandboxProvider as _;
    if tier != SandboxTier::Namespace {
        return Ok(tier);
    }
    match awaken_sandbox_local::NamespaceProvider::new(namespace_base.to_path_buf())
        .probe_ready()
        .await
    {
        Ok(()) => Ok(SandboxTier::Namespace),
        Err(e) if namespace_degrades_to_local(allow_local_fallback) => {
            eprintln!(
                "awaken: OS-native sandbox unavailable ({e}); running UNSANDBOXED local ACP \
                 execution (no OS isolation for this worker). Install bwrap on Linux or enable \
                 macOS Seatbelt, or disable sandbox.allow_local_fallback to require \
                 isolation (fail closed)."
            );
            Ok(SandboxTier::Local)
        }
        Err(e) => Err(format!(
            "OS-native sandbox unavailable: {e}. Install bwrap (Linux) / use macOS Seatbelt, \
             configure sandbox_allow_local_fallback=true for an explicit unsafe fallback, or \
             configure sandbox_tier=local to run unsandboxed"
        )),
    }
}

/// Whether an unavailable `Namespace` provider may degrade to the UNSANDBOXED
/// `Local` tier. Isolation never silently downgrades: the explicit unsafe opt-in
/// is required whether the namespace tier was configured explicitly or selected
/// as a default. Pure, so the policy is unit-testable off-host.
fn namespace_degrades_to_local(fallback_optin: bool) -> bool {
    fallback_optin
}

async fn validate_namespace_hand_bin(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "namespace Session environments require the release companion at {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "namespace Session environment companion is not a file: {}",
            path.display()
        ));
    }

    let child = tokio::process::Command::new(path)
        .args(["hand", "--check"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("start namespace hand companion {}: {error}", path.display()))?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "namespace hand companion check timed out after 5s: {}",
                path.display()
            )
        })?
        .map_err(|error| format!("check namespace hand companion {}: {error}", path.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("namespace hand companion check failed: {}", path.display())
    } else {
        format!("namespace hand companion check failed: {detail}")
    })
}

/// The installed Hand companion beside `current_exe`: the release sibling, or
/// Cargo's profile-level artifact for a test or example binary.
pub(crate) fn namespace_hand_path(current_exe: &Path) -> PathBuf {
    let binary_name = format!("awaken-sandbox{}", std::env::consts::EXE_SUFFIX);
    let sibling = current_exe.with_file_name(&binary_name);
    let Some(parent) = current_exe.parent() else {
        return sibling;
    };
    if matches!(
        parent.file_name().and_then(std::ffi::OsStr::to_str),
        Some("deps" | "examples")
    ) && let Some(profile_dir) = parent.parent()
    {
        let cargo_companion = profile_dir.join(binary_name);
        if cargo_companion.is_file() {
            return cargo_companion;
        }
    }
    sibling
}

/// The installed Hand companion of this process.
pub(crate) fn installed_namespace_hand_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .map(|path| namespace_hand_path(&path))
        .unwrap_or_else(|| PathBuf::from(format!("awaken-sandbox{}", std::env::consts::EXE_SUFFIX)))
}

/// The provider of a non-container Deployment tier; container tiers return
/// `None` because their provider requires async assembly.
fn local_tier_provider(
    tier: SandboxTier,
    base: PathBuf,
    inherit_agent_stderr: bool,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    namespace_hand_bin: String,
    hand_idle_after: std::time::Duration,
) -> Result<Option<SandboxEnvironmentProvider>, String> {
    Ok(match tier {
        SandboxTier::Local => Some(SandboxEnvironmentProvider::workdir_with_agent_stderr(
            base,
            inherit_agent_stderr,
        )),
        SandboxTier::Namespace => {
            let hand_factory = hand_factory
                .ok_or("namespace Session environments require a hand executor factory")?;
            Some(SandboxEnvironmentProvider::namespace_with_agent_stderr(
                base,
                inherit_agent_stderr,
                hand_factory,
                namespace_hand_bin,
                hand_idle_after,
            ))
        }
        SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => None,
    })
}

/// Select the local Session environment tier and the TrustedHost backend from
/// the Host's typed Deployment, validating `namespace_hand_path` whenever the
/// Namespace tier or TrustedHost execution needs the Hand companion. An
/// already-installed authoritative provider is preserved; the independent,
/// operator-enabled TrustedHost backend is still completed.
pub async fn select_local_session_environment(
    host: SharedHost,
    hand_factory: Option<Arc<dyn HandExecutorFactory>>,
    namespace_hand_path: PathBuf,
) -> Result<SharedHost, String> {
    let deployment: DeploymentConfig = host.deployment_config().clone();
    deployment.validate_local_execution()?;
    let explicit = host.session_environment_provider_installed();
    if explicit && !deployment.trusted_host_enabled() {
        return Ok(host);
    }
    let base = awaken_session_executor::session_sandbox_base(&deployment);
    let tier = if explicit {
        deployment.sandbox_tier
    } else {
        resolve_sandbox_tier(
            deployment.sandbox_tier,
            deployment.sandbox.allow_local_fallback,
            &base,
        )
        .await
        .map_err(|error| format!("configure the Session sandbox tier: {error}"))?
    };
    // Namespace and TrustedHost validate and install the same local companion.
    let trusted_hand_factory = if deployment.trusted_host_enabled() {
        Some(
            hand_factory
                .clone()
                .ok_or("TrustedHost Native execution requires the installed Hand factory")?,
        )
    } else {
        None
    };
    if (!explicit && tier == SandboxTier::Namespace) || trusted_hand_factory.is_some() {
        validate_namespace_hand_bin(&namespace_hand_path).await?;
    }
    let namespace_hand_bin = namespace_hand_path.to_string_lossy().into_owned();
    let hand_idle_after =
        std::time::Duration::from_secs(deployment.sandbox.container_hand_idle_secs);
    let mut host = host;
    if deployment.trusted_host_enabled() {
        // The TrustedHost backend is configured completely before the Host
        // holds it: an installed backend cannot gain its Hand or scoped
        // execution afterwards, so it must not exist yet when either is needed.
        if host.backend_owned_environment_provider_installed() {
            return Err(
                "configure TrustedHost execution: its backend was installed before the \
                 deployment selected its Hand; select the Session environment before binding ACP"
                    .into(),
            );
        }
        let mut provider = SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(
            base.join("trusted-host"),
            deployment.sandbox.inherit_agent_stderr,
        );
        if let Some(factory) = trusted_hand_factory {
            provider.install_workdir_hand(factory, namespace_hand_bin.clone(), hand_idle_after);
        }
        #[cfg(target_os = "linux")]
        if deployment.sandbox.trusted_host_scoped_execution {
            let execution =
                awaken_sandbox_local::LocalExecutionDeployment::discover(namespace_hand_path)
                    .map_err(|error| format!("configure TrustedHost execution: {error}"))?;
            provider
                .install_scoped_execution(execution)
                .await
                .map_err(|error| format!("configure TrustedHost execution: {error}"))?;
        }
        host = host.with_backend_owned_environment_provider(provider.into_session_provider());
    }
    if !explicit {
        let Some(provider) = local_tier_provider(
            tier,
            base,
            deployment.sandbox.inherit_agent_stderr,
            hand_factory,
            namespace_hand_bin,
            hand_idle_after,
        )?
        else {
            // The container tiers are assembled by `install_container_environment`,
            // which runs before this selection. Reaching here means a process
            // root skipped it; fail closed instead of running the Session
            // outside its configured isolation.
            return Err(format!(
                "configure the Session sandbox tier: the {tier:?} container tier is installed by \
                 the execution composition, which this process did not run"
            ));
        };
        host = host.with_session_environment_provider(provider.into_session_provider());
    }
    Ok(host)
}

/// Install the TrustedHost Workdir backend that BackendOwned ACP candidates
/// realize in, unless startup already installed one. Consumers: process
/// roots that bind ACP without enabling TrustedHost Native execution.
#[must_use]
pub fn with_backend_owned_environment(host: SharedHost) -> SharedHost {
    if host.backend_owned_environment_provider_installed() {
        return host;
    }
    let deployment = host.deployment_config();
    let provider = SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(
        awaken_session_executor::session_sandbox_base(deployment).join("backend-owned"),
        deployment.sandbox.inherit_agent_stderr,
    );
    host.with_backend_owned_environment_provider(provider.into_session_provider())
}

/// Install the cheap Workdir adapter under the Deployment's sandbox root as a
/// non-authoritative fallback: Deployment selection replaces it. Consumers:
/// test and Scenario compositions whose deterministic fixtures run without
/// Deployment selection.
#[must_use]
pub fn with_workdir_fallback(host: SharedHost) -> SharedHost {
    let base = awaken_session_executor::session_sandbox_base(host.deployment_config());
    host.with_fallback_session_environment_provider(
        SandboxEnvironmentProvider::workdir(base).into_session_provider(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_sandbox_tier_passes_non_namespace_tiers_through_unprobed() {
        // Only the namespace tier is probed; the rest resolve to themselves.
        let base = std::env::temp_dir().join(format!("awaken-tier-ut-{}", std::process::id()));
        assert_eq!(
            resolve_sandbox_tier(SandboxTier::Local, true, &base)
                .await
                .unwrap(),
            SandboxTier::Local
        );
        assert_eq!(
            resolve_sandbox_tier(SandboxTier::Docker, true, &base)
                .await
                .unwrap(),
            SandboxTier::Docker
        );
    }

    #[tokio::test]
    async fn resolve_sandbox_tier_never_errors_for_namespace_with_the_local_fallback_optin() {
        // Cause/effect decision table:
        // | Namespace ready | typed fallback | Effect |
        // | yes             | either         | Namespace |
        // | no              | true           | Local |
        // The assertion covers either host state without ambient configuration.
        let base = std::env::temp_dir().join(format!("awaken-tier-ut-{}", std::process::id()));
        let resolved = resolve_sandbox_tier(SandboxTier::Namespace, true, &base).await;
        assert!(matches!(
            resolved,
            Ok(SandboxTier::Namespace | SandboxTier::Local)
        ));
    }

    #[test]
    fn namespace_degrade_policy_requires_an_explicit_unsafe_optin() {
        // Cause/effect decision table:
        // | Namespace ready | typed fallback | Effect |
        // | no              | false          | reject |
        // | no              | true           | Local |
        // Readiness is tested at the impure caller; this covers the pure policy.
        assert!(
            !namespace_degrades_to_local(false),
            "namespace fails closed without an explicit fallback opt-in"
        );
        assert!(
            namespace_degrades_to_local(true),
            "the explicit unsafe opt-in permits local fallback"
        );
    }

    #[test]
    fn namespace_companion_resolution_preserves_release_and_cargo_test_layouts() {
        // Cause/effect table: C1 a release executable and companion are siblings
        // -> E1 keep the sibling path; C2 Cargo runs a test from `debug/deps`
        // or an example from `debug/examples` while the built companion is in
        // `debug` -> E2 use that profile-level companion. A missing profile
        // companion falls back to the release sibling so validation owns the
        // one fail-closed error path.
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let companion_name = format!("awaken-sandbox{}", std::env::consts::EXE_SUFFIX);
        let companion = profile.join(&companion_name);
        std::fs::write(&companion, []).unwrap();

        assert_eq!(
            namespace_hand_path(&profile.join("awaken")),
            profile.join(&companion_name),
            "C1/E1"
        );
        assert_eq!(
            namespace_hand_path(&deps.join("awaken-test-hash")),
            companion,
            "C2 test/E2"
        );
        assert_eq!(
            namespace_hand_path(&profile.join("examples/awaken-example")),
            companion,
            "C2 example/E2"
        );
        std::fs::remove_file(&companion).unwrap();
        assert_eq!(
            namespace_hand_path(&deps.join("awaken-test-hash")),
            deps.join(companion_name),
            "missing profile companion preserves fail-closed sibling validation"
        );
    }
}

/// Deployment-driven startup rows. The ignored rows need the just-built Hand
/// companion (`AWAKEN_TEST_SCOPED_HELPER`) and are mandatory in the
/// trusted-host portability gate.
#[cfg(test)]
mod startup_tests {
    use super::*;

    struct NoLlm;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _r: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("the startup test never runs a Run")
        }
    }

    struct FixedModel;

    #[async_trait::async_trait]
    impl awaken_run_executor_acp::LaunchResolver for FixedModel {
        async fn model(
            &self,
            _activation: &awaken_runtime_contract::RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, awaken_run_executor_acp::OpenError>
        {
            Ok(awaken_run_executor_acp::ResolvedModel::Managed {
                base_url: "http://example.invalid".into(),
                model: "test".into(),
                process_secret: None,
                credential_artifact: None,
                acp: None,
                provider_server_tools: Vec::new(),
            })
        }
    }

    struct NoHandFactory;

    impl HandExecutorFactory for NoHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
            _operation_scope: &str,
            _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
        ) -> Arc<dyn awaken_runtime_contract::tool::ToolExecutor> {
            panic!("the startup test never opens a Hand")
        }
    }

    fn deployment_test_binary(key: &str) -> PathBuf {
        let path = PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("required {key}")));
        assert!(path.is_absolute() && path.is_file(), "invalid {key}");
        path
    }

    fn host(deployment: DeploymentConfig) -> SharedHost {
        awaken_runtime_host_testkit::volatile_host_with_deployment(
            Arc::new(NoLlm),
            "test",
            deployment,
        )
    }

    fn trusted_host_provider(host: &SharedHost) -> &SandboxEnvironmentProvider {
        host.installed_backend_owned_environment_provider()
            .expect("TrustedHost provider installed")
            .origin()
            .downcast_ref::<SandboxEnvironmentProvider>()
            .expect("a sandbox-backed TrustedHost provider")
    }

    /// P1: Sandbox tier selection is caused by Deployment, not by ACP presence.
    /// A Native-only Worker must install the selected provider before serving.
    #[tokio::test]
    #[ignore = "requires installed Hand companion; mandatory in trusted-host portability gate"]
    async fn native_only_deployment_selects_the_session_provider() {
        let mut deployment = DeploymentConfig::ephemeral();
        deployment.sandbox_tier = SandboxTier::Local;
        deployment.acp = None;
        let expected = deployment.sandbox_support().0;

        let host = select_local_session_environment(
            host(deployment),
            Some(Arc::new(NoHandFactory)),
            deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
        )
        .await
        .unwrap();
        let host = crate::with_acp_from_deployment(host, None, None);

        assert!(host.session_environment_provider_installed());
        assert_eq!(
            host.installed_session_environment_provider()
                .realizer()
                .capabilities(),
            expected
        );
    }

    #[tokio::test]
    #[ignore = "requires installed Hand companion; mandatory in trusted-host portability gate"]
    async fn trusted_host_hand_startup_covers_prior_acp_and_explicit_primary_orderings() {
        // Cause/effect table: S1 ACP provider absent/present; S2 primary
        // implicit/explicit; S3 TrustedHost enabled. All four S1*S2 rules with
        // S3 must install ONE mandatory Hand on the trusted provider, retaining
        // primary capabilities. No tool launch is needed: real CLI/Hand/resource
        // execution lives in the E2E gate. S7 scoped={off,on Linux} crosses both
        // startup orderings, probes the real binary and preserves root
        // relocation. S8 Cargo's test and binary directories may differ: supply
        // its exact artifact to the SAME startup owner, never copy/search
        // another binary. A second selection over an installed TrustedHost
        // backend is refused rather than silently reconfigured.
        let modes: &[bool] = if cfg!(target_os = "linux") {
            &[false, true]
        } else {
            &[false]
        };
        for &scoped in modes {
            for prior_acp in [false, true] {
                for explicit_primary in [false, true] {
                    let base = tempfile::tempdir().unwrap();
                    let mut deployment = DeploymentConfig::ephemeral();
                    deployment.sandbox_tier = SandboxTier::Local;
                    deployment.sandbox_dir = Some(base.path().into());
                    deployment.sandbox.trusted_host_scoped_execution = scoped;
                    let mut host = host(deployment);
                    if prior_acp {
                        host = crate::with_projected_acp(
                            host,
                            *awaken_run_executor_acp::acp_cli("claude").unwrap(),
                            Arc::new(FixedModel),
                            None,
                        );
                    }
                    if explicit_primary {
                        host = host.with_session_environment_provider(
                            SandboxEnvironmentProvider::workdir(base.path().join("primary"))
                                .into_session_provider(),
                        );
                    }
                    let capabilities = host
                        .installed_session_environment_provider()
                        .realizer()
                        .capabilities();
                    let host = select_local_session_environment(
                        host,
                        Some(Arc::new(NoHandFactory)),
                        deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
                    )
                    .await
                    .unwrap();
                    #[cfg(target_os = "linux")]
                    {
                        let provider = trusted_host_provider(&host);
                        for provider in [provider, &provider.at_root(base.path().join("relocated"))]
                        {
                            assert_eq!(provider.has_scoped_execution(), scoped);
                        }
                    }
                    assert!(trusted_host_provider(&host).has_workdir_hand());
                    assert_eq!(
                        host.installed_session_environment_provider()
                            .realizer()
                            .capabilities(),
                        capabilities
                    );
                    assert!(host.session_environment_provider_installed());
                    let reentry = select_local_session_environment(
                        host,
                        Some(Arc::new(NoHandFactory)),
                        deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
                    )
                    .await
                    .err()
                    .expect("an installed TrustedHost backend is never reconfigured");
                    assert!(reentry.contains("installed before"), "{reentry}");
                }
            }
        }
    }

    #[tokio::test]
    async fn trusted_host_startup_rejects_missing_factory_and_invalid_companion() {
        // S4 enabled+no factory => fail before launching a companion. S5 missing
        // or directory companion => fail; S6 executable without Hand --check
        // support => fail. There is no successful Local executor substitution.
        // Configuration failure is a returned error, not an unwind: callers
        // must still be able to withdraw an already-created Worker registration.
        // S5/S6 enter the same startup owner with an explicit artifact, proving
        // artifact injection cannot bypass capability checks or fall back to a
        // different executable.
        let mut deployment = DeploymentConfig::ephemeral();
        deployment.sandbox_tier = SandboxTier::Local;
        let message = select_local_session_environment(
            host(deployment),
            None,
            installed_namespace_hand_path(),
        )
        .await
        .err()
        .expect("missing factory must reject");
        assert!(message.contains("requires the installed Hand factory"));
        let root = tempfile::tempdir().unwrap();
        for (artifact, reason) in [
            (root.path().join("missing"), "require the release companion"),
            (root.path().to_path_buf(), "companion is not a file"),
            (std::env::current_exe().unwrap(), "companion check failed"),
        ] {
            let mut deployment = DeploymentConfig::ephemeral();
            deployment.sandbox_tier = SandboxTier::Local;
            let error = select_local_session_environment(
                host(deployment),
                Some(Arc::new(NoHandFactory)),
                artifact,
            )
            .await
            .err()
            .expect("S5/S6 explicit invalid artifact must fail startup");
            assert!(error.contains(reason), "S5/S6 expected {reason}: {error}");
        }
    }
}
