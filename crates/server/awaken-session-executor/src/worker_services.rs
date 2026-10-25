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

    pub fn worker_completion_sink(&self) -> Arc<dyn CompletionSink> {
        self.settlement.clone()
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
        use awaken_agent_contract::agent::run::RunState;

        if dispatch.run_id() != &claim.run_id {
            return Err(DispatchSettlementError(
                "Memory settlement dispatch does not match the guarded Run claim".into(),
            ));
        }
        let cause = match committed_state {
            RunState::Ended(cause) => cause.clone(),
            RunState::Awaiting => return Ok(()),
            RunState::Running => {
                return Err(DispatchSettlementError(
                    "Memory settlement requires committed Awaiting or Ended truth".into(),
                ));
            }
        };
        let commit = self
            .reads
            .commit_for_read(&dispatch.session_thread_id().0)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))?;
        let observers = self
            .terminal_observers
            .for_dispatch(dispatch, commit)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))?
            .into_iter()
            .collect::<Vec<_>>();
        let terminal = awaken_runtime_contract::terminal::CommittedTerminalRun {
            run_id: dispatch.run_id().clone(),
            thread_id: dispatch.thread_id().clone(),
            cause,
        };
        let failures =
            awaken_runtime_contract::terminal::deliver_committed_terminal(&observers, &terminal)
                .await;
        if failures.is_empty() {
            return Ok(());
        }
        Err(DispatchSettlementError(
            failures
                .into_iter()
                .map(|failure| format!("{}: {}", failure.observer_id, failure.error))
                .collect::<Vec<_>>()
                .join("; "),
        ))
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
