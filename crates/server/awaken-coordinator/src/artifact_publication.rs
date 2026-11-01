// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
//! Coordinator-local claim fencing for Runtime artifact publication.

use std::sync::{Arc, OnceLock};

use awaken_dispatch::DispatchCommitAuthority;
use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery,
};
use awaken_session_execution_contract::ResourceOperationFence;

use awaken_time::WallClockExt as _;

/// Decorates the canonical Resources publisher with the same exact-epoch fence
/// used by the remote Worker endpoint. The Runtime Host therefore has one
/// topology-independent publication contract.
pub struct ClaimFencedArtifactPublisher {
    inner: Arc<dyn ArtifactPublisher<ResourceOperationFence>>,
    dispatch: Arc<dyn DispatchCommitAuthority>,
    terminal_preparation_control:
        OnceLock<Arc<dyn awaken_session_execution_contract::SessionTerminalPreparationControl>>,
    checkpoint_artifact_control:
        OnceLock<Arc<dyn awaken_session_execution_contract::SessionCheckpointArtifactControl>>,
    /// The Coordinator's wall clock: claim and Session lease liveness are
    /// judged on it.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl ClaimFencedArtifactPublisher {
    #[must_use]
    pub fn new(
        inner: Arc<dyn ArtifactPublisher<ResourceOperationFence>>,
        dispatch: Arc<dyn DispatchCommitAuthority>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            inner,
            dispatch,
            terminal_preparation_control: OnceLock::new(),
            checkpoint_artifact_control: OnceLock::new(),
            clock,
        }
    }

    /// Complete the all-in-one composition after the Session application and
    /// its shared mutation Core have been selected. Each original role cell is
    /// write-once; neither stores a second cleanup state or receipt cache.
    pub fn install_session_control(
        &self,
        terminal: Arc<dyn awaken_session_execution_contract::SessionTerminalPreparationControl>,
        checkpoint: Arc<dyn awaken_session_execution_contract::SessionCheckpointArtifactControl>,
    ) -> Result<(), &'static str> {
        self.terminal_preparation_control
            .set(terminal)
            .map_err(|_| "Session terminal Artifact authority is already installed")?;
        self.checkpoint_artifact_control
            .set(checkpoint)
            .map_err(|_| "Session checkpoint Artifact authority is already installed")
    }

    async fn authorize_effect(
        &self,
        fence: Option<&ResourceOperationFence>,
        workspace_id: &str,
        session_id: &str,
        idempotency_scope: Option<&str>,
    ) -> Result<Option<awaken_dispatch::CommitEpochGuard>, ArtifactPublicationError> {
        let guard = match fence {
            Some(ResourceOperationFence::SessionMaterialization(_))
            | Some(ResourceOperationFence::RepositoryPublication { .. }) => {
                return Err(ArtifactPublicationError::InvalidRequest {
                    detail: "Session Resource materialization cannot publish Artifacts".into(),
                });
            }
            Some(ResourceOperationFence::Run(claim)) => {
                if idempotency_scope.is_some() {
                    return Err(ArtifactPublicationError::InvalidRequest {
                        detail:
                            "ordinary Artifact publication cannot use a terminal idempotency scope"
                                .into(),
                    });
                }
                let guard = self
                    .dispatch
                    .lock_commit_epoch(claim)
                    .await
                    .map_err(|error| ArtifactPublicationError::new(error.to_string()))?
                    .filter(|guard| guard.is_live_at(self.clock.now_ms()))
                    .ok_or_else(|| ArtifactPublicationError::Conflict {
                        detail: "dispatch claim is stale".into(),
                    })?;
                if !guard.matches_execution_scope(workspace_id, session_id) {
                    return Err(ArtifactPublicationError::Denied {
                        detail: "artifact is outside its dispatch scope".into(),
                    });
                }
                Some(guard)
            }
            Some(ResourceOperationFence::CheckpointRelease(operation)) => {
                self.authorize_checkpoint_release_artifact(
                    operation,
                    workspace_id,
                    &awaken_session_types::SessionId::from(session_id),
                    idempotency_scope,
                )
                .await?;
                None
            }
            Some(ResourceOperationFence::Terminal(effect)) => {
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
                return Err(ArtifactPublicationError::InvalidRequest {
                    detail: "unfenced Artifact publication cannot use a terminal idempotency scope"
                        .into(),
                });
            }
        };
        Ok(guard)
    }

    fn terminal_control(
        &self,
        effect_kind: &str,
    ) -> Result<
        &Arc<dyn awaken_session_execution_contract::SessionTerminalPreparationControl>,
        ArtifactPublicationError,
    > {
        self.terminal_preparation_control.get().ok_or_else(|| {
            ArtifactPublicationError::AuthorityUnavailable {
                detail: format!("{effect_kind} Artifact effect has no Session authority"),
            }
        })
    }

    fn checkpoint_control_for_live_lease(
        &self,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        effect_kind: &str,
    ) -> Result<
        &Arc<dyn awaken_session_execution_contract::SessionCheckpointArtifactControl>,
        ArtifactPublicationError,
    > {
        let now_unix_ms = self.clock.now_ms();
        if !awaken_session_configuration_contract::resource::realization_lease_is_live_at(
            lease.expires_at_unix_ms,
            now_unix_ms,
        ) {
            return Err(ArtifactPublicationError::Conflict {
                detail: format!("{effect_kind} Artifact effect is expired"),
            });
        }
        self.checkpoint_artifact_control.get().ok_or_else(|| {
            ArtifactPublicationError::AuthorityUnavailable {
                detail: format!("{effect_kind} Artifact effect has no Session authority"),
            }
        })
    }

    async fn authorize_terminal_artifact(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        workspace_id: &str,
        session_id: &str,
        idempotency_scope: Option<&str>,
    ) -> Result<(), ArtifactPublicationError> {
        if idempotency_scope != Some(effect.operation_id()) {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "terminal Artifact scope does not match its cleanup operation".into(),
            });
        }
        let control = self.terminal_control("terminal")?;
        let authorization = control
            .authorize_terminal_cleanup_effect(effect)
            .await
            .map_err(control_publication_error)?;
        authorization
            .verify_for(effect)
            .map_err(control_publication_error)?;
        if authorization.workspace_id() != workspace_id || effect.command.thread_id != session_id {
            return Err(ArtifactPublicationError::Denied {
                detail: "terminal Artifact effect is outside its Session authority".into(),
            });
        }
        Ok(())
    }

    async fn authorize_checkpoint_release_artifact(
        &self,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency_scope: Option<&str>,
    ) -> Result<(), ArtifactPublicationError> {
        if idempotency_scope.is_some() {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail:
                    "checkpoint source-release Artifact cannot reserve terminal association scope"
                        .into(),
            });
        }
        let lease =
            operation
                .realization()
                .ok_or_else(|| ArtifactPublicationError::InvalidRequest {
                    detail: "checkpoint source-release Artifact has no realization lease".into(),
                })?;
        let control = self.checkpoint_control_for_live_lease(lease, "checkpoint source-release")?;
        let authorized_workspace = control
            .authorize_checkpoint_release_artifact_effect(session_id, operation)
            .await
            .map_err(control_publication_error)?;
        if authorized_workspace != workspace_id {
            return Err(ArtifactPublicationError::Denied {
                detail: "checkpoint source-release Artifact is outside its Session authority"
                    .into(),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher<ResourceOperationFence> for ClaimFencedArtifactPublisher {
    async fn capture(
        &self,
        workspace_id: &str,
        session_id: &str,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError> {
        let scope = match fence {
            Some(ResourceOperationFence::Terminal(effect)) => Some(effect.operation_id()),
            _ => None,
        };
        let _guard = self
            .authorize_effect(fence, workspace_id, session_id, scope)
            .await?;
        self.inner.capture(workspace_id, session_id, fence).await
    }

    async fn replace_capture(
        &self,
        mut request: awaken_resource_contract::ReplaceArtifactCapture,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
    {
        let scope = match fence {
            Some(ResourceOperationFence::Terminal(effect)) => Some(effect.operation_id()),
            _ => None,
        };
        let _guard = self
            .authorize_effect(fence, &request.workspace_id, &request.session_id, scope)
            .await?;
        request.bind_run_origin(fence.and_then(ResourceOperationFence::run_claim_identity))?;
        self.inner.replace_capture(request, fence).await
    }

    async fn publish(
        &self,
        publication: ArtifactPublication<ResourceOperationFence>,
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
        recovery: ArtifactRecovery<ResourceOperationFence>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        let ResourceOperationFence::Terminal(effect) = &recovery.fence else {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "artifact recovery requires an exact terminal effect".into(),
            });
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

fn control_publication_error(
    error: awaken_session_execution_contract::SessionRealizationControlFailure,
) -> ArtifactPublicationError {
    use awaken_session_execution_contract::SessionRealizationControlFailure as Failure;
    let detail = error.to_string();
    match error {
        Failure::Unavailable(_) => ArtifactPublicationError::AuthorityUnavailable { detail },
        _ => ArtifactPublicationError::Conflict { detail },
    }
}

#[cfg(test)]
mod tests {
    use awaken_dispatch::DispatchClaims;
    use awaken_dispatch::DispatchEnqueue;
    /// The tests' host-clock reading, the clock their publishers use.
    fn artifact_now_ms() -> u64 {
        awaken_time::WallClockExt::now_ms(awaken_time::system_clock().as_ref())
    }
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_dispatch::MemoryDispatchStore;
    use awaken_session_execution_contract::SessionRealizationControlFailure;

    use super::*;

    #[derive(Default)]
    struct RecordingPublisher {
        publications: AtomicUsize,
        recoveries: AtomicUsize,
        capture_reads: AtomicUsize,
        capture_replaces: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ArtifactPublisher<ResourceOperationFence> for RecordingPublisher {
        async fn capture(
            &self,
            _workspace: &str,
            _session: &str,
            _fence: Option<&ResourceOperationFence>,
        ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError>
        {
            self.capture_reads.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
        async fn replace_capture(
            &self,
            request: awaken_resource_contract::ReplaceArtifactCapture,
            _fence: Option<&ResourceOperationFence>,
        ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
        {
            self.capture_replaces.fetch_add(1, Ordering::SeqCst);
            request
                .decide(None)
                .map_err(|error| ArtifactPublicationError::new(error.to_string()))
        }

        async fn publish(
            &self,
            _publication: ArtifactPublication<ResourceOperationFence>,
        ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
            self.publications.fetch_add(1, Ordering::SeqCst);
            Err(ArtifactPublicationError::new(
                "authorized publication reached the inner owner",
            ))
        }

        async fn recover(
            &self,
            _recovery: ArtifactRecovery<ResourceOperationFence>,
        ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
            self.recoveries.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    struct ExactResourceControl {
        workspace_id: String,
        asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect,
        current_lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
        checkpoint: Option<awaken_session_contract::SessionEnvironmentOperation>,
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionCheckpointArtifactControl for ExactResourceControl {
        async fn authorize_checkpoint_release_artifact_effect(
            &self,
            session: &awaken_session_types::SessionId,
            operation: &awaken_session_contract::SessionEnvironmentOperation,
        ) -> Result<String, SessionRealizationControlFailure> {
            if *session != self.asserted_effect.command.thread_id
                || self.checkpoint.as_ref() != Some(operation)
            {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            Ok(self.workspace_id.clone())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionTerminalPreparationControl for ExactResourceControl {
        async fn authorize_terminal_cleanup_effect(
            &self,
            effect: &awaken_session_contract::SessionTerminalCleanupEffect,
        ) -> Result<
            awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization,
            SessionRealizationControlFailure,
        > {
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            if effect != &self.asserted_effect
                || !awaken_session_configuration_contract::resource::realization_lease_generation_authorizes(
                    &self.current_lease,
                    &effect.lease,
                )
                || !awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                    self.current_lease.expires_at_unix_ms,
                    now_unix_ms,
                )
            {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                self.workspace_id.clone(),
                None,
            )
        }

        async fn record_terminal_cleanup_preparation(
            &self,
            _lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
            _preparation: awaken_session_contract::SessionCleanupPreparation,
        ) -> Result<(), SessionRealizationControlFailure> {
            Err(SessionRealizationControlFailure::Invalid(
                "Artifact authorization fixture does not admit cleanup receipts".into(),
            ))
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
            awaken_session_contract::SessionCleanupCommand::new(
                &awaken_session_types::SessionId::from("session"),
                "session",
                "cleanup",
            ),
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "coordinator".into(),
                runtime_incarnation: "coordinator:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
            },
        );
        let dispatch = Arc::new(
            awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
                MemoryDispatchStore::new(awaken_time::system_clock()),
            )
                as Arc<dyn awaken_dispatch::DurableDispatch>),
        );
        let publisher_for = |asserted_effect, current_lease| {
            let inner = Arc::new(RecordingPublisher::default());
            let publisher = ClaimFencedArtifactPublisher::new(
                inner.clone(),
                dispatch.clone(),
                awaken_time::system_clock(),
            );
            let control = Arc::new(ExactResourceControl {
                workspace_id: "workspace".into(),
                asserted_effect,
                current_lease,
                checkpoint: None,
            });
            publisher
                .install_session_control(control.clone(), control)
                .expect("install exact root authority");
            (publisher, inner)
        };
        let recovery = |workspace_id: &str, scope: &str, effect| ArtifactRecovery {
            workspace_id: workspace_id.into(),
            session_id: "session".into(),
            idempotency_scope: scope.into(),
            fence: ResourceOperationFence::Terminal(effect),
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
            fence: Some(ResourceOperationFence::Terminal(effect)),
        };

        let renewed = awaken_session_configuration_contract::resource::SessionRealizationLease {
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
            awaken_session_configuration_contract::resource::SessionRealizationLease {
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
            origin: None,
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            expected_revision: 0,
            file_ids: Vec::new(),
        };
        let exact_fence = ResourceOperationFence::Terminal(asserted.clone());
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
                ResourceOperationFence::Terminal(foreign),
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
            // Lease liveness is judged by the store's own clock now, so the
            // expired case mints its claim in the distant past and then returns
            // the clock to the present.
            let clock = Arc::new(awaken_lease_testkit::ManualClock::new(artifact_now_ms()));
            let queue = Arc::new(MemoryDispatchStore::new(clock.clone()));
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
                dispatch = dispatch.with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
                    awaken_agent_contract::ScopeId::from("workspace"),
                ));
            }
            queue.enqueue(dispatch).await.unwrap();
            if cause == "expired" {
                clock.set(0);
            }
            let claimed = queue
                .claim("owner", 60_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            if cause == "expired" {
                clock.set(artifact_now_ms());
            }
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
                Arc::new(
                    awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(
                        queue as Arc<dyn awaken_dispatch::DurableDispatch>,
                    ),
                ),
                awaken_time::system_clock(),
            );
            let fence = ResourceOperationFence::Run(claim);
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
                        origin: None,
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
            let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "owner".into(),
                runtime_incarnation: "owner:boot".into(),
                epoch: 1,
                expires_at_unix_ms: if cause == "expired" {
                    0
                } else {
                    artifact_now_ms() + 60_000
                },
            };
            let generation = awaken_session_contract::SandboxGeneration::new(
                &awaken_session_types::SessionId::from("session"),
                0,
                u64::MAX,
                "environment",
                "base",
            );
            let mut operation = awaken_session_contract::SessionEnvironmentOperation::new(
                "workspace",
                &awaken_session_types::SessionId::from("session"),
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
            let queue = Arc::new(
                awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
                    MemoryDispatchStore::new(awaken_time::system_clock()),
                )
                    as Arc<dyn awaken_dispatch::DurableDispatch>),
            );
            let peer = ClaimFencedArtifactPublisher::new(
                inner.clone(),
                queue,
                awaken_time::system_clock(),
            );
            if cause != "unavailable" {
                let control = Arc::new(ExactResourceControl {
                    workspace_id: "workspace".into(),
                    asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect::new(
                        awaken_session_contract::SessionCleanupCommand::new(
                            &awaken_session_types::SessionId::from("session"),
                            "session",
                            "cleanup",
                        ),
                        lease.clone(),
                    ),
                    current_lease: lease,
                    checkpoint: Some(operation.clone()),
                });
                peer.install_session_control(control.clone(), control)
                    .unwrap();
            }
            if cause == "changed" {
                // Existing changed-authority row: keep the same corrupted wire,
                // without recomputing the operation identity through `new`.
                let mut wire = serde_json::to_value(&operation).expect("operation fixture");
                wire["effect_id"] = serde_json::json!(format!("{}-changed", operation.effect_id()));
                operation = serde_json::from_value(wire).expect("changed operation fixture");
            }
            let fence = ResourceOperationFence::CheckpointRelease(operation);
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
                        origin: None,
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
    #[test]
    fn artifact_role_installation_preserves_original_order_and_duplicate_failure() {
        // C1 terminal cell already installed; C2 checkpoint cell installed.
        // Effects: E1 original first-cell refusal leaves checkpoint untouched;
        // E2 original second-cell refusal retains the newly installed terminal;
        // E3 both new role identities are installed. Decision table:
        // C1 + (C2=*) => E1; !C1+C2 => E2; !C1+!C2 => E3.
        // Installing two original role values
        // cannot add rollback, a second observer, or fallback aggregate control.
        for (terminal_bound, checkpoint_bound) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let make_control = || {
                let lease =
                    awaken_session_configuration_contract::resource::SessionRealizationLease {
                        owner: "coordinator".into(),
                        runtime_incarnation: "coordinator:incarnation".into(),
                        epoch: 1,
                        expires_at_unix_ms: 10_000,
                    };
                Arc::new(ExactResourceControl {
                    workspace_id: "workspace".into(),
                    asserted_effect: awaken_session_contract::SessionTerminalCleanupEffect::new(
                        awaken_session_contract::SessionCleanupCommand::new(
                            &awaken_session_types::SessionId::from("session"),
                            "session",
                            "cleanup",
                        ),
                        lease.clone(),
                    ),
                    current_lease: lease,
                    checkpoint: None,
                })
            };
            let dispatch = Arc::new(
                awaken_dispatch::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
                    awaken_dispatch::MemoryDispatchStore::new(awaken_time::system_clock()),
                )
                    as Arc<dyn awaken_dispatch::DurableDispatch>),
            );
            let publisher = ClaimFencedArtifactPublisher::new(
                Arc::new(RecordingPublisher::default()),
                dispatch,
                awaken_time::system_clock(),
            );
            let terminal: Arc<
                dyn awaken_session_execution_contract::SessionTerminalPreparationControl,
            > = make_control();
            let checkpoint: Arc<
                dyn awaken_session_execution_contract::SessionCheckpointArtifactControl,
            > = make_control();
            let prior_terminal: Arc<
                dyn awaken_session_execution_contract::SessionTerminalPreparationControl,
            > = make_control();
            let prior_checkpoint: Arc<
                dyn awaken_session_execution_contract::SessionCheckpointArtifactControl,
            > = make_control();
            if terminal_bound {
                assert!(
                    publisher
                        .terminal_preparation_control
                        .set(prior_terminal.clone())
                        .is_ok()
                );
            }
            if checkpoint_bound {
                assert!(
                    publisher
                        .checkpoint_artifact_control
                        .set(prior_checkpoint.clone())
                        .is_ok()
                );
            }
            let result = publisher.install_session_control(terminal.clone(), checkpoint.clone());
            let expected = if terminal_bound {
                Err("Session terminal Artifact authority is already installed")
            } else if checkpoint_bound {
                Err("Session checkpoint Artifact authority is already installed")
            } else {
                Ok(())
            };
            assert_eq!(result, expected);
            let installed_terminal = publisher.terminal_preparation_control.get().unwrap();
            assert!(Arc::ptr_eq(
                installed_terminal,
                if terminal_bound {
                    &prior_terminal
                } else {
                    &terminal
                }
            ));
            match publisher.checkpoint_artifact_control.get() {
                Some(installed) => assert!(Arc::ptr_eq(
                    installed,
                    if checkpoint_bound {
                        &prior_checkpoint
                    } else {
                        &checkpoint
                    }
                )),
                None => assert!(
                    terminal_bound && !checkpoint_bound,
                    "E1 second cell stays empty"
                ),
            }
        }
    }
}
