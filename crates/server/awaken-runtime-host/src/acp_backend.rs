// @formal entrypoint worker.acp_capability_is_detected_only_after_a_verified_handshake
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! The Host-owned ACP launch observer, factory installation, and shared sandbox
//! root. Concrete ACP process assembly lives in Worker execution composition.

use std::sync::Arc;

use awaken_acp_contract::LaunchObserver;

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn deployment_test_binary(key: &str) -> std::path::PathBuf {
    let path =
        std::path::PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("required {key}")));
    assert!(path.is_absolute() && path.is_file(), "invalid {key}");
    path
}

/// Bridge ACP bring-up observations onto this Host's existing thread event hub.
pub(crate) struct HubLaunchObserver {
    hub: Arc<crate::hub::ThreadEventHub>,
}

impl HubLaunchObserver {
    pub(crate) fn new(hub: Arc<crate::hub::ThreadEventHub>) -> Self {
        Self { hub }
    }
}

impl LaunchObserver for HubLaunchObserver {
    fn on_launch(&self, scope: &str, event: &awaken_acp_contract::AcpLaunchEvent) {
        use awaken_acp_contract::AcpLaunchStage::*;
        let stage = match event.stage {
            Launching => "launching",
            Initializing => "initializing",
            Ready => "ready",
            Failed => "failed",
        };
        let protocol_step = event.detail.as_deref().filter(|step| {
            matches!(
                *step,
                "initialize" | "session/new" | "session/load" | "session/prompt"
            )
        });
        tracing::info!(scope, stage, protocol_step, "ACP launch progress");
        self.hub.publish(
            scope,
            crate::hub::ThreadEvent::AgentLaunch {
                stage: stage.to_string(),
                detail: event.detail.clone(),
            },
        );
    }
}

impl crate::host::SharedHost {
    pub(crate) fn acp_adapter_policy(
        &self,
    ) -> Result<
        &dyn awaken_acp_contract::AcpAdapterPolicy<
            awaken_runtime_contract::resolved::ProviderServerTool,
        >,
        crate::HostError,
    > {
        self.exec
            .acp
            .as_ref()
            .map(|factory| factory.adapter_policy())
            .or(self.exec.acp_projection_policy)
            .ok_or_else(|| crate::HostError::bad_request("ACP backend is not installed"))
    }

    #[cfg(test)]
    pub(crate) fn register_thread_backend_projection(&self, thread: &str, backend_ref: &str) {
        self.installed_sessions.replace_frozen_with(thread, |next| {
            next.backend_ref = crate::frozen_inputs::backend_projection(backend_ref);
        });
    }
}

