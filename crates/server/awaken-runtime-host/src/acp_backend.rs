//! ACP backend routing for the managed host (R3/R4): which threads run on an
//! external ACP CLI, and the executor that drives them.
//!
//! A session selects its runtime through the Managed API (`agent.runtime`), staged
//! here per thread. `is_acp` decides the routing in `run_exec`; the executor is a
//! peer `RunExecutor` that launches the CLI and commits through the same boundary
//! as the native path.

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

/// Holds the ACP executor and the per-thread runtime selection.
pub(crate) struct AcpBackend {
    source: AcpExecutorSource,
    slots: crate::session_slot::SessionRuntimeSlots,
    /// The deployment's DEFAULT backend adapter (e.g. `"acp:claude"`), applied to a
    /// thread that staged none. This is how a single-purpose ACP deployment routes
    /// every session to the CLI WITHOUT a per-session `awaken.runtime` override — the
    /// backend is a property of the deployment/agent, not a client-supplied knob. A
    /// mixed host leaves it `None`, so only explicitly-selected `acp:*` threads route.
    default_adapter: Option<String>,
}

impl AcpBackend {
    /// Set the deployment default backend adapter (see [`Self::default_adapter`]).
    fn with_default_adapter(mut self, adapter: String) -> Self {
        self.default_adapter = Some(adapter);
        self
    }

    pub(crate) fn new(
        executor: Arc<AcpRunExecutor>,
        slots: crate::session_slot::SessionRuntimeSlots,
    ) -> Self {
        Self {
            source: AcpExecutorSource::Static(executor),
            slots,
            default_adapter: None,
        }
    }

    fn bound(
        launch: crate::LaunchSource,
        observer: Option<Arc<dyn LaunchObserver>>,
        session_home: Option<Arc<dyn SessionHomeProvider>>,
        slots: crate::session_slot::SessionRuntimeSlots,
    ) -> Self {
        Self {
            source: AcpExecutorSource::Bound {
                launch,
                observer,
                session_home,
            },
            slots,
            default_adapter: None,
        }
    }

