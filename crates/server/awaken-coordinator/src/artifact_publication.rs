//! Coordinator-local claim fencing for Runtime artifact publication.

use std::sync::{Arc, OnceLock};

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery,
};
use awaken_dispatch::{AnyDispatchStore, ArtifactPublicationFence, DispatchQueue};

/// Decorates the canonical Resources publisher with the same exact-epoch fence
/// used by the remote Worker endpoint. The Runtime Host therefore has one
/// topology-independent publication contract.
pub struct ClaimFencedArtifactPublisher {
    inner: Arc<dyn ArtifactPublisher<ArtifactPublicationFence>>,
    dispatch: Arc<AnyDispatchStore>,
    session_control: OnceLock<Arc<dyn awaken_session_contract::SessionRealizationControl>>,
}

impl ClaimFencedArtifactPublisher {
    #[must_use]
    pub fn new(
        inner: Arc<dyn ArtifactPublisher<ArtifactPublicationFence>>,
        dispatch: Arc<AnyDispatchStore>,
    ) -> Self {
        Self {
            inner,
            dispatch,
            session_control: OnceLock::new(),
        }
    }

    /// Complete the all-in-one composition cycle after the Session application
    /// has been constructed around the Runtime Host. The cell is write-once;
    /// it is only a reference to the existing aggregate authority, never a
    /// second cleanup state or cache.
    pub fn install_session_control(
        &self,
        control: Arc<dyn awaken_session_contract::SessionRealizationControl>,
    ) -> Result<(), &'static str> {
        self.session_control
            .set(control)
            .map_err(|_| "Session Artifact authority is already installed")
    }

    fn control(
        &self,
        effect_kind: &str,
    ) -> Result<
        &Arc<dyn awaken_session_contract::SessionRealizationControl>,
        ArtifactPublicationError,
    > {
        self.session_control.get().ok_or_else(|| {
            ArtifactPublicationError::new(format!(
                "{effect_kind} Artifact effect has no Session authority"
            ))
        })
    }

    fn control_for_live_lease(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        effect_kind: &str,
    ) -> Result<
        &Arc<dyn awaken_session_contract::SessionRealizationControl>,
        ArtifactPublicationError,
    > {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        if !awaken_session_contract::realization_lease_is_live_at(
            lease.expires_at_unix_ms,
            now_unix_ms,
        ) {
            return Err(ArtifactPublicationError::new(format!(
                "{effect_kind} Artifact effect is expired"
            )));
        }
        self.control(effect_kind)
    }

    async fn authorize_terminal_artifact(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        workspace_id: &str,
        session_id: &str,
        idempotency_scope: Option<&str>,
    ) -> Result<(), ArtifactPublicationError> {
        if idempotency_scope != Some(effect.operation_id()) {
            return Err(ArtifactPublicationError::new(
                "terminal Artifact scope does not match its cleanup operation",
            ));
        }
        let control = self.control("terminal")?;
        let authorization = control
            .authorize_terminal_cleanup_effect(effect)
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        authorization
            .verify_for(effect)
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        if authorization.workspace_id() != workspace_id || effect.command.thread_id != session_id {
            return Err(ArtifactPublicationError::new(
                "terminal Artifact effect is outside its Session authority",
            ));
        }
        Ok(())
    }

    async fn authorize_checkpoint_release_artifact(
        &self,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        workspace_id: &str,
        session_id: &str,
        idempotency_scope: Option<&str>,
    ) -> Result<(), ArtifactPublicationError> {
        if idempotency_scope.is_some() {
            return Err(ArtifactPublicationError::new(
                "checkpoint source-release Artifact cannot reserve terminal association scope",
            ));
        }
        let lease = operation.realization.as_ref().ok_or_else(|| {
            ArtifactPublicationError::new(
                "checkpoint source-release Artifact has no realization lease",
            )
        })?;
        let control = self.control_for_live_lease(lease, "checkpoint source-release")?;
        let authorized_workspace = control
            .authorize_checkpoint_release_artifact_effect(session_id, operation)
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        if authorized_workspace != workspace_id {
            return Err(ArtifactPublicationError::new(
                "checkpoint source-release Artifact is outside its Session authority",
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher<ArtifactPublicationFence> for ClaimFencedArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<ArtifactPublicationFence>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        let _guard = match publication.fence.as_ref() {
            Some(ArtifactPublicationFence::Run(claim)) => {
                if publication.idempotency_scope.is_some() {
                    return Err(ArtifactPublicationError::new(
                        "ordinary Artifact publication cannot use a terminal idempotency scope",
                    ));
                }
                Some(
                    self.dispatch
                        .lock_commit_epoch(claim)
                        .await
                        .map_err(|error| ArtifactPublicationError::new(error.to_string()))?
                        .ok_or_else(|| ArtifactPublicationError::new("dispatch claim is stale"))?,
                )
            }
            Some(ArtifactPublicationFence::CheckpointRelease(operation)) => {
                self.authorize_checkpoint_release_artifact(
                    operation,
                    &publication.workspace_id,
                    &publication.session_id,
                    publication.idempotency_scope.as_deref(),
                )
                .await?;
                None
            }
            Some(ArtifactPublicationFence::Terminal(effect)) => {
                self.authorize_terminal_artifact(
                    effect,
                    &publication.workspace_id,
                    &publication.session_id,
                    publication.idempotency_scope.as_deref(),
                )
                .await?;
                None
            }
            None if publication.idempotency_scope.is_none() => None,
            None => {
                return Err(ArtifactPublicationError::new(
                    "unfenced Artifact publication cannot use a terminal idempotency scope",
                ));
            }
        };
        self.inner.publish(publication).await
    }

    async fn recover(
        &self,
        recovery: ArtifactRecovery<ArtifactPublicationFence>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        let ArtifactPublicationFence::Terminal(effect) = &recovery.fence else {
            return Err(ArtifactPublicationError::new(
                "artifact recovery requires an exact terminal effect",
            ));
        };
        self.authorize_terminal_artifact(
            effect,
            &recovery.workspace_id,
            &recovery.session_id,
            Some(&recovery.idempotency_scope),
        )
        .await?;
        self.inner.recover(recovery).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_dispatch::{Dispatch, MemoryDispatchStore};
    use awaken_session_contract::{SessionRealizationControlFailure, SessionRealizationDirective};

    use super::*;

    struct RecordingPublisher {
        publications: AtomicUsize,
        recoveries: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ArtifactPublisher<ArtifactPublicationFence> for RecordingPublisher {
        async fn publish(
            &self,
            _publication: ArtifactPublication<ArtifactPublicationFence>,
        ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
            self.publications.fetch_add(1, Ordering::SeqCst);
            Err(ArtifactPublicationError::new(
                "authorized publication reached the inner owner",
            ))
        }

        async fn recover(
            &self,
            _recovery: ArtifactRecovery<ArtifactPublicationFence>,
        ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
            self.recoveries.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    struct ExactTerminalControl {
        workspace_id: String,
        asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect,
        current_lease: awaken_session_contract::SessionRealizationLease,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for ExactTerminalControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid("unused".into()))
        }

        async fn activate_session_realization(
            &self,
            _command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid("unused".into()))
        }

        async fn acknowledge_session_realization(
            &self,
            _command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid("unused".into()))
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid("unused".into()))
        }

        async fn authorize_terminal_cleanup_effect(
            &self,
            effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        ) -> Result<
            awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
            SessionRealizationControlFailure,
        > {
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            if effect != &self.asserted_effect
                || !awaken_session_contract::realization_lease_generation_authorizes(
                    &self.current_lease,
                    &effect.lease,
                )
                || !awaken_session_contract::realization_lease_is_live_at(
                    self.current_lease.expires_at_unix_ms,
                    now_unix_ms,
                )
            {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                self.workspace_id.clone(),
                None,
            )
        }
    }
}
