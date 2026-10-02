// @formal entrypoint worker.acp_capability_is_detected_only_after_a_verified_handshake
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! The one bound ACP executor factory installed by Worker process composition.
//! Session selection and Run commits remain in Runtime Host.

use std::sync::Arc;

use awaken_acp_contract::LaunchObserver;
use awaken_run_executor_acp::{AcpRunExecutor, SessionHomeProvider};
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::resolved::Backend;
use awaken_session_environment_contract::{AcpSessionExecution, AcpSessionExecutorFactory};

use crate::sandbox_source::{BoundLocalChannelSource, LaunchSource};

pub(crate) struct BoundAcpSessionFactory {
    launch: LaunchSource,
    observer: Option<Arc<dyn LaunchObserver>>,
    session_home: Option<Arc<dyn SessionHomeProvider>>,
}

impl BoundAcpSessionFactory {
    pub(crate) fn new(
        launch: LaunchSource,
        observer: Option<Arc<dyn LaunchObserver>>,
        portable_root: Option<std::path::PathBuf>,
        store_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let session_home = portable_root.map(|root| {
            let blobs = Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(root));
            Arc::new(awaken_run_executor_acp::DirSessionHome::new(
                store_dir, blobs,
            )) as Arc<dyn SessionHomeProvider>
        });
        Self {
            launch,
            observer,
            session_home,
        }
    }
}

impl AcpSessionExecutorFactory for BoundAcpSessionFactory {
    fn create(&self, request: AcpSessionExecution) -> Result<Arc<dyn RunAttemptExecutor>, String> {
        let source = Arc::new(
            BoundLocalChannelSource::from_environment(
                request.environment,
                self.launch.clone(),
                request.backend,
                request.mcp_servers,
            )
            .with_attached_tools(request.attached_tools),
        );
        let mut executor = AcpRunExecutor::new(source);
        if let Some(observer) = &self.observer {
            executor = executor.with_launch_observer(observer.clone());
        }
        if let Some(session_home) = &self.session_home {
            executor = executor.with_session_home(session_home.clone());
        }
        if let Some(observer) = request.grant_observer {
            executor = executor.with_permission_grant_observer(observer);
        }
        Ok(Arc::new(
            executor
                .with_permission_policy(request.permission)
                .with_owned_tool_continuation(request.continuation),
        ))
    }

    fn credential_realization_capabilities(
        &self,
        backend: &Backend,
    ) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, String> {
        self.launch
            .credential_realization_capabilities(backend)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_session_blob_root_is_the_only_recovery_configuration() {
        // Causes: C1 the frozen Deployment names a portable blob root; C2 it
        // leaves that root absent. They are mutually exclusive. Effects: E1
        // C1 installs the one session-home adapter; E2 C2 installs none. The
        // exact acp:* route is independently checked by launch registry tests.
        // Decision table: R1 C1 -> E1; R2 C2 -> E2.
        let root = std::env::temp_dir().join(format!("acp-blobs-{}", std::process::id()));
        for (portable_root, installed, rule) in [(Some(root), true, "R1"), (None, false, "R2")] {
            let factory = BoundAcpSessionFactory::new(
                LaunchSource::Fixed(Box::new(awaken_run_executor_acp::AcpLaunch::custom(
                    vec!["fixture".into()],
                    vec![],
                ))),
                None,
                portable_root,
                None,
            );
            assert_eq!(factory.session_home.is_some(), installed, "{rule}");
        }
    }
}
