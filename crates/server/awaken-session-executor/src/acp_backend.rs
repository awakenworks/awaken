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
            cli, store_dir,
        ));
        let source = Arc::new(awaken_run_executor_acp::ProjectingChannelSource::new(
            cli, resolver,
        ));
        // Publish this CLI's bring-up (install/launch/initialize/ready) to the hub,
        // so a UI can show progress while a cold npx cache installs the adapter.
        let observer = self.acp_launch_observer();
        let executor = AcpRunExecutor::new(source).with_launch_observer(observer);
        self.with_acp(Arc::new(executor))
    }

    /// Stage `thread`'s runtime adapter (R3): `"acp:*"` routes it to the ACP CLI.
    pub fn register_thread_runtime(&self, thread: &str, adapter: &str) {
        if let Some(acp) = &self.acp {
            acp.register(thread, adapter);
        }
    }
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

    #[tokio::test]
    async fn hub_launch_observer_republishes_lifecycle_to_the_thread_hub() {
        use awaken_run_executor_acp::{AcpLaunchEvent, AcpLaunchStage, LaunchObserver};

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
