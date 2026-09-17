//! Coordinator-local claim fencing for Runtime artifact publication.

use std::sync::{Arc, OnceLock};

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery,
};
use awaken_dispatch::{AnyDispatchStore, ArtifactPublicationFence, DispatchQueue};

fn artifact_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

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

    async fn authorize_effect(
        &self,
        fence: Option<&ArtifactPublicationFence>,
        workspace_id: &str,
        session_id: &str,
        idempotency_scope: Option<&str>,
    ) -> Result<Option<awaken_dispatch::CommitEpochGuard>, ArtifactPublicationError> {
        let guard = match fence {
            Some(ArtifactPublicationFence::SessionMaterialization(_))
            | Some(ArtifactPublicationFence::RepositoryPublication { .. }) => {
                return Err(ArtifactPublicationError::new(
                    "Session Resource materialization cannot publish Artifacts",
                ));
            }
            Some(ArtifactPublicationFence::Run(claim)) => {
                if idempotency_scope.is_some() {
                    return Err(ArtifactPublicationError::new(
                        "ordinary Artifact publication cannot use a terminal idempotency scope",
                    ));
                }
                let guard = self
                    .dispatch
                    .lock_commit_epoch(claim)
                    .await
                    .map_err(|error| ArtifactPublicationError::new(error.to_string()))?
                    .filter(|guard| guard.is_live_at(artifact_now_ms()))
                    .ok_or_else(|| ArtifactPublicationError::new("dispatch claim is stale"))?;
                if !guard.matches_execution_scope(workspace_id, session_id) {
                    return Err(ArtifactPublicationError::new(
                        "artifact is outside its dispatch scope",
                    ));
                }
                Some(guard)
            }
            Some(ArtifactPublicationFence::CheckpointRelease(operation)) => {
                self.authorize_checkpoint_release_artifact(
                    operation,
                    workspace_id,
                    &awaken_session_contract::SessionId::from(session_id),
                    idempotency_scope,
                )
                .await?;
                None
            }
            Some(ArtifactPublicationFence::Terminal(effect)) => {
                self.authorize_terminal_artifact(
                    effect,
                    workspace_id,
                    session_id,
                    idempotency_scope,
                )
                .await?;
                None
            }
            None if idempotency_scope.is_none() => None,
            None => {
                return Err(ArtifactPublicationError::new(
                    "unfenced Artifact publication cannot use a terminal idempotency scope",
                ));
            }
        };
        Ok(guard)
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
        let now_unix_ms = artifact_now_ms();
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
        session_id: &awaken_session_contract::SessionId,
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
    async fn capture(
        &self,
        workspace_id: &str,
        session_id: &str,
        fence: Option<&ArtifactPublicationFence>,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError> {
        let scope = match fence {
            Some(ArtifactPublicationFence::Terminal(effect)) => Some(effect.operation_id()),
            _ => None,
        };
        let _guard = self
            .authorize_effect(fence, workspace_id, session_id, scope)
            .await?;
        self.inner.capture(workspace_id, session_id, fence).await
    }

    async fn replace_capture(
        &self,
        request: awaken_resource_contract::ReplaceArtifactCapture,
        fence: Option<&ArtifactPublicationFence>,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
    {
        let scope = match fence {
            Some(ArtifactPublicationFence::Terminal(effect)) => Some(effect.operation_id()),
            _ => None,
        };
        let _guard = self
            .authorize_effect(fence, &request.workspace_id, &request.session_id, scope)
            .await?;
        self.inner.replace_capture(request, fence).await
    }

    async fn publish(
        &self,
        publication: ArtifactPublication<ArtifactPublicationFence>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        let _guard = self
            .authorize_effect(
                publication.fence.as_ref(),
                &publication.workspace_id,
                &publication.session_id,
                publication.idempotency_scope.as_deref(),
            )
            .await?;
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

    #[derive(Default)]
    struct RecordingPublisher {
        publications: AtomicUsize,
        recoveries: AtomicUsize,
        capture_reads: AtomicUsize,
        capture_replaces: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ArtifactPublisher<ArtifactPublicationFence> for RecordingPublisher {
        async fn capture(
            &self,
            _workspace: &str,
            _session: &str,
            _fence: Option<&ArtifactPublicationFence>,
        ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError>
        {
            self.capture_reads.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        async fn replace_capture(
            &self,
            request: awaken_resource_contract::ReplaceArtifactCapture,
            _fence: Option<&ArtifactPublicationFence>,
        ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
        {
            self.capture_replaces.fetch_add(1, Ordering::SeqCst);
            request
                .decide(None)
                .map_err(|error| ArtifactPublicationError::new(error.to_string()))
        }

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

    struct ExactResourceControl {
        workspace_id: String,
        asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect,
        current_lease: awaken_session_contract::SessionRealizationLease,
        checkpoint: Option<awaken_session_contract::SessionEnvironmentOperation>,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for ExactResourceControl {
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

        async fn authorize_checkpoint_release_artifact_effect(
            &self,
            session: &awaken_session_contract::SessionId,
            operation: &awaken_session_contract::SessionEnvironmentOperation,
        ) -> Result<String, SessionRealizationControlFailure> {
            if *session != self.asserted_effect.command.thread_id
                || self.checkpoint.as_ref() != Some(operation)
            {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            Ok(self.workspace_id.clone())
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

    #[tokio::test]
    async fn terminal_artifacts_use_the_current_root_for_an_expired_asserted_generation() {
        // Cause/effect graph: C1 the terminal effect assertion is exact but its
        // original timestamp elapsed; C2 Control observes the current root as a
        // live same-generation renewal, an expired predecessor, or a foreign
        // epoch; C3 Workspace/Session/scope are exact or foreign; C4 the edge is
        // publication or recovery. Effects: E1 C1+C2(live)+C3(exact) delegates
        // both edges exactly once; E2 expired-current, foreign-generation, or
        // foreign scope facts fail before either inner effect. Control remains
        // the one current-root clock authority; this local topology wrapper
        // must not reintroduce an asserted-expiry decision beside it.
        //
        // | Rule | asserted | current root | scope | Edge | Effect |
        // |---|---|---|---|---|---|
        // | T1 | expired | same generation live | exact | publish+recover | E1 |
        // | T2 | expired | same generation expired | exact | publish+recover | E2 |
        // | T3 | successor epoch | predecessor live | exact | publish+recover | E2 |
        // | T4 | expired | same generation live | foreign | publish+recover | E2 |
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let asserted = awaken_session_contract::SessionTerminalCleanupEffect::new(
            awaken_session_contract::SessionCleanupCommand::new("session", "session", "cleanup"),
            awaken_session_contract::SessionRealizationLease {
                owner: "coordinator".into(),
                runtime_incarnation: "coordinator:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
            },
        );
        let dispatch = Arc::new(AnyDispatchStore::from_dispatch(
            Arc::new(MemoryDispatchStore::new()) as Arc<dyn Dispatch>,
        ));
        let publisher_for = |asserted_effect, current_lease| {
            let inner = Arc::new(RecordingPublisher::default());
            let publisher = ClaimFencedArtifactPublisher::new(inner.clone(), dispatch.clone());
            publisher
                .install_session_control(Arc::new(ExactResourceControl {
                    workspace_id: "workspace".into(),
                    asserted_effect,
                    current_lease,
                    checkpoint: None,
                }))
                .expect("install exact root authority");
            (publisher, inner)
        };
        let recovery = |workspace_id: &str, scope: &str, effect| ArtifactRecovery {
            workspace_id: workspace_id.into(),
            session_id: "session".into(),
            idempotency_scope: scope.into(),
            fence: ArtifactPublicationFence::Terminal(effect),
        };
        let publication = |workspace_id: &str, scope: &str, effect| ArtifactPublication {
            effect_id: "artifact-effect".into(),
            workspace_id: workspace_id.into(),
            session_id: "session".into(),
            logical_path: "result.txt".into(),
            mime_type: "text/plain".into(),
            content_id: "artifact-content".into(),
            bytes: b"artifact".to_vec(),
            idempotency_scope: Some(scope.into()),
            fence: Some(ArtifactPublicationFence::Terminal(effect)),
        };

        let renewed = awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(60_000),
            ..asserted.lease.clone()
        };
        let (publisher, inner) = publisher_for(asserted.clone(), renewed.clone());
        publisher
            .recover(recovery(
                "workspace",
                asserted.operation_id(),
                asserted.clone(),
            ))
            .await
            .expect("T1/E1 exact recovery through current renewal");
        assert!(
            publisher
                .publish(publication(
                    "workspace",
                    asserted.operation_id(),
                    asserted.clone(),
                ))
                .await
                .expect_err("T1/E1 authorized publication reaches the recording inner")
                .to_string()
                .contains("inner owner"),
            "T1/E1"
        );
        assert_eq!(inner.recoveries.load(Ordering::SeqCst), 1, "T1/E1");
        assert_eq!(inner.publications.load(Ordering::SeqCst), 1, "T1/E1");

        assert!(
            publisher
                .recover(recovery(
                    "foreign-workspace",
                    asserted.operation_id(),
                    asserted.clone(),
                ))
                .await
                .is_err(),
            "T4/E2 foreign Workspace"
        );
        assert!(
            publisher
                .publish(publication("workspace", "foreign-scope", asserted.clone(),))
                .await
                .is_err(),
            "T4/E2 foreign scope"
        );
        assert_eq!(inner.recoveries.load(Ordering::SeqCst), 1, "T4/E2");
        assert_eq!(inner.publications.load(Ordering::SeqCst), 1, "T4/E2");

        let (expired_publisher, expired_inner) =
            publisher_for(asserted.clone(), asserted.lease.clone());
        assert!(
            expired_publisher
                .recover(recovery(
                    "workspace",
                    asserted.operation_id(),
                    asserted.clone(),
                ))
                .await
                .is_err(),
            "T2/E2 expired current root"
        );
        assert!(
            expired_publisher
                .publish(publication(
                    "workspace",
                    asserted.operation_id(),
                    asserted.clone(),
                ))
                .await
                .is_err(),
            "T2/E2 expired current root"
        );
        assert_eq!(expired_inner.recoveries.load(Ordering::SeqCst), 0, "T2/E2");
        assert_eq!(
            expired_inner.publications.load(Ordering::SeqCst),
            0,
            "T2/E2"
        );

        let foreign = awaken_session_contract::SessionTerminalCleanupEffect::new(
            asserted.command.clone(),
            awaken_session_contract::SessionRealizationLease {
                epoch: asserted.lease.epoch + 1,
                ..asserted.lease.clone()
            },
        );
        let (foreign_publisher, foreign_inner) = publisher_for(foreign.clone(), renewed);
        assert!(
            foreign_publisher
                .recover(recovery(
                    "workspace",
                    foreign.operation_id(),
                    foreign.clone(),
                ))
                .await
                .is_err(),
            "T3/E2 foreign generation"
        );
        assert!(
            foreign_publisher
                .publish(publication(
                    "workspace",
                    foreign.operation_id(),
                    foreign.clone(),
                ))
                .await
                .is_err(),
            "T3/E2 foreign generation"
        );
        assert_eq!(
            foreign_inner.recoveries.load(Ordering::SeqCst),
            0,
            "T3/E2 zero delegated read"
        );
        assert_eq!(
            foreign_inner.publications.load(Ordering::SeqCst),
            0,
            "T3/E2 zero delegated write"
        );
        // T5 the two capture edges follow the same terminal generation rules.
        // Exact renewed root reaches the inner owner; expired/foreign roots and
        // foreign Workspace do not even read the capture catalog.
        let command = awaken_resource_contract::ReplaceArtifactCapture {
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            expected_revision: 0,
            file_ids: Vec::new(),
        };
        let exact_fence = ArtifactPublicationFence::Terminal(asserted.clone());
        assert!(
            publisher
                .capture("workspace", "session", Some(&exact_fence))
                .await
                .is_ok()
        );
        assert!(
            publisher
                .replace_capture(command.clone(), Some(&exact_fence))
                .await
                .is_ok()
        );
        assert!(
            publisher
                .capture("foreign", "session", Some(&exact_fence))
                .await
                .is_err()
        );
        let mut wrong = command.clone();
        wrong.workspace_id = "foreign".into();
        assert!(
            publisher
                .replace_capture(wrong, Some(&exact_fence))
                .await
                .is_err()
        );
        for (peer, observed, fence) in [
            (&expired_publisher, &expired_inner, exact_fence),
            (
                &foreign_publisher,
                &foreign_inner,
                ArtifactPublicationFence::Terminal(foreign),
            ),
        ] {
            assert!(
                peer.capture("workspace", "session", Some(&fence))
                    .await
                    .is_err(),
                "T5"
            );
            assert!(
                peer.replace_capture(command.clone(), Some(&fence))
                    .await
                    .is_err(),
                "T5"
            );
            assert_eq!(observed.capture_reads.load(Ordering::SeqCst), 0, "T5");
            assert_eq!(observed.capture_replaces.load(Ordering::SeqCst), 0, "T5");
        }
        assert_eq!(inner.capture_reads.load(Ordering::SeqCst), 1, "T5");
        assert_eq!(inner.capture_replaces.load(Ordering::SeqCst), 1, "T5");
    }
    #[tokio::test]
    async fn local_run_effects_match_the_locked_scope_and_live_epoch() {
        // L1 exact live claim + exact scope -> delegate publication/read/CAS.
        // L2 independently falsify Workspace, Session, epoch, owner, scope
        // presence, and lease liveness -> all three inner I/O counts stay zero.
        // Local dispatch fencing must have the same scope/expiry contract as
        // registered Worker HTTP; a matching epoch alone grants no other scope.
        use awaken_dispatch_testkit::worker_http as support;
        for cause in [
            "exact",
            "workspace",
            "session",
            "epoch",
            "owner",
            "missing_scope",
            "expired",
            "child_parent",
            "child",
        ] {
            let queue = Arc::new(MemoryDispatchStore::new());
            let mut dispatch =
                awaken_dispatch::RunDispatch::new(support::activation("capture-local"));
            let session = dispatch.thread_id().0.clone();
            // L3 a child's recovery affinity names the parent, but its output
            // File scope is its own execution Thread. Parent publication must
            // remain forbidden even though the affinity matches that parent.
            if cause == "child" || cause == "child_parent" {
                dispatch = dispatch.for_session(awaken_agent_contract::agent::thread::Id(
                    "parent-session".into(),
                ));
            }
            if cause != "missing_scope" {
                dispatch = dispatch.with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace"),
                ));
            }
            queue.enqueue(dispatch).await.unwrap();
            let now = if cause == "expired" {
                0
            } else {
                artifact_now_ms()
            };
            let claimed = queue
                .claim("owner", 60_000, now, &Default::default())
                .await
                .unwrap()
                .unwrap();
            let mut claim = awaken_dispatch::RunClaim::from(&claimed.lease);
            if cause == "epoch" {
                claim.epoch += 1;
            }
            if cause == "owner" {
                claim.owner = "foreign".into();
            }
            let workspace = if cause == "workspace" {
                "foreign"
            } else {
                "workspace"
            };
            let session = if cause == "session" {
                "foreign"
            } else if cause == "child_parent" {
                "parent-session"
            } else {
                &session
            };
            let inner = Arc::new(RecordingPublisher::default());
            let peer = ClaimFencedArtifactPublisher::new(
                inner.clone(),
                Arc::new(AnyDispatchStore::from_dispatch(queue as Arc<dyn Dispatch>)),
            );
            let fence = ArtifactPublicationFence::Run(claim);
            let bytes = b"result".to_vec();
            let digest = awaken_resource_contract::content_id(&bytes);
            let publication = ArtifactPublication {
                effect_id: awaken_resource_contract::harvest_idempotency_key(
                    session,
                    "result.txt",
                    &digest,
                ),
                workspace_id: workspace.into(),
                session_id: session.into(),
                logical_path: "result.txt".into(),
                mime_type: "text/plain".into(),
                content_id: digest,
                bytes,
                idempotency_scope: None,
                fence: Some(fence.clone()),
            };
            // This recording inner intentionally rejects publish after counting.
            let _ = peer.publish(publication).await;
            let _ = peer.capture(workspace, session, Some(&fence)).await;
            let _ = peer
                .replace_capture(
                    awaken_resource_contract::ReplaceArtifactCapture {
                        workspace_id: workspace.into(),
                        session_id: session.into(),
                        expected_revision: 0,
                        file_ids: Vec::new(),
                    },
                    Some(&fence),
                )
                .await;
            let expected = usize::from(cause == "exact" || cause == "child");
            assert_eq!(
                inner.publications.load(Ordering::SeqCst),
                expected,
                "L1/L2 {cause}"
            );
            assert_eq!(
                inner.capture_reads.load(Ordering::SeqCst),
                expected,
                "L1/L2 {cause}"
            );
            assert_eq!(
                inner.capture_replaces.load(Ordering::SeqCst),
                expected,
                "L1/L2 {cause}"
            );
        }
    }
    #[tokio::test]
    async fn local_checkpoint_capture_requires_exact_live_root_authority() {
        // K1 exact live checkpoint root -> publication/read/CAS delegate.
        // K2 unavailable authority, changed operation, absent/expired lease or
        // foreign scope -> zero inner effects. The control fixture accepts an
        // exact expired assertion, so removing the adapter's live-lease check
        // would turn the expired row green incorrectly.
        for cause in [
            "exact",
            "unavailable",
            "changed",
            "expired",
            "missing_lease",
            "workspace",
            "session",
        ] {
            let lease = awaken_session_contract::SessionRealizationLease {
                owner: "owner".into(),
                runtime_incarnation: "owner:boot".into(),
                epoch: 1,
                expires_at_unix_ms: if cause == "expired" {
                    0
                } else {
                    artifact_now_ms() + 60_000
                },
            };
            let generation = awaken_session_contract::SandboxGeneration {
                id: "generation".into(),
                created_at_unix_ms: 0,
                expires_at_unix_ms: u64::MAX,
                environment_fingerprint: "environment".into(),
                base_image_fingerprint: "base".into(),
            };
            let mut operation = awaken_session_contract::SessionEnvironmentOperation::new(
                "workspace",
                "session",
                "suspend",
                &generation,
                1,
                if cause == "missing_lease" {
                    None
                } else {
                    Some(lease.clone())
                },
                None,
            );
            let inner = Arc::new(RecordingPublisher::default());
            let queue = Arc::new(AnyDispatchStore::from_dispatch(
                Arc::new(MemoryDispatchStore::new()) as Arc<dyn Dispatch>,
            ));
            let peer = ClaimFencedArtifactPublisher::new(inner.clone(), queue);
            if cause != "unavailable" {
                peer.install_session_control(Arc::new(ExactResourceControl {
                    workspace_id: "workspace".into(),
                    asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect::new(
                        awaken_session_contract::SessionCleanupCommand::new(
                            "session", "session", "cleanup",
                        ),
                        lease.clone(),
                    ),
                    current_lease: lease,
                    checkpoint: Some(operation.clone()),
                }))
                .unwrap();
            }
            if cause == "changed" {
                operation.effect_id.push_str("-changed");
            }
            let fence = ArtifactPublicationFence::CheckpointRelease(operation);
            let workspace = if cause == "workspace" {
                "foreign"
            } else {
                "workspace"
            };
            let session = if cause == "session" {
                "foreign"
            } else {
                "session"
            };
            let _ = peer
                .publish(ArtifactPublication {
                    effect_id: "fixture".into(),
                    workspace_id: workspace.into(),
                    session_id: session.into(),
                    logical_path: "out.txt".into(),
                    mime_type: "text/plain".into(),
                    content_id: "fixture".into(),
                    bytes: Vec::new(),
                    idempotency_scope: None,
                    fence: Some(fence.clone()),
                })
                .await;
            let _ = peer.capture(workspace, session, Some(&fence)).await;
            let _ = peer
                .replace_capture(
                    awaken_resource_contract::ReplaceArtifactCapture {
                        workspace_id: workspace.into(),
                        session_id: session.into(),
                        expected_revision: 0,
                        file_ids: Vec::new(),
                    },
                    Some(&fence),
                )
                .await;
            let expected = usize::from(cause == "exact");
            assert_eq!(
                inner.publications.load(Ordering::SeqCst),
                expected,
                "K1/K2 {cause}"
            );
            assert_eq!(
                inner.capture_reads.load(Ordering::SeqCst),
                expected,
                "K1/K2 {cause}"
            );
            assert_eq!(
                inner.capture_replaces.load(Ordering::SeqCst),
                expected,
                "K1/K2 {cause}"
            );
        }
    }
}
