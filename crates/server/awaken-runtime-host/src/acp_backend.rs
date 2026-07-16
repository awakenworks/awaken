//! ACP backend routing for the managed host (R3/R4): which threads run on an
//! external ACP CLI, and the executor that drives them.
//!
//! A session selects its runtime through the Managed API (`agent.runtime`), staged
//! here per thread. `is_acp` decides the routing in `run_exec`; the executor is a
//! peer `RunExecutor` that launches the CLI and commits through the same boundary
//! as the native path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_run_executor_acp::AcpRunExecutor;

/// Holds the ACP executor and the per-thread runtime selection.
pub(crate) struct AcpBackend {
    pub(crate) executor: Arc<AcpRunExecutor>,
    thread_runtime: Mutex<HashMap<String, String>>,
}

impl AcpBackend {
    pub(crate) fn new(executor: Arc<AcpRunExecutor>) -> Self {
        Self {
            executor,
            thread_runtime: Mutex::new(HashMap::new()),
        }
    }

    /// Stage `thread`'s selected runtime adapter (e.g. `"acp:claude"` or `"awaken"`).
    pub(crate) fn register(&self, thread: &str, adapter: &str) {
        self.thread_runtime
            .lock()
            .expect("acp thread-runtime mutex poisoned")
            .insert(thread.to_string(), adapter.to_string());
    }

    /// Whether `thread` runs on an ACP CLI (`acp` / `acp:*`). Routes through the
    /// typed [`Backend`](awaken_runtime_contract::resolved::Backend) so the `acp:`
    /// parsing lives in one place, not duplicated as a string check here.
    pub(crate) fn is_acp(&self, thread: &str) -> bool {
        self.thread_runtime
            .lock()
            .expect("acp thread-runtime mutex poisoned")
            .get(thread)
            .is_some_and(|a| awaken_runtime_contract::resolved::Backend::from_ref(a).is_acp())
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

    /// Wire the ACP backend from the standard environment — the ONE place both the
    /// server (`awaken serve`) and worker (`awaken_worker::run`) composition roots
    /// configure ACP, so they never drift (ADR-0057 `serve-selected-cli`).
    ///
    /// `AWAKEN_ACP_CLI=<id>` selects the production projecting path (each run's
    /// config-plane `acp:<cli>` launched through its catalog row); `AWAKEN_ACP_ARGV`
    /// a fixed trusted/test CLI; neither set → no ACP backend served. The source is
    /// realized in `AWAKEN_SANDBOX_TIER` (`local`/`namespace`/container) — the
    /// executor is unaware of which (worker + provisioning own the environment).
    /// Panics on a misconfigured tier, never a silent fallback.
    pub async fn with_acp_from_env(self) -> Self {
        let base = acp_sandbox_base();
        let source = match (acp_serve_cli(), acp_launch_argv()) {
            (Some(id), _) => {
                let cli = *awaken_run_executor_acp::acp_cli(&id)
                    .unwrap_or_else(|| panic!("AWAKEN_ACP_CLI={id} is not a known ACP CLI"));
                let resolver = Arc::new(crate::EnvLaunchResolver::from_process_env(
                    cli,
                    Some(base.clone()),
                ));
                crate::LaunchSource::Projected { cli, resolver }
            }
            (None, Some(argv)) => {
                crate::LaunchSource::Fixed(awaken_run_executor_acp::AcpLaunch::custom(argv, vec![]))
            }
            (None, None) => return self,
        };
        let dep = crate::DeploymentConfig::from_env();
        let egress = self.thread_egress();
        let resources = self.thread_resources_handle();
        let channel = crate::build_acp_channel_source(
            dep.sandbox_tier,
            dep.container_image.as_deref(),
            source,
            egress,
            resources,
            base,
        )
        .await
        .unwrap_or_else(|e| panic!("configure the ACP sandbox tier: {e}"));
        self.with_acp(Arc::new(AcpRunExecutor::new(channel)))
    }

    /// The hub-backed launch observer for this host: republishes an ACP agent's
    /// bring-up (install → launch → initialize → ready → failed) onto the per-thread
    /// hub, so a composition root wires it onto the [`AcpRunExecutor`] it builds and
    /// any protocol adapter observing the thread can render progress.
    #[must_use]
    pub fn acp_launch_observer(&self) -> Arc<dyn awaken_run_executor_acp::LaunchObserver> {
        Arc::new(HubLaunchObserver::new(self.hub.clone()))
    }

    /// Serve `acp:*` sessions on a projecting executor for `cli` (R3/R4): each run's
    /// model is resolved from the environment + the thread's [`ConfigHome`], and the
    /// [`AcpCli`] row projects it onto the launch. `store_dir` is the durable root for
    /// the config home. This is the composition-root path for a real ACP CLI — it
    /// assembles the catalog projection, the host resolver, and the executor into one.
    #[must_use]
    pub fn with_projected_acp(
        self,
        cli: awaken_run_executor_acp::AcpCli,
        store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let resolver = Arc::new(crate::acp_provision::EnvLaunchResolver::from_process_env(
            cli,
            store_dir.clone(),
        ));
        let source = Arc::new(awaken_run_executor_acp::ProjectingChannelSource::new(
            cli, resolver,
        ));
        // Publish this CLI's bring-up (install/launch/initialize/ready) to the hub,
        // so a UI can show progress while a cold npx cache installs the adapter.
        let observer = self.acp_launch_observer();
        let mut executor = AcpRunExecutor::new(source).with_launch_observer(observer);
        // When a session-blob root is configured, recover this CLI's session across
        // directories/machines: harvest it to the (shared) root after a run and
        // restore it before the next, keyed by thread+adapter — under the same
        // config home the resolver opens. A `Gateway`/stateless CLI is skipped by the
        // executor's own dispatch; a single-machine host leaves this unset.
        if let Some(blob_root) = self.session_blob_root.clone() {
            let blobs = Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(blob_root));
            executor = executor.with_session_home(Arc::new(
                awaken_run_executor_acp::DirSessionHome::new(store_dir, blobs),
            ));
        }
        self.with_acp(Arc::new(executor))
    }

