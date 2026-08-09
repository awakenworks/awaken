//! ACP execution for the managed host (R3/R4). Backend selection belongs to the
//! immutable Agent publication and its frozen Session projection; this module
//! owns only the executor that drives an already-selected ACP backend.

use std::sync::Arc;

use awaken_run_executor_acp::{AcpRunExecutor, LaunchObserver, SessionHomeProvider};

enum AcpExecutorSource {
    Static(Arc<AcpRunExecutor>),
    Bound {
        launch: Box<crate::LaunchSource>,
        observer: Option<Arc<dyn LaunchObserver>>,
        session_home: Option<Arc<dyn SessionHomeProvider>>,
    },
}

/// Holds the ACP executor. Per-Session backend projections live in the common
/// Session slot because Native, ACP, and A2A share that domain fact.
pub(crate) struct AcpBackend {
    source: AcpExecutorSource,
}

impl AcpBackend {
    pub(crate) fn credential_realization_capabilities(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, String> {
        match &self.source {
            AcpExecutorSource::Static(_) => Ok(Default::default()),
            AcpExecutorSource::Bound { launch, .. } => launch
                .credential_realization_capabilities(backend)
                .map_err(|error| error.to_string()),
        }
    }

    pub(crate) fn new(executor: Arc<AcpRunExecutor>) -> Self {
        Self {
            source: AcpExecutorSource::Static(executor),
        }
    }

    fn bound(
        launch: crate::LaunchSource,
        observer: Option<Arc<dyn LaunchObserver>>,
        session_home: Option<Arc<dyn SessionHomeProvider>>,
    ) -> Self {
        Self {
            source: AcpExecutorSource::Bound {
                launch: Box::new(launch),
                observer,
                session_home,
            },
        }
    }

    /// Materialize the executor for this session. Production local ACP uses a
    /// source bound to the session's existing sandbox; injected/static executors
    /// remain available for remote adapters and deterministic tests.
    pub(crate) fn executor_for(
        &self,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        backend: awaken_runtime_contract::resolved::Backend,
        mcp_servers: Vec<awaken_run_executor_acp::McpServerConfig>,
    ) -> Arc<AcpRunExecutor> {
        match &self.source {
            AcpExecutorSource::Static(executor) => {
                Arc::new(executor.for_session(permission, &mcp_servers))
            }
            AcpExecutorSource::Bound {
                launch,
                observer,
                session_home,
            } => {
                let source = Arc::new(
                    crate::sandbox_source::BoundLocalChannelSource::from_environment(
                        sandbox,
                        launch.as_ref().clone(),
                        backend,
                        mcp_servers,
                    ),
                );
                let mut executor = AcpRunExecutor::new(source);
                if let Some(observer) = observer {
                    executor = executor.with_launch_observer(observer.clone());
                }
                if let Some(session_home) = session_home {
                    executor = executor.with_session_home(session_home.clone());
                }
                Arc::new(executor.with_permission_policy(permission))
            }
        }
    }
}

/// A [`LaunchObserver`] that republishes ACP agent bring-up onto the per-thread
/// [`ThreadEventHub`], so any protocol adapter observing the thread projects the
/// progress to its UI as a "starting agent…" affordance.
/// The launch scope is the thread key the hub routes on.
pub(crate) struct HubLaunchObserver {
    hub: Arc<crate::hub::ThreadEventHub>,
}

impl HubLaunchObserver {
    pub(crate) fn new(hub: Arc<crate::hub::ThreadEventHub>) -> Self {
        Self { hub }
    }
}

impl awaken_run_executor_acp::LaunchObserver for HubLaunchObserver {
    fn on_launch(&self, scope: &str, event: &awaken_run_executor_acp::AcpLaunchEvent) {
        use awaken_run_executor_acp::AcpLaunchStage::*;
        let stage = match event.stage {
            Launching => "launching",
            Initializing => "initializing",
            Ready => "ready",
            Failed => "failed",
        };
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
    /// Serve `acp:*` sessions on `executor` (R3/R4). Threads select it via the API.
    /// To publish bring-up progress to a UI, build the executor with
    /// `.with_launch_observer(host.acp_launch_observer())` before passing it here.
    pub fn with_acp(mut self, executor: Arc<AcpRunExecutor>) -> Self {
        self.acp = Some(Arc::new(AcpBackend::new(executor)));
        self
    }

    fn with_bound_acp(
        mut self,
        launch: crate::LaunchSource,
        session_home: Option<Arc<dyn SessionHomeProvider>>,
    ) -> Self {
        if self.backend_owned_session_provider.is_none() {
            let provider =
                crate::session_environment::SessionEnvironmentProvider::workdir_with_agent_stderr(
                    session_sandbox_base(&self.deployment).join("backend-owned"),
                    self.deployment.sandbox.inherit_agent_stderr,
                );
            if let Some(mounter) = self.memory_mounter() {
                provider.install_memory_mounter(mounter);
            }
            self.backend_owned_session_provider = Some(provider);
        }
        let observer = self.acp_launch_observer();
        self.acp = Some(Arc::new(AcpBackend::bound(
            launch,
            Some(observer),
            session_home,
        )));
        self
    }

    /// Wire the ACP backend from deployment capability discovery plus persisted,
    /// publication-pinned provider access. This is the ONE place both the server and
    /// worker roots configure ACP, so they never drift.
    ///
    /// The typed deployment profile advertises every CLI installed on this worker;
    /// the run's published `acp:<cli>` binding remains authoritative and must match. Provider
    /// endpoint/model/credential data come only from `credentials` and the snapshot.
    /// Neither capability set → no ACP backend served. Panics on an advertised ACP
    /// capability without a credential materializer or on a misconfigured tier.
    pub async fn with_acp_from_deployment(
        self,
        credentials: Option<crate::PinnedCredentialMaterializer>,
    ) -> Self {
        let deployment = self.deployment.clone();
        let Some(profile) = deployment.acp.as_ref() else {
            return match credentials {
                Some(credentials) => self.with_session_secret_broker(Arc::new(credentials)),
                None => self,
            };
        };
        let base = session_sandbox_base(&deployment);
        let credentials = credentials.unwrap_or_else(|| {
            panic!("configured ACP CLIs require persisted credential materialization stores")
        });
        let routes = profile
            .cli_ids()
            .map(|id| {
                let cli = *awaken_run_executor_acp::acp_cli(id)
                    .expect("AcpWorkerProfile validates every CLI");
                let resolver = Arc::new(crate::PublishedAcpLaunchResolver::new(
                    cli,
                    Some(base.clone()),
                    credentials.clone(),
                ));
                (
                    cli,
                    resolver as Arc<dyn awaken_run_executor_acp::LaunchResolver>,
                    profile.launch_argv(id).map(<[String]>::to_vec),
                )
            })
            .collect();
        let default = profile.default_cli().map(str::to_string);
        let source = crate::LaunchSource::Projected(
            crate::AcpLaunchRegistry::with_resolved_argv(routes, default)
                .unwrap_or_else(|error| panic!("configure ACP launch routes: {error}")),
        );
        // The same exact, claim-fenced materializer owns both sides of the
        // last-mile seam: the resolver issues an opaque one-shot reference and
        // the selected sandbox asks it for bytes immediately before spawn.
        self.with_bound_acp(source, None)
            .with_session_secret_broker(Arc::new(credentials))
    }

    /// Select the Session environment once from the typed Deployment, regardless
    /// of whether this Host serves Native, provider, or ACP model execution.
    /// Model backend selection must not create a parallel sandbox-tier decision.
    pub async fn with_session_environment_from_deployment(
        self,
        hand_factory: Option<Arc<dyn crate::HandExecutorFactory>>,
    ) -> Self {
        if self.session_provider_explicit {
            return self;
        }
        let deployment = self.deployment.clone();
        let base = session_sandbox_base(&deployment);
        let tier = crate::resolve_sandbox_tier(
            deployment.sandbox_tier,
            deployment.sandbox.allow_local_fallback,
            &base,
        )
        .await
        .unwrap_or_else(|error| panic!("configure the Session sandbox tier: {error}"));
        let mut host = self;
        host.session_provider = if let Some(provider) =
            crate::session_environment::SessionEnvironmentProvider::for_host_tier(
                tier,
                base,
                deployment.sandbox.inherit_agent_stderr,
            ) {
            provider
        } else {
            let hand_factory = hand_factory.unwrap_or_else(|| {
                panic!("container Session environments require a hand executor factory")
            });
            let (provider, extra_mounts) = crate::container_environment::build(
                tier,
                deployment.container_image.as_deref(),
                &deployment.sandbox,
            )
            .await
            .unwrap_or_else(|error| panic!("configure the Session sandbox tier: {error}"));
            crate::session_environment::SessionEnvironmentProvider::container_with_hand_idle(
                provider,
                extra_mounts,
                hand_factory,
                deployment.sandbox.container_hand_bin.clone(),
                std::time::Duration::from_secs(deployment.sandbox.container_hand_idle_secs),
            )
        };
        host.session_provider_explicit = true;
        if let Some(mounter) = host.memory_mounter() {
            host.session_provider.install_memory_mounter(mounter);
        }
        host
    }

    /// Realize an explicitly supplied ACP launch source in the deployment's sandbox
    /// tier. Product code supplies a projected source backed by published access;
    /// deterministic dev fixtures may supply [`crate::LaunchSource::Fixed`] directly.
    pub async fn with_acp_launch_source(
        self,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
        source: crate::LaunchSource,
    ) -> Self {
        self.with_session_environment_from_deployment(Some(hand_factory))
            .await
            .with_bound_acp(source, None)
    }

    /// The hub-backed launch observer for this host: republishes an ACP agent's
    /// bring-up (install → launch → initialize → ready → failed) onto the per-thread
    /// hub, so a composition root wires it onto the [`AcpRunExecutor`] it builds and
    /// any protocol adapter observing the thread can render progress.
    #[must_use]
    pub fn acp_launch_observer(&self) -> Arc<dyn awaken_run_executor_acp::LaunchObserver> {
        Arc::new(HubLaunchObserver::new(self.hub.clone()))
    }

    /// Serve `acp:*` sessions on an explicitly supplied projecting resolver. This is
    /// the test/dev composition seam; production uses
    /// [`with_acp_from_deployment`](Self::with_acp_from_deployment).
    #[must_use]
    pub fn with_projected_acp(
        self,
        cli: awaken_run_executor_acp::AcpCli,
        resolver: Arc<dyn awaken_run_executor_acp::LaunchResolver>,
        store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // A projected resolver owns both the opaque process-secret requirement and
        // its broker. Bound ACP launches from the Session Environment, so install
        // that broker on the same authoritative provider before moving the resolver
        // into the launch registry. This is last-mile composition, not a second
        // materialization path: the resolver still emits only the opaque reference.
        let secret_broker = resolver.secret_broker();
        let source =
            crate::LaunchSource::Projected(crate::AcpLaunchRegistry::single(cli, resolver));
        // When a session-blob root is configured, recover this CLI's session across
        // directories/machines: harvest it to the (shared) root after a run and
        // restore it before the next, keyed by thread+adapter — under the same
        // config home the resolver opens. A `Gateway`/stateless CLI is skipped by the
        // executor's own dispatch; a single-machine host leaves this unset.
        let session_home = self.session_blob_store.clone().map(|blobs| {
            Arc::new(awaken_run_executor_acp::DirSessionHome::new(
                store_dir, blobs,
            )) as Arc<dyn SessionHomeProvider>
        });
        let host = self.with_bound_acp(source, session_home);
        match secret_broker {
            Some(broker) => host.with_session_secret_broker(broker),
            None => host,
        }
    }

    /// Stage the exact backend copied from the frozen Session baseline. This is a
    /// realization cache, not another selection API. `default` is the canonical
    /// Native absence and therefore must not create a redundant second authority
    /// beside the immutable publication/host executor.
    pub(crate) fn register_thread_backend_projection(&self, thread: &str, backend_ref: &str) {
        self.session_slots.update(thread, |slot| {
            slot.backend_ref = (backend_ref != "default").then(|| backend_ref.to_string());
        });
    }
}

/// The base dir the ACP sandbox roots and per-thread config homes live under
/// (`AWAKEN_SANDBOX_DIR`), or a per-process temp dir when unset.
pub(crate) fn session_sandbox_base(deployment: &crate::DeploymentConfig) -> std::path::PathBuf {
    deployment
        .sandbox_dir
        .clone()
        .or_else(|| {
            deployment
                .storage_dir
                .as_ref()
                .map(|storage| storage.join("sandboxes"))
        })
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SharedHost;

    struct NoLlm;

    #[test]
    fn session_sandbox_root_has_one_precedence_for_build_and_tier_selection() {
        // Cause/effect graph: C1=explicit sandbox root; C2=durable runtime root.
        // Effects: E1=use explicit operator root; E2=derive stable sandboxes
        // below runtime storage; E3=use process-temporary root only for a fully
        // ephemeral host. Constraint: C1 has precedence over C2.
        //
        // Decision table: S1 C1+C2=>E1; S2 !C1+C2=>E2; S3 !C1+!C2=>E3.
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
    }
    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _r: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("the builder test never runs a turn")
        }
    }

