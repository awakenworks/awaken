// @formal entrypoint remote_worker.recovery_projection_is_committed_prefix
//! Runtime services exposed to a database-less execution Worker.

use std::sync::Arc;

use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_contract::CompletionSink;
use awaken_dispatch_contract::RunClaim;
use awaken_dispatch_contract::{DispatchSettlementError, DispatchSettlementObserver, RunDispatch};

use crate::{ExecutorComponents, FileContentSource};

/// Coordinator-side Memory observation at the registered-Worker settlement
/// boundary. The Worker already owns Session-coordination settlement; keeping
/// this adapter Memory-only prevents the two processes from delivering the
/// same coordinated child effect twice.
struct MemoryDispatchSettlementObserver {
    reads: Arc<crate::CommittedReads>,
    terminal_observers: Arc<crate::memory::MemoryTerminalObservers>,
}

impl ExecutorComponents {
    pub fn worker_recovery_source(&self) -> Arc<dyn RunRecoverySource> {
        self.thread_reads.clone()
    }

    pub fn worker_memory_settlement_observer(&self) -> Arc<dyn DispatchSettlementObserver> {
        Arc::new(MemoryDispatchSettlementObserver {
            reads: self.thread_reads.clone(),
            terminal_observers: self.resources.memory_terminal_observers.clone(),
        })
    }

    /// The original completion relay. Managed composition wraps this same
    /// value with its lifecycle hint before starting dispatch and transport;
    /// a standalone SDK or Worker uses the relay directly.
    pub fn worker_completion_sink(&self) -> Arc<dyn CompletionSink> {
        self.completion.clone()
    }

    pub fn worker_stream_sink(&self) -> Arc<dyn StreamSink> {
        self.completion.clone()
    }

    pub fn worker_file_content_source(
        &self,
    ) -> Arc<dyn FileContentSource<crate::ResourceOperationFence>> {
        self.resources.file_content_source.clone()
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for MemoryDispatchSettlementObserver {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &awaken_agent_contract::agent::run::RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        awaken_session_run_delivery::observe_dispatched_memory_terminal(
            &self.reads,
            dispatch,
            claim,
            committed_state,
            |commit| async move {
                self.terminal_observers
                    .for_dispatch(dispatch, commit)
                    .await
                    .map_err(|error| DispatchSettlementError(error.to_string()))
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RemotePublisher;

    #[async_trait::async_trait]
    impl awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>
        for RemotePublisher
    {
        async fn publish(
            &self,
            _publication: awaken_resource_contract::ArtifactPublication<
                crate::ResourceOperationFence,
            >,
        ) -> Result<
            awaken_resource_contract::ArtifactPublicationReceipt,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture",
            ))
        }

        async fn capture(
            &self,
            _workspace_id: &str,
            _session_id: &str,
            _fence: Option<&crate::ResourceOperationFence>,
        ) -> Result<
            Option<awaken_resource_contract::ArtifactCapture>,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture does not capture Artifacts",
            ))
        }

        async fn replace_capture(
            &self,
            _request: awaken_resource_contract::ReplaceArtifactCapture,
            _fence: Option<&crate::ResourceOperationFence>,
        ) -> Result<
            awaken_resource_contract::ReplaceArtifactCaptureOutcome,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture does not replace Artifact captures",
            ))
        }

        async fn recover(
            &self,
            _recovery: awaken_resource_contract::ArtifactRecovery<crate::ResourceOperationFence>,
        ) -> Result<
            Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture does not recover Artifacts",
            ))
        }
    }

    #[tokio::test]
    async fn remote_artifact_publisher_removes_local_file_commands() {
        // FMECA: a database-less Worker retaining local File commands could
        // publish outside the Coordinator's fenced Resource authority.
        // Cause/effect graph: C1 test/embedded Host owns a local File
        // application; C2 startup installs the Worker's remote artifact
        // publisher; E1 only remote publication remains and local File commands
        // are absent. Decision rule P1: C1+C2 => E1. This prevents an unfenced
        // parallel command path.
        let dependencies = || {
            crate::SessionExecutorBuilder::ephemeral(
                Arc::new(crate::NoModelConfiguredExecutor),
                "startup-fixture",
            )
        };
        let host = (dependencies().await)
            .build()
            .expect("validated Host dependencies");
        assert!(host.file_application().is_some(), "P1/C1");
        let host = (dependencies()
            .await
            .with_artifact_publisher(Arc::new(RemotePublisher)))
        .build()
        .expect("validated Host dependencies");
        assert!(host.file_application().is_none(), "P1/E1");
    }
}
