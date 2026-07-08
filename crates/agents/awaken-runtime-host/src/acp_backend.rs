//! ACP backend routing for the managed host (R3/R4): which threads run on an
//! external ACP CLI, and the executor that drives them.
//!
//! A session selects its runtime through the Managed API (`agent.runtime`), staged
//! here per thread. `is_acp` decides the routing in `turn_exec`; the executor is a
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

impl crate::host::SharedHost {
    /// Serve `acp:*` sessions on `executor` (R3/R4). Threads select it via the API.
    pub fn with_acp(mut self, executor: Arc<AcpRunExecutor>) -> Self {
        self.acp = Some(Arc::new(AcpBackend::new(executor)));
        self
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
        self.with_acp(Arc::new(AcpRunExecutor::new(source)))
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
}