    struct FixedModel;
    impl awaken_run_executor_acp::LaunchResolver for FixedModel {
        fn model(
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
            })
        }
    }

    struct NoHandFactory;

    impl crate::HandExecutorFactory for NoHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
            _operation_scope: &str,
        ) -> Arc<dyn awaken_runtime_contract::tool::ToolExecutor> {
            panic!("the native-only composition test never opens a container")
        }
    }

    /// P1: Sandbox tier selection is caused by Deployment, not by ACP presence.
    /// A Native-only Worker must install the selected provider before serving.
    #[tokio::test]
    async fn native_only_deployment_selects_the_session_provider() {
        let mut deployment = crate::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = crate::SandboxTier::Local;
        deployment.acp = None;
        let expected = deployment.sandbox_support().0;

        let host = SharedHost::new_with_deployment(Arc::new(NoLlm), "test", deployment)
            .with_session_environment_from_deployment(Some(Arc::new(NoHandFactory)))
            .await
            .with_acp_from_deployment(None)
            .await;

        assert!(host.session_provider_explicit);
        assert_eq!(host.session_provider.capabilities(), expected);
        assert!(host.acp.is_none());
    }

    #[test]
    fn with_projected_acp_wires_an_acp_backend_that_routes_acp_threads() {
        // Cause graph: wiring an ACP executor provides execution capability only;
        // a frozen Session projection supplies the backend fact. Neither creates a
        // deployment-wide default.
        //
        // | Rule | Executor wired | Session projection | ACP selected |
        // | A1 | yes | acp:claude | yes |
        // | A2 | yes | absent | no |
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_projected_acp(
            cli,
            Arc::new(FixedModel),
            None,
        );
        host.register_thread_backend_projection("t", "acp:claude");
        assert!(host.acp.is_some(), "A1 executor");
        assert_eq!(
            host.session_slots
                .read("t", |slot| slot.backend_ref.clone())
                .flatten()
                .as_deref(),
            Some("acp:claude"),
            "A1"
        );
        assert_eq!(
            host.session_slots
                .read("unstaged-thread", |slot| slot.backend_ref.clone())
                .flatten(),
            None,
            "A2"
        );
    }

    #[test]
    fn provisioning_selects_one_session_environment_policy() {
        // Causes: immutable provisioning is BackendOwned, Provider, or
        // HostExecutor. Effects: BackendOwned receives the trusted Workdir
        // provider; all managed/in-process variants retain the configured tier.
        //
        // | Rule | Provisioning | Selected provider |
        // | E1 | BackendOwned | host identity |
        // | E2 | Provider | configured Namespace |
        // | E3 | HostExecutor | configured Namespace |
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let mut host = SharedHost::new(Arc::new(NoLlm), "test").with_projected_acp(
            cli,
            Arc::new(FixedModel),
            None,
        );
        host.session_provider =
            crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
                std::env::temp_dir().join("awaken-managed-environment"),
                false,
            );
        let backend_owned = awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
            credential: awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            model_selection: awaken_runtime_contract::resolved::BackendModelSelection::Default,
            acp: awaken_runtime_contract::resolved::AcpExecutionProfile {
                capability_adapter_version: "test".into(),
                capability_fingerprint: "sha256:test-capability".into(),
                session_configuration: Default::default(),
            },
        };
        let provider = awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            provider_ref: "provider".into(),
            route_ref: "route".into(),
            scope_id: "workspace".into(),
            credential: None,
            endpoint: Box::new(awaken_runtime_contract::resolved::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "responses".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: "model".into(),
            }),
            acp: None,
        };

        assert!(
            host.session_environment_provider(&backend_owned)
                .expect("E1")
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
                    .session_environment_provider(provisioning)
                    .unwrap_or_else(|_| panic!("{rule}"))
                    .supports_host_identity(),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_session_blob_root_composes_a_recovering_acp_backend() {
        // With a session-blob root set, the projecting-ACP composition wires the
        // session-home recovery into the executor and still routes acp:* threads.
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let blobs = std::env::temp_dir().join(format!("acp-blobs-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test")
            .with_session_blob_root(blobs)
            .with_projected_acp(cli, Arc::new(FixedModel), None);
        host.register_thread_backend_projection("t", "acp:claude");
        assert!(host.acp.is_some());
        assert_eq!(
            host.session_slots
                .read("t", |slot| slot.backend_ref.clone())
                .flatten()
                .as_deref(),
            Some("acp:claude")
        );
    }

    #[tokio::test]
    async fn hub_launch_observer_republishes_lifecycle_to_the_thread_hub() {
        use awaken_run_executor_acp::{AcpLaunchEvent, AcpLaunchStage};

        let host = SharedHost::new(Arc::new(NoLlm), "test");
        // A UI-facing observer subscribes to the thread before the agent starts.
        let mut sub = host.hub().subscribe("thread-42");
        let observer = host.acp_launch_observer();

        observer.on_launch(
            "thread-42",
            &AcpLaunchEvent::with_detail(AcpLaunchStage::Launching, "claude-agent-acp"),
        );
        observer.on_launch("thread-42", &AcpLaunchEvent::stage(AcpLaunchStage::Ready));

        // The launch event (with detail) then the ready event arrive on the hub,
        // scoped to the thread — exactly what a per-session UI channel renders.
        match sub.recv().await.unwrap() {
            crate::hub::ThreadEvent::AgentLaunch { stage, detail } => {
                assert_eq!(stage, "launching");
                assert_eq!(detail.as_deref(), Some("claude-agent-acp"));
            }
            other => panic!("expected AgentLaunch, got {other:?}"),
        }
        match sub.recv().await.unwrap() {
            crate::hub::ThreadEvent::AgentLaunch { stage, .. } => assert_eq!(stage, "ready"),
            other => panic!("expected AgentLaunch ready, got {other:?}"),
        }
    }
}