/// Install the existing ACP executor's policy/factory port for Host unit tests
/// that exercise ACP projection without launching a real CLI process.
#[cfg(test)]
pub(crate) fn with_fixture_acp(dependencies: crate::HostDependencies) -> crate::HostDependencies {
    let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    dependencies.with_acp(Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(
        source,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_sandbox_base;

    struct NoLlm;

    #[test]
    fn session_sandbox_root_has_one_precedence_for_build_and_tier_selection() {
        // Cause/effect graph: C1=explicit sandbox root; C2=durable runtime root.
        // Effects: E1=use explicit operator root; E2=derive stable sandboxes
        // below runtime storage; E3=use process-temporary root only for a fully
        // ephemeral host. Constraint: C1 has precedence over C2. C3=an earlier
        // process with the same pid left its root; E4=E3 never reuses it and
        // stays one root for this whole process.
        //
        // Decision table: S1 C1+C2=>E1; S2 !C1+C2=>E2; S3 !C1+!C2=>E3;
        // S4 !C1+!C2+C3=>E3+E4.
        let mut deployment = crate::DeploymentConfig::ephemeral();
        deployment.storage_dir = Some("/durable/runtime".into());
        deployment.sandbox_dir = Some("/operator/sandboxes".into());
        assert_eq!(
            session_sandbox_base(&deployment),
            std::path::PathBuf::from("/operator/sandboxes"),
            "S1"
        );

        deployment.sandbox_dir = None;
        assert_eq!(
            session_sandbox_base(&deployment),
            std::path::PathBuf::from("/durable/runtime/sandboxes"),
            "S2"
        );

        deployment.storage_dir = None;
        assert!(
            session_sandbox_base(&deployment)
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("awaken-acp-sbx-")),
            "S3"
        );
        let pid_only = std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()));
        let root = session_sandbox_base(&deployment);
        assert_ne!(
            root, pid_only,
            "S4 a root keyed by the pid alone is never reused"
        );
        assert_eq!(
            root,
            session_sandbox_base(&deployment),
            "S4 one root per process"
        );
    }
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _r: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("the builder test never runs a Run")
        }
    }

    #[tokio::test]
    async fn coordinator_catalog_policy_never_installs_an_acp_executor() {
        // Causes: C1 no ACP installation, a projection-only catalog, or an
        // executor factory; C2 canonical, alias, or unknown CLI id. Effects:
        // E1 absent installation refuses policy; E2 projection-only and
        // executor roles read the same catalog instance; E3 only the executor
        // role can launch; E4 only canonical ids pass. Decision rules:
        // P1=absent=>E1; P2=projection-only=>E2+!E3+E4;
        // P3=executor=>E2+E3+E4; P4=alias|unknown=>!E4 for P2/P3.
        let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await)
            .build()
            .expect("validated Host dependencies");
        assert!(
            host.acp_adapter_policy()
                .err()
                .expect("P1 absent policy")
                .to_string()
                .contains("not installed"),
            "P1"
        );
        let projection_only = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
            .await
            .with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy());
        let host = (projection_only)
            .build()
            .expect("validated Host dependencies");
        assert!(host.exec.acp.is_none(), "P2/E3 no execution factory");
        assert!(
            std::ptr::eq(
                host.acp_adapter_policy().expect("P2 policy"),
                awaken_run_executor_acp::acp_adapter_policy()
            ),
            "P2/E2 one catalog instance"
        );
        let host = (with_fixture_acp(
            crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
                .await
                .with_acp_projection_policy(awaken_run_executor_acp::acp_adapter_policy()),
        ))
        .build()
        .expect("validated Host dependencies");
        assert!(
            host.exec.acp_projection_policy.is_none(),
            "P3 one effective policy owner"
        );
        let policy = host.acp_adapter_policy().expect("P3 installed policy");
        assert!(
            std::ptr::eq(policy, awaken_run_executor_acp::acp_adapter_policy()),
            "P3/E2 one catalog instance"
        );
        assert!(policy.contains("codex"), "P3/E4 canonical");
        assert!(!policy.contains("CODEX"), "P4 alias");
        assert!(!policy.contains("unknown-cli"), "P4 unknown");
    }

    #[tokio::test]
    async fn with_acp_factory_wires_only_frozen_acp_thread_projections() {
        // Cause graph: wiring an ACP executor provides execution capability only;
        // a frozen Session projection supplies the backend fact. Neither creates a
        // deployment-wide default.
        //
        // | Rule | Executor wired | Session projection | ACP selected |
        // | A1 | yes | acp:claude | yes |
        // | A2 | yes | absent | no |
        let host =
            (with_fixture_acp(crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await))
                .build()
                .expect("validated Host dependencies");
        host.register_thread_backend_projection("t", "acp:claude");
        assert!(host.exec.acp.is_some(), "A1 executor");
        assert_eq!(
            host.installed_sessions
                .realizations()
                .frozen("t")
                .expect("installed Sessions")
                .backend_ref
                .clone()
                .as_deref(),
            Some("acp:claude"),
            "A1"
        );
        assert_eq!(
            host.installed_sessions
                .realizations()
                .frozen("unstaged-thread")
                .expect("installed Sessions")
                .backend_ref
                .clone(),
            None,
            "A2"
        );
    }

    #[tokio::test]
    async fn provisioning_selects_one_session_environment_policy() {
        // Causes: immutable provisioning is BackendOwned, Provider, or
        // HostExecutor. Effects: BackendOwned receives the trusted Workdir
        // provider; all managed/in-process variants retain the configured tier.
        //
        // | Rule | Provisioning | Selected provider |
        // | E1 | BackendOwned | host identity |
        // | E2 | Provider | configured Namespace |
        // | E3 | HostExecutor | configured Namespace |
        let mut dependencies =
            with_fixture_acp(crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await);
        let backend_owned = awaken_session_environment::SandboxEnvironmentProvider::
            trusted_host_workdir_with_agent_stderr(
                dependencies.env.sandbox_root.join("backend-owned"),
                false,
            )
            .into_session_provider();
        dependencies = dependencies.with_backend_owned_environment_provider(backend_owned);
        dependencies.env.session_provider =
            awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
                std::env::temp_dir().join("awaken-managed-environment"),
                false,
                Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_session_provider();
        let host = (dependencies).build().expect("validated Host dependencies");
        let backend_owned = awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
            credential: awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            model_selection: awaken_runtime_contract::resolved::BackendModelSelection::Default,
            acp: awaken_runtime_contract::resolved::AcpExecutionProfile {
                supports_tool_aliases: false,
                capability_adapter_version: "test".into(),
                capability_fingerprint: "sha256:test-capability".into(),
                session_configuration: Default::default(),
            },
        };
        let provider = awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            provider_ref: "provider".into(),
            route_ref: "route".into(),
            access_kind: awaken_runtime_contract::resolved::ProviderAccessKind::Direct,
            scope_id: "workspace".into(),
            credential: None,
            endpoint: Box::new(awaken_runtime_contract::resolved::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "responses".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: "model".into(),
                processing_placement: None,
            }),
            unspecified_reasoning: Default::default(),
            acp: None,
        };

        assert!(
            host.realization_executor
                .environments()
                .session_environment_provider(&backend_owned)
                .expect("E1")
                .realizer()
                .supports_host_identity(),
            "E1"
        );
        for (rule, provisioning) in [
            ("E2", &provider),
            (
                "E3",
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            ),
        ] {
            assert!(
                !host
                    .realization_executor
                    .environments()
                    .session_environment_provider(provisioning)
                    .unwrap_or_else(|_| panic!("{rule}"))
                    .realizer()
                    .supports_host_identity(),
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn hub_launch_observer_republishes_lifecycle_to_the_thread_hub() {
        use awaken_run_executor_acp::{AcpLaunchEvent, AcpLaunchStage};

        let dependencies = crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await;
        let observer = dependencies.acp_launch_observer();
        let host = (dependencies).build().expect("validated Host dependencies");
        // A UI-facing observer subscribes to the thread before the agent starts.
        let mut sub = host.hub().subscribe("thread-42");

        observer.on_launch(
            "thread-42",
            &AcpLaunchEvent::with_detail(AcpLaunchStage::Launching, "claude-agent-acp"),
        );
        observer.on_launch("thread-42", &AcpLaunchEvent::stage(AcpLaunchStage::Ready));

        // The launch event (with detail) then the ready event arrive on the hub,
        // scoped to the thread — exactly what a per-session UI channel renders.
        match tokio::time::timeout(std::time::Duration::from_secs(30), sub.recv())
            .await
            .expect("timed out waiting for sub.recv()")
            .unwrap()
        {
            crate::hub::ThreadEvent::AgentLaunch { stage, detail } => {
                assert_eq!(stage, "launching");
                assert_eq!(detail.as_deref(), Some("claude-agent-acp"));
            }
            other => panic!("expected AgentLaunch, got {other:?}"),
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), sub.recv())
            .await
            .expect("timed out waiting for sub.recv()")
            .unwrap()
        {
            crate::hub::ThreadEvent::AgentLaunch { stage, .. } => assert_eq!(stage, "ready"),
            other => panic!("expected AgentLaunch ready, got {other:?}"),
        }
    }
}
