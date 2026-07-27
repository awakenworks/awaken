//! ACP execution for the managed host (R3/R4). Backend selection belongs to the
//! immutable Agent publication and its frozen Session projection; this module
//! owns only the executor that drives an already-selected ACP backend.

use std::sync::Arc;

use awaken_run_executor_acp::{AcpRunExecutor, LaunchObserver, SessionHomeProvider};

enum AcpExecutorSource {
    Static(Arc<AcpRunExecutor>),
    Bound {
        launch: crate::LaunchSource,
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
    pub(crate) fn combined_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        match &self.source {
            AcpExecutorSource::Static(_) => Default::default(),
            AcpExecutorSource::Bound { launch, .. } => {
                launch.combined_credential_realization_capabilities()
            }
        }
    }

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
                launch,
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
                let source = Arc::new(crate::BoundLocalChannelSource::from_environment(
                    sandbox,
                    launch.clone(),
                    backend,
                    mcp_servers,
                ));
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
/// progress to its UI (a "starting agent…" affordance during a dynamic install).
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
            Installing => "installing",
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
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
        credentials: Option<crate::PinnedCredentialMaterializer>,
    ) -> Self {
        let deployment = self.deployment.clone();
        let Some(profile) = deployment.acp.as_ref() else {
            return match credentials {
                Some(credentials) => self.with_session_secret_broker(Arc::new(credentials)),
                None => self,
            };
        };
        let base = acp_sandbox_base(&deployment);
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
                )
            })
            .collect();
        let default = profile.default_cli().map(str::to_string);
        let source = crate::LaunchSource::Projected(
            crate::AcpLaunchRegistry::new(routes, default)
                .unwrap_or_else(|error| panic!("configure ACP launch routes: {error}")),
        );
        // The same exact, claim-fenced materializer owns both sides of the
        // last-mile seam: the resolver issues an opaque one-shot reference and
        // the selected sandbox asks it for bytes immediately before spawn.
        self.with_acp_launch_source(hand_factory, source)
            .await
            .with_session_secret_broker(Arc::new(credentials))
    }

    /// Realize an explicitly supplied ACP launch source in the deployment's sandbox
    /// tier. Product code supplies a projected source backed by published access;
    /// deterministic dev fixtures may supply [`crate::LaunchSource::Fixed`] directly.
    pub async fn with_acp_launch_source(
        self,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
        source: crate::LaunchSource,
    ) -> Self {
        if self.session_provider_explicit {
            return self.with_bound_acp(source, None);
        }
        let dep = self.deployment.clone();
        let base = acp_sandbox_base(&dep);
        // Probe the OS-native sandbox once. A bwrap-less host degrades to unsandboxed local
        // ACP when the tier was left at its default (dev/single-machine ergonomics — the
        // environment still runs) and fails closed only when `AWAKEN_SANDBOX_TIER=namespace`
        // was requested EXPLICITLY, so an operator who asked for isolation never silently
        // loses it. So a worker without bwrap works out of the box.
        let tier = crate::resolve_sandbox_tier(dep.sandbox_tier, dep.sandbox_tier_explicit, &base)
            .await
            .unwrap_or_else(|e| panic!("configure the ACP sandbox tier: {e}"));
        let mut host = self;
        match tier {
            crate::SandboxTier::Local => {
                host.session_provider =
                    crate::session_environment::SessionEnvironmentProvider::workdir(base.clone());
                if let Some(mounter) = host.memory_mounter() {
                    host.session_provider.install_memory_mounter(mounter);
                }
                return host.with_bound_acp(source, None);
            }
            crate::SandboxTier::Namespace => {
                host.session_provider =
                    crate::session_environment::SessionEnvironmentProvider::namespace(base.clone());
                if let Some(mounter) = host.memory_mounter() {
                    host.session_provider.install_memory_mounter(mounter);
                }
                return host.with_bound_acp(source, None);
            }
            _ => {}
        }
        let (provider, extra_mounts) =
            crate::container_environment::build(tier, dep.container_image.as_deref())
                .await
                .unwrap_or_else(|e| panic!("configure the ACP sandbox tier: {e}"));
        host.session_provider = crate::session_environment::SessionEnvironmentProvider::container(
            provider,
            extra_mounts,
            hand_factory,
        );
        if let Some(mounter) = host.memory_mounter() {
            host.session_provider.install_memory_mounter(mounter);
        }
        host.with_bound_acp(source, None)
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
        let session_home = if let Some(blob_root) = self.session_blob_root.clone() {
            let blobs = Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(blob_root));
            Some(Arc::new(awaken_run_executor_acp::DirSessionHome::new(
                store_dir, blobs,
            )) as Arc<dyn SessionHomeProvider>)
        } else {
            None
        };
        let host = self.with_bound_acp(source, session_home);
        match secret_broker {
            Some(broker) => host.with_session_secret_broker(broker),
            None => host,
        }
    }

    /// Stage the exact backend copied from the frozen Session baseline. This is a
    /// realization cache, not another selection API.
    pub(crate) fn register_thread_backend_projection(&self, thread: &str, backend_ref: &str) {
        self.session_slots.update(thread, |slot| {
            slot.backend_ref = Some(backend_ref.to_string());
        });
    }
}

/// The base dir the ACP sandbox roots and per-thread config homes live under
/// (`AWAKEN_SANDBOX_DIR`), or a per-process temp dir when unset.
fn acp_sandbox_base(deployment: &crate::DeploymentConfig) -> std::path::PathBuf {
    deployment.sandbox_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("awaken-acp-sbx-{}", std::process::id()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SharedHost;

    struct NoLlm;
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
            Ok(awaken_run_executor_acp::ResolvedModel {
                base_url: "http://example.invalid".into(),
                model: "test".into(),
                process_secret: None,
                credential_artifact: None,
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
            &AcpLaunchEvent::with_detail(AcpLaunchStage::Installing, "@…/claude-agent-acp@0.44"),
        );
        observer.on_launch("thread-42", &AcpLaunchEvent::stage(AcpLaunchStage::Ready));

        // The install event (with detail) then the ready event arrive on the hub,
        // scoped to the thread — exactly what a per-session UI channel renders.
        match sub.recv().await.unwrap() {
            crate::hub::ThreadEvent::AgentLaunch { stage, detail } => {
                assert_eq!(stage, "installing");
                assert_eq!(detail.as_deref(), Some("@…/claude-agent-acp@0.44"));
            }
            other => panic!("expected AgentLaunch, got {other:?}"),
        }
        match sub.recv().await.unwrap() {
            crate::hub::ThreadEvent::AgentLaunch { stage, .. } => assert_eq!(stage, "ready"),
            other => panic!("expected AgentLaunch ready, got {other:?}"),
        }
    }
}
