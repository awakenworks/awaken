//! Narrow Coordinator composition ports exposed by the runtime Host.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_run_ingress::CompletionSink;

use crate::{FileContentSource, SharedHost};

struct HostRunRecoverySource(Arc<SharedHost>);

#[async_trait::async_trait]
impl RunRecoverySource for HostRunRecoverySource {
    async fn recovery_snapshot(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let context = self
            .0
            .ctx_for(&thread_id.0, None)
            .await
            .map_err(|error| RecoveryError::Rejected(error.to_string()))?;
        context
            .commit
            .recovery_snapshot(thread_id, claimed_run_id)
            .await
    }
}

impl SharedHost {
    pub fn worker_recovery_source(self: &Arc<Self>) -> Arc<dyn RunRecoverySource> {
        Arc::new(HostRunRecoverySource(self.clone()))
    }

    pub fn worker_completion_sink(&self) -> Arc<dyn CompletionSink> {
        self.completion.clone()
    }

    pub fn worker_stream_sink(&self) -> Arc<dyn StreamSink> {
        self.completion.clone()
    }

    pub fn worker_file_content_source(
        &self,
    ) -> Arc<dyn FileContentSource<awaken_run_ingress::RunClaim>> {
        self.file_content_source.clone()
    }

    pub fn worker_artifact_publisher(
        &self,
    ) -> Arc<dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>> {
        self.artifact_publisher.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RemotePublisher;

    #[async_trait::async_trait]
    impl awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim> for RemotePublisher {
        async fn publish(
            &self,
            _publication: awaken_resource_contract::ArtifactPublication<
                awaken_run_ingress::RunClaim,
            >,
        ) -> Result<
            awaken_resource_contract::FileRecord,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture",
            ))
        }
    }

    #[test]
    fn remote_artifact_port_removes_the_local_file_command_path() {
        // Composition FMECA/cause-effect rule: C1 test/embedded Host owns a local
        // File application; C2 composition installs the database-less Worker's
        // remote artifact port. Effect E1 only the remote port remains and local
        // File management authority is absent. Rule P1 C1+C2=>E1 prevents the
        // Worker from selecting an unfenced parallel command path.
        let host = SharedHost::new(
            Arc::new(crate::NoModelConfiguredExecutor),
            "composition-fixture",
        );
        assert!(host.file_application().is_some(), "P1/C1");
        let host = host.with_artifact_publisher(Arc::new(RemotePublisher));
        assert!(host.file_application().is_none(), "P1/E1");
    }
}
