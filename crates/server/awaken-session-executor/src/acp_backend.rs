// @formal entrypoint worker.acp_capability_is_detected_only_after_a_verified_handshake
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! ACP execution for the managed host (R3/R4). Backend selection belongs to the
//! immutable Agent publication and its frozen Session projection; this module
//! owns only the executor that drives an already-selected ACP backend.

use std::sync::Arc;

use awaken_acp_contract::LaunchObserver;
use awaken_run_executor_acp::{AcpRunExecutor, SessionHomeProvider};

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn deployment_test_binary(key: &str) -> std::path::PathBuf {
    let path =
        std::path::PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("required {key}")));
    assert!(path.is_absolute() && path.is_file(), "invalid {key}");
    path
}

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

    pub(crate) fn child_executor_factory(
        self: &Arc<Self>,
        sandbox: Arc<dyn awaken_session_environment_contract::AgentProcessEnvironment>,
    ) -> crate::agent_runner::ChildAcpExecutorFactory {
        let acp = self.clone();
        Arc::new(move |backend, permission| {
            Ok(Arc::new(acp.executor_for(
                sandbox.clone(),
                permission,
                backend,
                Vec::new(),
                None,
                None,
            )?)
                as Arc<
                    dyn awaken_runtime_contract::execution::RunAttemptExecutor,
                >)
        })
    }

    /// Materialize the executor for this session. Production local ACP uses a
    /// source bound to the session's existing sandbox; injected/static executors
    /// remain available for remote adapters and deterministic tests.
    pub(crate) fn executor_for(
        &self,
        sandbox: Arc<dyn awaken_session_environment_contract::AgentProcessEnvironment>,
        permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        backend: awaken_runtime_contract::resolved::Backend,
        mcp_servers: Vec<awaken_acp_contract::SessionMcpServer>,
        attached_tools: Option<Arc<crate::acp_tool_export::AttachedToolSet>>,
        grant_observer: Option<
            Arc<dyn awaken_runtime_contract::permission::PermissionGrantObserver>,
        >,
    ) -> Result<AcpRunExecutor, String> {
        match &self.source {
            AcpExecutorSource::Static(executor) => {
                if attached_tools.is_some() {
                    return Err(
                        "attached MCP tools require the bound Environment ACP source".into(),
                    );
                }
                Ok(executor.for_session_servers_with_grant_observer(
                    permission,
                    &mcp_servers,
                    grant_observer,
                ))
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
                    )
                    .with_attached_tools(attached_tools),
                );
                let mut executor = AcpRunExecutor::new(source);
                if let Some(observer) = observer {
                    executor = executor.with_launch_observer(observer.clone());
                }
                if let Some(session_home) = session_home {
                    executor = executor.with_session_home(session_home.clone());
                }
                if let Some(observer) = grant_observer {
                    executor = executor.with_permission_grant_observer(observer);
                }
                Ok(executor.with_permission_policy(permission))
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

impl LaunchObserver for HubLaunchObserver {
    fn on_launch(&self, scope: &str, event: &awaken_acp_contract::AcpLaunchEvent) {
        use awaken_acp_contract::AcpLaunchStage::*;
        let stage = match event.stage {
            Launching => "launching",
            Initializing => "initializing",
            Ready => "ready",
            Failed => "failed",
        };
        // Only protocol step names enter Worker diagnostics. Other launch
        // details may contain argv/provider errors and remain on the existing
        // scoped observer surface; never copy prompt/config/credentials to logs.
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
    /// Serve `acp:*` sessions on `executor` (R3/R4). Threads select it via the API.
    /// To publish bring-up progress to a UI, build the executor with
    /// `.with_launch_observer(host.acp_launch_observer())` before passing it here.
    pub fn with_acp(mut self, executor: Arc<AcpRunExecutor>) -> Self {
        self.exec.acp = Some(Arc::new(AcpBackend::new(executor)));
        self
    }

    fn with_bound_acp(
        mut self,
        launch: crate::LaunchSource,
        session_home: Option<Arc<dyn SessionHomeProvider>>,
    ) -> Self {
        // The process composition installs the TrustedHost provider that
        // BackendOwned candidates realize in; without one they fail closed.
        // This crate's own tests keep the Workdir default they were written for.
        #[cfg(test)]
        if self.env.backend_owned_session_provider.is_none() {
            let provider = crate::session_environment::SandboxEnvironmentProvider::
                trusted_host_workdir_with_agent_stderr(
                    session_sandbox_base(&self.deployment).join("backend-owned"),
                    self.deployment.sandbox.inherit_agent_stderr,
                )
                .into_session_provider();
            self = self.with_backend_owned_environment_provider(provider);
        }
        let observer = self.acp_launch_observer();
        self.exec.acp = Some(Arc::new(AcpBackend::bound(
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
    /// Neither capability set → no ACP backend served. A Worker without a
    /// credential materializer serves only backend-owned candidates; provider
    /// candidates fail before launch instead of inventing a credential source.
    pub async fn with_acp_from_deployment(
        self,
        credentials: Option<crate::PinnedCredentialMaterializer>,
        brokered: Option<Arc<dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>>,
    ) -> Self {
        let deployment = self.deployment.clone();
        let Some(profile) = deployment.acp.as_ref() else {
            return match credentials {
                Some(credentials) => self.with_session_secret_broker(Arc::new(credentials)),
                None => self,
            };
        };
        let base = session_sandbox_base(&deployment);
        let routes = profile
            .cli_ids()
            .map(|id| {
                let cli = *awaken_run_executor_acp::acp_cli(id)
                    .expect("AcpWorkerProfile validates every CLI");
                let resolver = Arc::new(match (&credentials, &brokered) {
                    (Some(_), Some(_)) => {
                        panic!("ACP credential materializers are mutually exclusive")
                    }
                    (Some(credentials), None) => crate::PublishedAcpLaunchResolver::new(
                        cli,
                        Some(base.clone()),
                        credentials.clone(),
                    ),
                    (None, Some(materializer)) => crate::PublishedAcpLaunchResolver::brokered(
                        cli,
                        Some(base.clone()),
                        materializer.clone(),
                    ),
                    (None, None) => {
                        crate::PublishedAcpLaunchResolver::backend_owned(cli, Some(base.clone()))
                    }
                });
                (
                    cli,
                    resolver as Arc<dyn awaken_run_executor_acp::LaunchResolver>,
                    profile.launch_argv(id).map(<[String]>::to_vec),
                )
            })
            .collect();
        let source = crate::LaunchSource::Projected(
            crate::AcpLaunchRegistry::with_resolved_argv(routes)
                .unwrap_or_else(|error| panic!("configure ACP launch routes: {error}")),
        );
        // The same exact, claim-fenced materializer owns both sides of the
        // last-mile seam: the resolver issues an opaque one-shot reference and
        // the selected sandbox asks it for bytes immediately before spawn.
        let host = self.with_bound_acp(source, None);
        match credentials {
            Some(credentials) => host.with_session_secret_broker(Arc::new(credentials)),
            None => host,
        }
    }

    /// Serve `acp:*` sessions from an explicitly supplied launch source in the
    /// Session environment the process composition already selected. Product
    /// code supplies a projected source backed by published access;
    /// deterministic dev fixtures may supply [`crate::LaunchSource::Fixed`].
    #[must_use]
    pub fn with_acp_launch_source(self, source: crate::LaunchSource) -> Self {
        self.with_bound_acp(source, None)
    }

    /// The hub-backed launch observer for this host: republishes an ACP agent's
    /// bring-up (install → launch → initialize → ready → failed) onto the per-thread
    /// hub, so a process startup wires it onto the [`AcpRunExecutor`] it builds and
    /// any protocol adapter observing the thread can render progress.
    #[must_use]
    pub fn acp_launch_observer(&self) -> Arc<dyn LaunchObserver> {
        Arc::new(HubLaunchObserver::new(self.hub.clone()))
    }

    /// Serve `acp:*` sessions on an explicitly supplied projecting resolver. This is
    /// the test/dev startup seam; production uses
    /// [`with_acp_from_deployment`](Self::with_acp_from_deployment).
    #[must_use]
    pub fn with_projected_acp(
        self,
        cli: awaken_run_executor_acp::AcpCli,
        resolver: Arc<dyn awaken_run_executor_acp::LaunchResolver>,
        store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        self.with_projected_acp_argv(cli, resolver, store_dir, None)
    }

    /// Test/dev startup seam that also consumes startup acquisition evidence.
    /// Product composition installs the same evidence through
    /// [`Self::with_acp_from_deployment`].
    #[must_use]
    pub fn with_projected_acp_argv(
        self,
        cli: awaken_run_executor_acp::AcpCli,
        resolver: Arc<dyn awaken_run_executor_acp::LaunchResolver>,
        store_dir: Option<std::path::PathBuf>,
        launch_argv: Option<Vec<String>>,
    ) -> Self {
        // A projected resolver owns both the opaque process-secret requirement and
        // its broker. Bound ACP launches from the Session Environment, so install
        // that broker on the same authoritative provider before moving the resolver
        // into the launch registry. This is last-mile startup, not a second
        // materialization path: the resolver still emits only the opaque reference.
        let secret_broker = resolver.secret_broker();
        let source = crate::LaunchSource::Projected(
            crate::AcpLaunchRegistry::single_with_resolved_argv(cli, resolver, launch_argv)
                .unwrap_or_else(|error| panic!("configure projected ACP launch route: {error}")),
        );
        // When a session-blob root is configured, recover this CLI's session across
        // directories/machines: harvest it to the (shared) root after a run and
        // restore it before the next, keyed by thread+adapter — under the same
        // config home the resolver opens. A `Gateway`/stateless CLI is skipped by the
        // executor's own dispatch; a single-machine host leaves this unset.
        let session_home = self.exec.session_blob_store.clone().map(|blobs| {
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
            slot.register_backend_projection(backend_ref);
        });
    }
}

impl crate::session_slot::SessionRuntimeSlot {
    pub(crate) fn register_backend_projection(&mut self, backend_ref: &str) {
        self.backend_ref = (backend_ref != "default").then(|| backend_ref.to_string());
    }
}

/// The base dir the Session sandbox roots and per-thread config homes live
/// under (`AWAKEN_SANDBOX_DIR`), or a per-process temp dir when unset. The
/// process composition roots the providers it selects here.
pub fn session_sandbox_base(deployment: &crate::DeploymentConfig) -> std::path::PathBuf {
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
            unreachable!("the builder test never runs a Run")
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
        assert!(host.exec.acp.is_some(), "A1 executor");
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
        host.env.session_provider =
            crate::session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
                std::env::temp_dir().join("awaken-managed-environment"),
                false,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_session_provider();
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
            host.session_environment_provider(&backend_owned)
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
                    .session_environment_provider(provisioning)
                    .unwrap_or_else(|_| panic!("{rule}"))
                    .realizer()
                    .supports_host_identity(),
                "{rule}"
            );
        }
    }

    #[test]
    fn deployment_session_blob_root_is_the_only_recovery_configuration() {
        // Causes: C1=the typed Deployment names a portable blob root;
        // C2=it leaves the root absent. Constraint: C1 and C2 are exclusive.
        // Effects: E1=C1 installs the one blob store and binds the ACP session
        // home; E2=C2 installs neither; E3=both route the exact acp:* backend.
        // Decision table: R1=C1 -> E1+E3; R2=C2 -> E2+E3. The prior
        // post-construction override is deliberately absent from both rules.
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let blobs = std::env::temp_dir().join(format!("acp-blobs-{}", std::process::id()));
        for (root, installed, rule) in [(Some(blobs), true, "R1"), (None, false, "R2")] {
            let mut deployment = crate::DeploymentConfig::ephemeral();
            deployment.acp_session_blob_root = root;
            let host = SharedHost::new_with_deployment(Arc::new(NoLlm), "test", deployment)
                .with_projected_acp(cli, Arc::new(FixedModel), None);
            assert_eq!(host.exec.session_blob_store.is_some(), installed, "{rule}");
            let acp = host.exec.acp.as_ref().expect("projected ACP backend");
            let AcpExecutorSource::Bound { session_home, .. } = &acp.source else {
                panic!("{rule} must bind the selected launch source");
            };
            assert_eq!(session_home.is_some(), installed, "{rule}/session home");
            host.register_thread_backend_projection("t", "acp:claude");
            assert_eq!(
                host.session_slots
                    .read("t", |slot| slot.backend_ref.clone())
                    .flatten()
                    .as_deref(),
                Some("acp:claude"),
                "{rule}/E3"
            );
        }
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