    /// Stage `thread`'s runtime adapter (R3): `"acp:*"` routes it to the ACP CLI.
    pub fn register_thread_runtime(&self, thread: &str, adapter: &str) {
        if let Some(acp) = &self.acp {
            acp.register(thread, adapter);
        }
    }
}

/// The ACP CLI id this worker serves (`AWAKEN_ACP_CLI`), selecting the production
/// projecting path. Takes precedence over the fixed `AWAKEN_ACP_ARGV`.
fn acp_serve_cli() -> Option<String> {
    std::env::var("AWAKEN_ACP_CLI")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// The fixed agent-CLI argv for `acp:*` sessions (`AWAKEN_ACP_ARGV`, whitespace-split,
/// e.g. `claude --acp`). `None` when unset/blank.
fn acp_launch_argv() -> Option<Vec<String>> {
    std::env::var("AWAKEN_ACP_ARGV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split_whitespace().map(String::from).collect())
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

    #[test]
    fn with_projected_acp_wires_an_acp_backend_that_routes_acp_threads() {
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let host = SharedHost::new(Arc::new(NoLlm), "test").with_projected_acp(cli, None);
        host.register_thread_runtime("t", "acp:claude");
        let acp = host.acp.as_ref().expect("acp backend wired");
        assert!(acp.is_acp("t"));
        assert!(!acp.is_acp("native-thread"));
    }

    #[test]
    fn a_session_blob_root_composes_a_recovering_acp_backend() {
        // With a session-blob root set, the projecting-ACP composition wires the
        // session-home recovery into the executor and still routes acp:* threads.
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let blobs = std::env::temp_dir().join(format!("acp-blobs-{}", std::process::id()));
        let host = SharedHost::new(Arc::new(NoLlm), "test")
            .with_session_blob_root(blobs)
            .with_projected_acp(cli, None);
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
