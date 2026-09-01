//! Authority-derived request deadline decoration for Session realization Control.

use std::sync::Arc;

#[derive(Clone)]
pub(super) struct DeadlineSessionRealizationControl {
    inner: Arc<dyn awaken_run_ingress_contract::ClaimedSessionControl>,
    request_timeout: std::time::Duration,
}

impl DeadlineSessionRealizationControl {
    pub(super) fn new(
        inner: Arc<dyn awaken_run_ingress_contract::ClaimedSessionControl>,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            inner,
            request_timeout,
        }
    }

    async fn call<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<
            Output = Result<T, awaken_session_contract::SessionRealizationControlFailure>,
        >,
    ) -> Result<T, awaken_session_contract::SessionRealizationControlFailure> {
        tokio::time::timeout(self.request_timeout, future)
            .await
            .map_err(|_| {
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(format!(
                    "Session realization Control `{operation}` exceeded its authority-derived request deadline"
                ))
            })?
    }
}

/// Deadline decoration only bounds transport waiting. The wrapped Control
/// remains the sole owner of leases, phases, cleanup commands, and receipts.
#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for DeadlineSessionRealizationControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("begin", self.inner.begin_session_realization(command))
            .await
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationLease,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("renew", self.inner.renew_session_realization(command))
            .await
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call("activate", self.inner.activate_session_realization(command))
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "acknowledge",
            self.inner.acknowledge_session_realization(command),
        )
        .await
    }

    async fn fail_session_realization(
        &self,
        command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call("fail", self.inner.fail_session_realization(command))
            .await
    }

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupAssignment>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "claim_terminal_cleanup",
            self.inner.claim_next_terminal_cleanup(target),
        )
        .await
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "poll_terminal_cleanup",
            self.inner.terminal_cleanup_work(session_id, lease),
        )
        .await
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "authorize_terminal_cleanup_effect",
            self.inner.authorize_terminal_cleanup_effect(effect),
        )
        .await
    }

    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "authorize_terminal_cleanup_disposal",
            self.inner.authorize_terminal_cleanup_disposal(effect),
        )
        .await
    }

    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "authorize_checkpoint_release_artifact_effect",
            self.inner
                .authorize_checkpoint_release_artifact_effect(session_id, operation),
        )
        .await
    }

    async fn authorize_terminal_memory_intent(
        &self,
        intent: &awaken_session_contract::SessionTerminalMemoryIntent,
    ) -> Result<
        awaken_session_contract::SessionTerminalMemoryTarget,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "authorize_terminal_memory_intent",
            self.inner.authorize_terminal_memory_intent(intent),
        )
        .await
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.call(
            "poll_terminal_repository_publication",
            self.inner
                .terminal_repository_publication_command(session_id, lease),
        )
        .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_repository_publication",
            self.inner
                .record_terminal_repository_publication_receipt(session_id, lease, receipt),
        )
        .await
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_repository_publication_rejection",
            self.inner
                .record_terminal_repository_publication_rejection(session_id, lease, rejection),
        )
        .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_cleanup_preparation",
            self.inner
                .record_terminal_cleanup_preparation(lease, preparation),
        )
        .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.call(
            "record_terminal_cleanup_disposal",
            self.inner.record_terminal_cleanup_disposal(lease, receipt),
        )
        .await
    }
}
