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
    fn adapter_policy(
        &self,
    ) -> &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    > {
        awaken_run_executor_acp::acp_adapter_policy()
    }

    fn create(&self, request: AcpSessionExecution) -> Result<Arc<dyn RunAttemptExecutor>, String> {
        let source = Arc::new(
            BoundLocalChannelSource::from_environment(
                request.environment,
                self.launch.clone(),
                request.backend,
                request.mcp_servers,
                request.environment_variables,
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
    ) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, String> {
        self.launch
            .credential_realization_capabilities(backend)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_data_governance_contract::ContentEraser;
    use awaken_run_executor_acp::{
        ConfigHome, FsSessionBlobStore, SessionBlobStore, SessionHomeKey, SessionHomePlan,
    };
    use awaken_runtime_contract::DataSubjectId;

    #[tokio::test]
    async fn deployment_session_blob_root_is_the_only_recovery_and_erasure_scope() {
        // Causes: C1 the frozen Deployment names a portable blob root; C2 it
        // leaves that root absent; C3 two subjects share thread and adapter;
        // C4 one subject is erased. Effects: E1 C1 harvests into exactly that
        // root; E2 C2 installs no portable home; E3 C3 scopes each opaque blob;
        // E4 C4 removes only the erased subject and retains the other.
        // Constraint: C1/C2 are mutually exclusive, and erasure consumes the
        // same FsSessionBlobStore root that Coordinator configures from the
        // Deployment. Decision table: R1 C1+C3 -> E1+E3; R2 C2 -> E2;
        // R3 C1+C3+C4 -> E4. Exact CLI routing is covered by launch tests.
        let root = tempfile::tempdir().unwrap();
        let other_root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.acp_session_blob_root = Some(root.path().to_path_buf());
        let launch = || {
            LaunchSource::Fixed(Box::new(awaken_run_executor_acp::AcpLaunch::custom(
                vec!["fixture".into()],
                vec![],
            )))
        };
        let factory = BoundAcpSessionFactory::new(
            launch(),
            None,
            deployment.acp_session_blob_root.clone(),
            Some(config.path().to_path_buf()),
        );
        let home = factory.session_home.as_ref().expect("R1 portable home");
        let plan = SessionHomePlan {
            config_home_env: "HOME".into(),
            session_subpath: "projects/conv".into(),
            exclude: vec![],
            keyed_by_cwd: false,
        };
        let alice = SessionHomeKey {
            data_subject_id: Some("alice".into()),
            thread_id: "shared-thread".into(),
            adapter: "claude".into(),
        };
        let bob = SessionHomeKey {
            data_subject_id: Some("bob".into()),
            ..alice.clone()
        };
        let source = ConfigHome::open(Some(config.path()), "shared-thread").unwrap();
        source
            .write("projects/conv/session.jsonl", b"alice")
            .unwrap();
        home.harvest(&alice, &plan).await;
        source.write("projects/conv/session.jsonl", b"bob").unwrap();
        home.harvest(&bob, &plan).await;

        let blobs = FsSessionBlobStore::new(root.path().to_path_buf());
        let wrong = FsSessionBlobStore::new(other_root.path().to_path_buf());
        let fetched = tempfile::tempdir().unwrap();
        assert!(
            blobs.fetch(&alice, fetched.path(), &[]).await.unwrap(),
            "R1"
        );
        assert_eq!(
            std::fs::read(fetched.path().join("session.jsonl")).unwrap(),
            b"alice",
            "R1"
        );
        assert!(
            !wrong.fetch(&alice, fetched.path(), &[]).await.unwrap(),
            "R1 exact root"
        );
        assert_eq!(
            blobs
                .erase_subject(&DataSubjectId("alice".into()))
                .await
                .unwrap(),
            1,
            "R3"
        );
        let survivor = tempfile::tempdir().unwrap();
        assert!(
            !blobs.fetch(&alice, fetched.path(), &[]).await.unwrap(),
            "R3 erased"
        );
        assert!(
            blobs.fetch(&bob, survivor.path(), &[]).await.unwrap(),
            "R3 retained"
        );
        assert_eq!(
            std::fs::read(survivor.path().join("session.jsonl")).unwrap(),
            b"bob",
            "R3"
        );

        deployment.acp_session_blob_root = None;
        let no_portable_home = BoundAcpSessionFactory::new(
            launch(),
            None,
            deployment.acp_session_blob_root,
            Some(config.path().to_path_buf()),
        );
        assert!(no_portable_home.session_home.is_none(), "R2");
    }
}
