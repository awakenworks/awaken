//! Runtime services exposed to a database-less execution Worker.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_run_ingress::CompletionSink;
use awaken_run_ingress::{
    DispatchSettlementError, DispatchSettlementObserver, RunClaim, RunDispatch,
};

use crate::{FileContentSource, SharedHost};

struct HostRunRecoverySource(Arc<SharedHost>);

/// Coordinator-side Memory observation at the registered-Worker settlement
/// boundary. The Worker already owns Session-coordination settlement; keeping
/// this adapter Memory-only prevents the two processes from delivering the
/// same coordinated child effect twice.
struct HostMemoryDispatchSettlementObserver(Arc<SharedHost>);

#[async_trait::async_trait]
impl RunRecoverySource for HostRunRecoverySource {
    async fn recovery_snapshot(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.recovery_snapshot_in_session(thread_id, thread_id, claimed_run_id)
            .await
    }

    async fn recovery_snapshot_in_session(
        &self,
        session_thread_id: &awaken_agent_contract::agent::thread::Id,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.0
            .session_thread_run_recovery_snapshot(&session_thread_id.0, thread_id, claimed_run_id)
            .await
            .map_err(|error| RecoveryError::Rejected(error.to_string()))
    }
}

impl SharedHost {
    pub fn worker_recovery_source(self: &Arc<Self>) -> Arc<dyn RunRecoverySource> {
        Arc::new(HostRunRecoverySource(self.clone()))
    }

    pub fn worker_memory_settlement_observer(
        self: &Arc<Self>,
    ) -> Arc<dyn DispatchSettlementObserver> {
        Arc::new(HostMemoryDispatchSettlementObserver(self.clone()))
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
    ) -> Arc<
        dyn awaken_resource_contract::ArtifactPublisher<
                awaken_run_ingress::ArtifactPublicationFence,
            >,
    > {
        self.artifact_publisher.clone()
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for HostMemoryDispatchSettlementObserver {
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
            .0
            .commit_for_read(&dispatch.session_thread_id().0)
            .await
            .map_err(|error| DispatchSettlementError(error.to_string()))?;
        let observers = self
            .0
            .dispatched_memory_terminal_observer(dispatch, commit)
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
    impl awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::ArtifactPublicationFence>
        for RemotePublisher
    {
        async fn publish(
            &self,
            _publication: awaken_resource_contract::ArtifactPublication<
                awaken_run_ingress::ArtifactPublicationFence,
            >,
        ) -> Result<
            awaken_resource_contract::ArtifactPublicationReceipt,
            awaken_resource_contract::ArtifactPublicationError,
        > {
            Err(awaken_resource_contract::ArtifactPublicationError::new(
                "fixture",
            ))
        }
    }

    #[test]
    fn remote_artifact_publisher_removes_local_file_commands() {
        // FMECA: a database-less Worker retaining local File commands could
        // publish outside the Coordinator's fenced Resource authority.
        // Cause/effect graph: C1 test/embedded Host owns a local File
        // application; C2 startup installs the Worker's remote artifact
        // publisher; E1 only remote publication remains and local File commands
        // are absent. Decision rule P1: C1+C2 => E1. This prevents an unfenced
        // parallel command path.
        let host = SharedHost::new(
            Arc::new(crate::NoModelConfiguredExecutor),
            "startup-fixture",
        );
        assert!(host.file_application().is_some(), "P1/C1");
        let host = host.with_artifact_publisher(Arc::new(RemotePublisher));
        assert!(host.file_application().is_none(), "P1/E1");
    }
}