    /// Materialize the executor for this session. Production local ACP uses a
    /// source bound to the session's existing sandbox; injected/static executors
    /// remain available for remote adapters and deterministic tests.
    pub(crate) fn executor_for(
        &self,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
    ) -> Arc<AcpRunExecutor> {
        match &self.source {
            AcpExecutorSource::Static(executor) => {
                Arc::new(executor.for_permission_policy(permission))
            }
            AcpExecutorSource::Bound {
                launch,
                observer,
                session_home,
            } => {
                let source = Arc::new(crate::BoundLocalChannelSource::from_environment(
                    sandbox,
                    launch.clone(),
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

    /// Stage `thread`'s selected runtime adapter (e.g. `"acp:claude"` or `"awaken"`).
    pub(crate) fn register(&self, thread: &str, adapter: &str) {
        self.slots.update(thread, |slot| {
            slot.runtime_adapter = Some(adapter.to_string());
        });
    }

    /// Resolve the effective runtime selected for a thread. An explicit Session
    /// selection wins over the deployment default.
    pub(crate) fn adapter_for(&self, thread: &str) -> Option<String> {
        self.slots
            .read(thread, |slot| slot.runtime_adapter.clone())
            .flatten()
            .or_else(|| self.default_adapter.clone())
    }

    /// Whether `thread` runs on an ACP CLI (`acp` / `acp:*`). Routes through the
    /// typed [`Backend`](awaken_runtime_contract::resolved::Backend) so the `acp:`
    /// parsing lives in one place, not duplicated as a string check here.
    pub(crate) fn is_acp(&self, thread: &str) -> bool {
        // The thread's explicit selection, else the deployment default — either way
        // the `acp:` parse lives in the one `Backend::from_ref`, never a string check.
        match self.adapter_for(thread).as_deref() {
            Some(adapter) => awaken_runtime_contract::resolved::Backend::from_ref(adapter).is_acp(),
            None => false,
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
        self.acp = Some(Arc::new(AcpBackend::new(
            executor,
            self.session_slots.clone(),
        )));
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
            self.session_slots.clone(),
        )));
        self
    }

    /// Serve `acp:*` sessions on `executor` AND make `default_adapter` (e.g.
    /// `"acp:claude"`) the deployment's default backend: every session routes to the
    /// ACP CLI unless it explicitly selects another runtime. This is the "backend
    /// resolved from the deployment/agent, not from a client `awaken.runtime` knob"
    /// path — for a single-purpose ACP deployment, sessions carry no runtime metadata.
    pub fn with_acp_default(
        mut self,
        executor: Arc<AcpRunExecutor>,
        default_adapter: impl Into<String>,
    ) -> Self {
        self.acp = Some(Arc::new(
            AcpBackend::new(executor, self.session_slots.clone())
                .with_default_adapter(default_adapter.into()),
        ));
        self
    }

    /// Wire the ACP backend from deployment capability discovery plus persisted,
    /// publication-pinned provider access. This is the ONE place both the server and
    /// worker roots configure ACP, so they never drift.
    ///
    /// `AWAKEN_ACP_CLI=<id>` advertises the CLI installed on this worker; the run's
    /// published `acp:<cli>` binding remains authoritative and must match. Provider
    /// endpoint/model/credential data come only from `credentials` and the snapshot.
    /// Neither capability set → no ACP backend served. Panics on an advertised ACP
    /// capability without a credential materializer or on a misconfigured tier.
    pub async fn with_acp_from_deployment(
        self,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
        credentials: Option<crate::PinnedCredentialMaterializer>,
    ) -> Self {
        let base = acp_sandbox_base();
        let source = match acp_serve_cli() {
            Some(id) => {
                let cli = *awaken_run_executor_acp::acp_cli(&id)
                    .unwrap_or_else(|| panic!("AWAKEN_ACP_CLI={id} is not a known ACP CLI"));
                let credentials = credentials.unwrap_or_else(|| {
                    panic!(
                        "AWAKEN_ACP_CLI={id} requires persisted credential materialization stores"
                    )
                });
                let resolver = Arc::new(crate::PublishedAcpLaunchResolver::new(
                    cli,
                    Some(base.clone()),
                    credentials,
                ));
                crate::LaunchSource::Projected {
                    cli: Box::new(cli),
                    resolver,
                }
            }
            None => return self,
        };
        self.with_acp_launch_source(hand_factory, source).await
    }

    /// Realize an explicitly supplied ACP launch source in the deployment's sandbox
    /// tier. Product code supplies a projected source backed by published access;
    /// deterministic dev fixtures may supply [`LaunchSource::Fixed`] directly.
    pub async fn with_acp_launch_source(
        self,
        hand_factory: Arc<dyn crate::HandExecutorFactory>,
        source: crate::LaunchSource,
    ) -> Self {
        let base = acp_sandbox_base();
        let dep = crate::DeploymentConfig::from_env();
        // Probe the OS-native sandbox once. A bwrap-less host degrades to unsandboxed local
        // ACP when the tier was left at its default (dev/single-machine ergonomics — the
        // environment still runs) and fails closed only when `AWAKEN_SANDBOX_TIER=namespace`
        // was requested EXPLICITLY, so an operator who asked for isolation never silently
        // loses it. So a worker without bwrap works out of the box.
        let tier_explicit = std::env::var_os("AWAKEN_SANDBOX_TIER").is_some();
        let tier = crate::resolve_sandbox_tier(dep.sandbox_tier, tier_explicit, &base)
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
        let source = crate::LaunchSource::Projected {
            cli: Box::new(cli),
            resolver,
        };
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
        self.with_bound_acp(source, session_home)
    }

    /// Stage `thread`'s runtime adapter (R3): `"acp:*"` routes it to the ACP CLI.
    pub fn register_thread_runtime(&self, thread: &str, adapter: &str) {
        if let Some(acp) = &self.acp {
            acp.register(thread, adapter);
        }
    }
}

/// The ACP CLI capability this worker advertises. It does not select the run's
/// backend: the published snapshot must independently name the same `acp:<cli>`.
fn acp_serve_cli() -> Option<String> {
    std::env::var("AWAKEN_ACP_CLI")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// The base dir the ACP sandbox roots and per-thread config homes live under
/// (`AWAKEN_SANDBOX_DIR`), or a per-process temp dir when unset.
fn acp_sandbox_base() -> std::path::PathBuf {
    std::env::var("AWAKEN_SANDBOX_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
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
        ) -> Result<awaken_run_executor_acp::ResolvedModel, awaken_run_executor_acp::OpenError>
        {
            Ok(awaken_run_executor_acp::ResolvedModel {
                base_url: "http://example.invalid".into(),
                model: "test".into(),
                api_key: "test".into(),
            })
        }
    }

    #[test]
    fn with_projected_acp_wires_an_acp_backend_that_routes_acp_threads() {
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_projected_acp(
            cli,
            Arc::new(FixedModel),
            None,
        );
        host.register_thread_runtime("t", "acp:claude");
        let acp = host.acp.as_ref().expect("acp backend wired");
        assert!(acp.is_acp("t"));
        assert_eq!(acp.adapter_for("t").as_deref(), Some("acp:claude"));
        assert!(!acp.is_acp("native-thread"));
    }

    #[test]
    fn a_deployment_default_backend_routes_a_session_with_no_runtime_metadata() {
        // A single-purpose ACP deployment declares its default backend, so a session
        // carrying NO `awaken.runtime` override still routes to the ACP CLI — the
        // backend comes from the deployment, not a per-session client knob.
        let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
        let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
            launch,
        ));
        let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
        let host =
            SharedHost::new(Arc::new(NoLlm), "test").with_acp_default(executor, "acp:custom");
        let acp = host.acp.as_ref().expect("acp backend wired");
        // An unstaged thread inherits the deployment default → routes to ACP.
        assert!(acp.is_acp("unstaged-thread"));
        assert_eq!(
            acp.adapter_for("unstaged-thread").as_deref(),
            Some("acp:custom")
        );
        // An explicit non-ACP selection still overrides the default (native path).
        host.register_thread_runtime("native-thread", "awaken");
        assert!(!acp.is_acp("native-thread"));
        assert_eq!(acp.adapter_for("native-thread").as_deref(), Some("awaken"));
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
        host.register_thread_runtime("t", "acp:claude");
        assert!(host.acp.as_ref().expect("acp backend wired").is_acp("t"));
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
