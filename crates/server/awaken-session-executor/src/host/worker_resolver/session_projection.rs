//! Host-composed tests of the canonical Worker projection synchronizer.
use super::*;
use awaken_session_realization::worker_projection::WorkerProjectionSynchronizer;
mod projection_synchronizer;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::ManagedSessionRootFixture;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, ToggleBindingSink, committed_environment, deferred_environment,
        eager_environment, managed_test_host, test_activation, with_empty_session_resources,
    };
    use awaken_dispatch_contract::DispatchClaims;
    use awaken_dispatch_contract::DispatchEnqueue;
    use std::sync::{Arc, Mutex};

    /// Ordinary cold-worker cause/effect table: C1 a real Queue claim has no
    /// Session Work authority; C2 its activation carries the exact publication;
    /// C3 its immutable Runtime and exact empty Resources carry one Workspace;
    /// C4 selects eager or on-tool-use provisioning. E1 eager resolution creates
    /// the Environment under the Run claim; E2 on-tool-use stays sandbox-free and
    /// retains its deferred executor/tool policy; E3 neither installs a Managed
    /// baseline, Session-dispatch marker, or realization lease.
    ///
    /// | Rule | C1 | C2/C3 | C4 | Effects |
    /// |---|---|---|---|---|
    /// | C1 | ordinary | complete | eager | E1 + E3 |
    /// | C2 | ordinary | complete | on-tool-use | E2 + E3 |
    ///
    /// Malformed projections belong to the shared cross-backend Run Ingress
    /// table. The complete Managed projection helper is deliberately absent:
    /// it changes the authority cause instead of supplying ordinary Run inputs.
    #[tokio::test]
    async fn cold_worker_runtime_projection_decision_table() {
        use awaken_dispatch::WorkerResolver as _;

        let now = awaken_time::system_clock().now().as_epoch_ms();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .expect("dispatch store"),
        );
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_ephemeral_dispatch(store.clone()))
        .build()
        .expect("validated Host dependencies");
        let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
            .expect("Host dispatch resolver");

        let eager_environment = eager_environment();
        let eager_activation = test_activation("cold-legacy", "run-legacy");
        let eager_runtime = awaken_dispatch_contract::SessionRuntimeEnvelope::new(
            awaken_session_configuration_contract::SessionRuntimeProjection {
                environment: eager_environment,
                tools: Some(Default::default()),
                mcp_stages: Some(Vec::new()),
            }
            .encode()
            .expect("encode eager Runtime projection"),
        );
        store
            .enqueue(with_empty_session_resources(
                awaken_dispatch_contract::RunDispatch::new(eager_activation)
                    .with_session_runtime(eager_runtime),
                host.run_assembly.local_workspace(),
            ))
            .await
            .expect("enqueue legacy projection with exact empty Resources");
        clock.set(now);
        let legacy = store
            .claim("worker-a", 1_000, &Default::default())
            .await
            .expect("claim legacy projection")
            .expect("legacy projection available");
        resolver
            .worker_for_claimed(&legacy)
            .await
            .expect("C1 complete eager projection remains eager");
        assert!(
            host.realization_executor
                .environments()
                .session_environment("cold-legacy")
                .await
                .expect("installed Sessions")
                .is_some(),
            "C1"
        );

        let deferred_environment = deferred_environment();
        let deferred_activation = test_activation("cold-deferred", "run-deferred");
        let runtime = awaken_dispatch_contract::SessionRuntimeEnvelope::new(
            awaken_session_configuration_contract::SessionRuntimeProjection {
                environment: deferred_environment,
                tools: Some(Default::default()),
                mcp_stages: Some(Vec::new()),
            }
            .encode()
            .expect("encode runtime projection"),
        );
        store
            .enqueue(with_empty_session_resources(
                awaken_dispatch_contract::RunDispatch::new(deferred_activation)
                    .with_session_runtime(runtime),
                host.run_assembly.local_workspace(),
            ))
            .await
            .expect("enqueue deferred projection");
        clock.set(now);
        let deferred = store
            .claim("worker-a", 1_000, &Default::default())
            .await
            .expect("claim deferred projection")
            .expect("deferred projection available");
        resolver
            .worker_for_claimed(&deferred)
            .await
            .expect("C2 cold Brain resolution stays deferred");
        assert!(
            host.realization_executor
                .environments()
                .session_environment("cold-deferred")
                .await
                .expect("installed Sessions")
                .is_none(),
            "C2"
        );

        assert!(
            host.run_assembly.installed_sessions
                .read("cold-deferred", |slot| slot.run.deferred_executor.is_some()
                    && slot.inputs.tools.as_ref().is_some_and(|tools| {
                        tools == &awaken_session_configuration_contract::SessionToolConfiguration::default()
                    })).expect("installed Sessions")
                .unwrap_or(false),
            "C2"
        );
        for (claimed, thread) in [(&legacy, "cold-legacy"), (&deferred, "cold-deferred")] {
            assert!(
                claimed.request.session_thread_id.is_none()
                    && claimed.session_work_authority.is_none()
                    && claimed.request.session_runtime.is_some()
                    && claimed.request.session_resources.is_some(),
                "C1/C2 ordinary authority with complete carried Runtime/Resources"
            );
            let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
            assert!(
                host.run_assembly
                    .installed_sessions
                    .read(thread, |slot| slot.inputs.baseline.is_none()
                        && !slot.run.session_dispatch
                        && slot.realization.execution().authority.lease().is_none()
                        && slot.run.dispatch_claim.as_ref() == Some(&claim)
                        && slot.inputs.published_snapshot.as_ref()
                            == Some(&claimed.request.activation.snapshot))
                    .expect("installed Sessions")
                    .unwrap_or(false),
                "E3 ordinary Queue/publication authority is retained without Managed facts"
            );
        }
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: Mutex<Vec<&'static str>>,
        required_environment: std::sync::OnceLock<(std::sync::Weak<ExecutorComponents>, String)>,
        stage_entered: Option<Arc<tokio::sync::Notify>>,
        release_stage: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
        ) -> Result<
            awaken_session_configuration_contract::mcp::McpRealizationReceipt,
            awaken_session_types::RunError,
        > {
            let first_stage = {
                let mut calls = self.calls.lock().unwrap();
                calls.push("stage");
                calls.iter().filter(|call| **call == "stage").count() == 1
            };
            if first_stage {
                if let Some(stage_entered) = &self.stage_entered {
                    stage_entered.notify_one();
                }
                if let Some(release_stage) = &self.release_stage {
                    release_stage.notified().await;
                }
            }
            let resident = self
                .required_environment
                .get()
                .is_none_or(|(host, thread)| {
                    host.upgrade().is_some_and(|host| {
                        host.run_assembly
                            .installed_sessions
                            .realizations()
                            .read_realization(thread, |r| r.environment().owner.is_resident())
                            .expect("installed Sessions")
                            .unwrap_or(false)
                    })
                });
            if !resident {
                return Err(awaken_session_types::RunError::classified(
                    "test_session_runtime_missing",
                    "MCP stage ran before the frozen Environment was resident",
                ));
            }
            Ok(
                awaken_session_configuration_contract::mcp::McpRealizationReceipt {
                    receipt_fingerprint: request.fingerprint(),
                    generation: request.generation,
                    realization_id: request.realization_id,
                    selected_plaintext_holder: request.selected_plaintext_holder,
                    actual_realization_kind: None,
                },
            )
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
        ) -> Result<(), awaken_session_types::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
        ) -> Result<(), awaken_session_types::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    pub(super) struct RecoveryControl {
        projection: Arc<Mutex<awaken_session_realization_contract::FrozenSessionProjection>>,
    }

    impl RecoveryControl {
        pub(super) fn fixed(
            projection: awaken_session_realization_contract::FrozenSessionProjection,
        ) -> Self {
            Self {
                projection: Arc::new(Mutex::new(projection)),
            }
        }

        pub(super) fn current_projection(
            &self,
        ) -> awaken_session_realization_contract::FrozenSessionProjection {
            self.projection.lock().unwrap().clone()
        }
    }

    pub(super) struct RecoveryEnvironmentBindingSink {
        projection: Arc<Mutex<awaken_session_realization_contract::FrozenSessionProjection>>,
        calls: std::sync::atomic::AtomicUsize,
        root_authority: Option<(
            Arc<ManagedSessionRootFixture>,
            Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
        )>,
    }

    impl RecoveryEnvironmentBindingSink {
        pub(super) fn new(control: &RecoveryControl) -> Self {
            Self {
                projection: control.projection.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
                root_authority: None,
            }
        }

        /// The same recorder mirrors an actual Root for the cross-Actor
        /// recovery rows. The delegate is its sole authority; rejection never
        /// falls back to the contract-only projection used by other rule tests.
        fn rooted(
            control: &RecoveryControl,
            roots: Arc<ManagedSessionRootFixture>,
            delegate: Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
        ) -> Self {
            let mut recorder = Self::new(control);
            recorder.root_authority = Some((roots, delegate));
            recorder
        }

        pub(super) fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionEnvironmentBindingSink
        for RecoveryEnvironmentBindingSink
    {
        async fn authorize(
            &self,
            intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
        ) -> Result<
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
            awaken_session_types::RunError,
        > {
            if let Some((_, delegate)) = &self.root_authority {
                return delegate.authorize(intent).await;
            }
            // Fixture cause/effect rule: C1 Control owns the current frozen
            // Environment state and C2 Runtime submits one exact intent. E1
            // delegate the decision to the contract state machine; never
            // approximate AlreadyApplied from a cached binding.
            let projection = self.projection.lock().unwrap();
            // Resource-bearing recovery shares the same frozen root. Derive
            // its pending transition from that root, never from the intent
            // being admitted (which would tautologically accept a foreign one).
            let transition = projection.resource_transition(
                awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
            )?;
            let transition_fingerprint = transition.operation_fingerprint();
            projection
                .environment
                .authorize_effect(
                    intent,
                    &projection.baseline.environment.config_fingerprint.0,
                    Some(&transition_fingerprint),
                )
                .map_err(|error| {
                    awaken_session_types::RunError::classified(
                        "test_environment_effect_rejected",
                        error.to_string(),
                    )
                })
        }

        async fn persist(
            &self,
            receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
        ) -> Result<
            awaken_session_state_contract::SessionEnvironmentState,
            awaken_session_types::RunError,
        > {
            // The recovery oracle counts Create/Adopt root receipts. Resource
            // reservations still commit below, but are a distinct effect kind.
            if !matches!(receipt.kind, awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }) {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some((roots, delegate)) = &self.root_authority {
                let thread = receipt.session_id.clone();
                let committed = delegate.persist(receipt).await?;
                let template = self.projection.lock().unwrap().clone();
                let projection = roots.read_projection(thread.as_str(), &template).await?;
                if projection.environment != committed {
                    return Err(awaken_session_types::RunError::unavailable(
                        "Root advanced after its Environment receipt readback",
                    ));
                }
                *self.projection.lock().unwrap() = projection;
                return Ok(committed);
            }
            let committed = committed_environment(receipt);
            let mut projection = self.projection.lock().unwrap();
            if projection.environment != committed {
                projection.revision.0 += 1;
                projection.environment = committed.clone();
            }
            Ok(committed)
        }
    }

    struct RenewalDuringStageControl {
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        stage: awaken_session_configuration_contract::mcp::StageMcpAttachment,
        lease: Mutex<awaken_session_configuration_contract::resource::SessionRealizationLease>,
        renewal_calls: std::sync::atomic::AtomicUsize,
        renewals_on_activate_remaining: std::sync::atomic::AtomicUsize,
        repeat_stage_without_progress: bool,
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionRealizationControl for RenewalDuringStageControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_realization_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn renew_session_realization(
            &self,
            command: awaken_session_realization_contract::RenewSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationLeaseReceipt,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            self.renewal_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut lease = self.lease.lock().unwrap();
            if !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &lease,
                &command.asserted_lease,
            ) || command.requested_expires_at_unix_ms < lease.expires_at_unix_ms
            {
                return Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
                );
            }
            lease.expires_at_unix_ms = command.requested_expires_at_unix_ms;
            Ok(awaken_session_realization::test_support::test_lease_receipt(lease.clone()))
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_realization_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            if self.repeat_stage_without_progress {
                return Ok(
                    awaken_session_realization_contract::SessionRealizationDirective {
                        projection: self.projection.clone(),
                        authority: awaken_session_realization::test_support::test_lease_receipt(
                            self.lease.lock().unwrap().clone(),
                        ),
                        action:
                            awaken_session_realization_contract::SessionRealizationAction::Stage {
                                prepare_session: false,
                                mcp_stages: vec![self.stage.clone()],
                            },
                    },
                );
            }
            if self
                .renewals_on_activate_remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                self.lease.lock().unwrap().expires_at_unix_ms += 1_000;
            }
            let lease = self.lease.lock().unwrap().clone();
            let needs_renewal_stage = command.mcp_receipts.iter().any(|receipt| {
                receipt.generation.lease_expires_at_unix_ms() < lease.expires_at_unix_ms
            });
            let action = if needs_renewal_stage {
                let mut stage = self.stage.clone();
                stage.generation = stage.generation.expiring_at(lease.expires_at_unix_ms);
                stage.stage_idempotency_key = format!("renew:{}", lease.expires_at_unix_ms);
                awaken_session_realization_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                }
            } else {
                awaken_session_realization_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                }
            };
            Ok(
                awaken_session_realization_contract::SessionRealizationDirective {
                    projection: self.projection.clone(),
                    authority: awaken_session_realization::test_support::test_lease_receipt(lease),
                    action,
                },
            )
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_realization_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            let lease = self.lease.lock().unwrap().clone();
            let action =
                if command.published.iter().any(|generation| {
                    generation.lease_expires_at_unix_ms() < lease.expires_at_unix_ms
                }) {
                    let mut stage = self.stage.clone();
                    stage.generation = stage.generation.expiring_at(lease.expires_at_unix_ms);
                    stage.stage_idempotency_key = format!("renew:{}", lease.expires_at_unix_ms);
                    awaken_session_realization_contract::SessionRealizationAction::Stage {
                        prepare_session: false,
                        mcp_stages: vec![stage],
                    }
                } else {
                    awaken_session_realization_contract::SessionRealizationAction::Complete
                };
            Ok(
                awaken_session_realization_contract::SessionRealizationDirective {
                    projection: self.projection.clone(),
                    authority: awaken_session_realization::test_support::test_lease_receipt(lease),
                    action,
                },
            )
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_realization_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure>
        {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::ClaimedSessionRealizationControl
        for RenewalDuringStageControl
    {
        async fn resume_frozen(
            &self,
            _claim: &awaken_dispatch_contract::RunClaim,
            _session_id: &str,
            _session_work_authority: Option<
                &awaken_session_application_contract::SessionWorkAuthorityReceipt,
            >,
            _continuation: Option<
                &awaken_session_configuration_contract::resource::SessionRealizationLease,
            >,
        ) -> Result<
            Option<awaken_session_realization_contract::SessionRealizationDirective>,
            awaken_session_realization_contract::ClaimedSessionControlError,
        > {
            Err(
                awaken_session_realization_contract::ClaimedSessionControlError::new(
                    "not used by the renewal concurrency fixture",
                ),
            )
        }
    }

    fn renewal_stage_fixture(
        thread: &str,
        expires_at_unix_ms: u64,
    ) -> (
        awaken_session_realization_contract::FrozenSessionProjection,
        awaken_session_configuration_contract::resource::SessionRealizationLease,
        awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) {
        let mut projection = frozen_projection();
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms,
        };
        let stage = awaken_session_configuration_contract::mcp::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: crate::test_mcp::mcp_fence(
                thread,
                "browser",
                1,
                lease.runtime_incarnation.clone(),
                lease.epoch,
                expires_at_unix_ms,
            ),
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "stage-browser-1".into(),
            name: "browser".into(),
            target: awaken_agent_contract::McpTarget::parse_http(
                "https://browser.example.test/mcp",
            )
            .expect("HTTP MCP target"),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };
        projection.mcp.push(
            awaken_session_configuration_contract::mcp::SessionMcpAttachment {
                attachment_id: stage.generation.attachment_id().clone(),
                name: stage.name.clone(),
                generation: stage.generation.generation().clone(),
                target: stage.target.clone(),
                prompts_as_skills: stage.prompts_as_skills,
                origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Agent,
                credential: stage.credential.clone(),
                selected_plaintext_holder: stage.selected_plaintext_holder.clone(),
                state: awaken_session_configuration_contract::mcp::McpAttachmentState::Realizing,
                publication_acknowledged: false,
                realization: Some(
                    awaken_session_configuration_contract::mcp::McpRealizationClaim {
                        realization_id: stage.realization_id.clone(),
                        runtime_incarnation: stage.generation.runtime_incarnation().to_owned(),
                        lease_epoch: stage.generation.lease_epoch().get(),
                        lease_expires_at_unix_ms: stage.generation.lease_expires_at_unix_ms(),
                        stage_idempotency_key: stage.stage_idempotency_key.clone(),
                    },
                ),
                attempts: 1,
                last_error: None,
            },
        );
        (projection, lease, stage)
    }

    /// Test-only typed-outcome seam. Production callers enter through
    /// `drive_session_realization`; these progress tests construct the same
    /// synchronizer and call the sole driver directly so they can assert its
    /// un-erased `DidNotConverge` result.
    async fn drive_test_session_realization(
        host: &ExecutorComponents,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        session_id: &awaken_session_types::SessionId,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationDriveError> {
        let realization = host
            .run_assembly
            .installed_sessions
            .acquire_realization_with(session_id.as_str(), |_| ())
            .await
            .map_err(|error| {
                awaken_session_types::RunError::from(
                    crate::realization_error::RealizationError::from(error),
                )
            })?
            .0;
        let synchronizer = WorkerProjectionSynchronizer::for_directive(
            host.run_assembly.as_ref(),
            &directive,
            None,
            None,
            false,
            None,
        );
        crate::application::worker_projection::drive_session_realization_with_synchronizer(
            control,
            directive,
            synchronizer,
            awaken_authority_clock::authority_proof_started().unwrap(),
            &realization,
        )
        .await
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionRealizationControl for RecoveryControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_realization_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_realization_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            Ok(
                awaken_session_realization_contract::SessionRealizationDirective {
                    projection: self.current_projection(),
                    authority: awaken_session_realization::test_support::test_lease_receipt(
                        command.lease,
                    ),
                    action:
                        awaken_session_realization_contract::SessionRealizationAction::Publish {
                            publish: command
                                .mcp_receipts
                                .into_iter()
                                .map(|receipt| receipt.generation)
                                .collect(),
                            drain: Vec::new(),
                        },
                },
            )
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_realization_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationDirective,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            Ok(
                awaken_session_realization_contract::SessionRealizationDirective {
                    projection: self.current_projection(),
                    authority: awaken_session_realization::test_support::test_lease_receipt(
                        command.lease,
                    ),
                    action: awaken_session_realization_contract::SessionRealizationAction::Complete,
                },
            )
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_realization_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure>
        {
            Ok(())
        }

        async fn renew_session_realization(
            &self,
            _command: awaken_session_realization_contract::RenewSessionRealization,
        ) -> Result<
            awaken_session_realization_contract::SessionRealizationLeaseReceipt,
            awaken_session_realization_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
        }
    }

    pub(super) fn frozen_projection() -> awaken_session_realization_contract::FrozenSessionProjection
    {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_environment_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint:
                        awaken_session_configuration_contract::EnvironmentFingerprint(
                            "env-fingerprint".into(),
                        ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network:
                        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization:
                        awaken_credential_contract::CredentialRealizationProfile {
                            inference_holder: holder.clone(),
                            mcp_holder: holder.clone(),
                            resource_holder: holder,
                        },
                },
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "agent-a".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        );
        awaken_session_realization_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_types::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment_materialization: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            previous_resource_manifest: Some(
                awaken_session_configuration_contract::resource::SessionResourceManifest::new(
                    "workspace",
                    awaken_session_configuration_contract::resource::ResolvedSessionResources::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    /// Lease-only Resource synchronization cause/effect graph: C1 the frozen
    /// baseline is resident; C2 a dispatch claim authorizes remote reads; C3
    /// this Stage actually prepares the Session; C4 the projection pins a remote
    /// custom Skill. E1 lease-only renewal reuses resident Resource/Skill bytes;
    /// E2 cold or preparing paths fail closed without material authority; E3 a
    /// claim keeps using the canonical revalidation path (covered by repository
    /// claim C1/C2 in host tests).
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|---|
    /// | M1 | yes | no | no | yes | E1; no remote read |
    /// | M2 | no | no | no | yes | E2; no partial cold projection |
    /// | M3 | yes | no | yes | yes | E2; material source required |
    /// | M4 | any | yes | any | yes | E3 |
    #[tokio::test]
    async fn lease_only_renewal_never_cold_materializes_remote_skills() {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "resident-renewal-resources";
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await)
            .build()
            .expect("validated Host dependencies");
        let _managed = host.clone();
        let resident = frozen_projection();
        host.run_assembly
            .install_frozen_session_projection(thread, resident.clone(), None, true, None)
            .await
            .expect("M1 establish resident baseline");
        let mut remote = resident;
        remote.revision.0 += 1;
        remote.resource_revision = 1;
        remote.resources = remote
            .resources
            .with_skills(vec![
                awaken_session_configuration_contract::resource::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "design".into(),
                    version: 36,
                    bundle_sha256: "sha256-design-v36".into(),
                },
            ])
            .unwrap();
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let synchronizer = WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        };
        synchronizer
            .synchronize_session_projection(
                &awaken_session_types::SessionId::from(thread),
                &remote,
                &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                false,
            )
            .await
            .expect("M1 lease-only renewal reuses resident projection");
        assert!(
            host.realization_executor
                .staging()
                .thread_resource_manifest(thread)
                .expect("installed Sessions")
                .is_none(),
            "M1"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r.execution().authority.lease().cloned())
                .expect("installed Sessions")
                .flatten(),
            Some(lease.clone()),
            "M1 lease still advances"
        );

        let cold = WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from("cold-renewal-resources"),
            &remote,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            false,
        )
        .await
        .expect_err("M2 cold renewal must fail closed");
        assert!(cold.to_string().contains("cannot cold-materialize"), "M2");

        let preparing = synchronizer
            .synchronize_session_projection(
                &awaken_session_types::SessionId::from(thread),
                &remote,
                &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                true,
            )
            .await
            .expect_err("M3 preparation needs a material source");
        assert!(preparing.to_string().contains("Skill"), "M3: {preparing}");
    }

    /// Dynamic-publication cause/effect graph: C0 a test fixture changes the
    /// model binding and recomputes its canonical fingerprint; C1 the Coordinator
    /// projects the exact publication before any Run can be claimed; C2 a
    /// compatibility renewal omits it; C3 a claimed Run carries a Session-effective overlay
    /// with the same frozen coordinates; C4 claimed coordinates differ. E1
    /// retain the immutable publication; E2 reuse it; E3 accept the overlay
    /// without replacing E1; E4 reject the foreign Run and preserve E1.
    /// Decision rules: P1 C1=>E1, P2 E1+C2=>E2, P3 E1+C3=>E3, P4 C4=>E4.
    #[tokio::test]
    async fn session_projection_delivers_publication_before_first_run_claim() {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "dynamic-publication-renewal";
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await)
            .build()
            .expect("validated Host dependencies");
        let _managed = host.clone();
        let mut projection = frozen_projection();
        projection.baseline.agent_revision = Some(7);
        projection.baseline.runtime = Some("acp:claude".into());
        let mut snapshot = test_activation(thread, "dynamic-publication").snapshot;
        snapshot.metadata.source.revision = 7;
        let (provider, model, _) = snapshot.resolved_spec.model_binding.identity();
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::parse(provider, model, "acp:claude")
                .unwrap();
        snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
        snapshot
            .recompute_fingerprint()
            .expect("P1 frozen publication has a canonical fingerprint");
        projection.agent_publication = Some(snapshot.clone());
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &projection,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            true,
        )
        .await
        .expect("projection publication realizes the Session before a Run claim");

        let mut compatibility_renewal = projection.clone();
        compatibility_renewal.agent_publication = None;
        WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &compatibility_renewal,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            false,
        )
        .await
        .expect("lease renewal reuses the retained immutable publication");
        let retained = host
            .run_assembly
            .installed_sessions
            .realizations()
            .published_snapshot(thread)
            .expect("installed Sessions");
        assert_eq!(retained.as_ref(), Some(&snapshot), "P1/P2");

        let mut effective = snapshot.clone();
        effective.resolved_spec.tool_descriptors.push(
            crate::config::session_client_tool_descriptor(
                &awaken_agent_contract::ClientToolDescriptor {
                    name: "resource_request".into(),
                    description: "Request a frozen WorkUnit resource".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ),
        );
        effective
            .recompute_fingerprint()
            .expect("Session overlay remains a valid snapshot");
        WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: Some(&effective),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &projection,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            false,
        )
        .await
        .expect("P3 Session-effective Run snapshot shares the frozen coordinates");
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .published_snapshot(thread)
                .expect("installed Sessions")
                .as_ref(),
            Some(&snapshot),
            "P3"
        );

        let mut foreign = effective;
        foreign.metadata.source.revision += 1;
        let error = WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: Some(&foreign),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &projection,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            false,
        )
        .await
        .expect_err("P4 foreign claimed coordinates are rejected");
        assert_eq!(error.code, "session_runtime_publication_conflict", "P4");

        let mut missing = projection.clone();
        missing.agent_publication = None;
        let error = WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from("publication-missing"),
            &missing,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            true,
        )
        .await
        .expect_err("a cold pinned Worker projection must carry its publication");
        assert_eq!(error.code, "session_runtime_publication_missing");

        for (session_id, mutate) in [
            ("publication-agent-mismatch", 0_u8),
            ("publication-revision-mismatch", 1),
            ("publication-runtime-mismatch", 2),
        ] {
            let mut conflicting = projection.clone();
            let delivered = conflicting
                .agent_publication
                .as_mut()
                .expect("fixture publication");
            match mutate {
                0 => delivered.root_agent_id.0 = "another-agent".into(),
                1 => delivered.metadata.source.revision = 8,
                _ => {
                    let (provider, model, _) = delivered.resolved_spec.model_binding.identity();
                    let binding = awaken_runtime_contract::resolved::ModelBinding::parse(
                        provider,
                        model,
                        "native:other",
                    )
                    .unwrap();
                    delivered.resolved_spec.model_binding =
                        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
                }
            }
            let error = WorkerProjectionSynchronizer {
                execution: host.run_assembly.as_ref(),
                claim: None,
                published_snapshot: None,
                rebuild_unavailable_environment: false,
                requires_runtime_before_effects: false,
                claimed_environment_binding: None,
            }
            .synchronize_session_projection(
                &awaken_session_types::SessionId::from(session_id),
                &conflicting,
                &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                true,
            )
            .await
            .expect_err("mismatched Worker publication must fail closed");
            assert_eq!(error.code, "session_runtime_publication_conflict");
        }
    }

    /// Concurrent-renewal cause/effect graph: C1 an initial phase driver owns
    /// the Session realization lock; C2 its MCP Stage is still pending; C3 a
    /// heartbeat requests the same owner/incarnation/epoch lease extension.
    /// Effects: E1 extend durable and local authority without blocking; E2 never
    /// start a second Stage/Publish driver; E3 the current driver catches its
    /// exact generation up before completion. Replacement fencing is owned by
    /// the contract authorization table; ordinary idle renewal is W6 in the
    /// parent resolver tests.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | R1 | yes | yes | yes | E1 + E2 + E3 |
    /// | R2 | no | no | yes | canonical renewal driver (W6) |
    /// | R3 | any | any | replacement | fence/revoke (authorization A2/W7) |
    #[tokio::test]
    async fn heartbeat_renewal_does_not_duplicate_an_in_flight_realization_driver() {
        let thread = "renew-during-stage";
        let initial_expiry = awaken_session_realization::test_support::wall_clock_now_ms() + 5_000;
        let renewed_expiry = initial_expiry + 10_000;
        let (projection, lease, stage) = renewal_stage_fixture(thread, initial_expiry);
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            renewal_calls: std::sync::atomic::AtomicUsize::new(0),
            renewals_on_activate_remaining: std::sync::atomic::AtomicUsize::new(0),
            repeat_stage_without_progress: false,
        });
        let stage_entered = Arc::new(tokio::sync::Notify::new());
        let release_stage = Arc::new(tokio::sync::Notify::new());
        let realizer = Arc::new(RecordingMcpRealizer {
            stage_entered: Some(stage_entered.clone()),
            release_stage: Some(release_stage.clone()),
            ..Default::default()
        });
        let host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_session_realization_control(control.clone()))
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_mcp_attachment_realizer(realizer.clone()),
        ))
        .build()
        .expect("validated Host dependencies");
        let _managed = host.clone();
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection,
            authority: awaken_session_realization::test_support::test_lease_receipt(lease),
            action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![stage],
            },
        };

        let initial_host = host.clone();
        let initial_control = control.clone();
        let initial = tokio::spawn(async move {
            crate::application::worker_projection::realize_session(
                &initial_host.run_assembly,
                initial_control.as_ref(),
                &awaken_session_types::SessionId::from(thread),
                directive,
                None,
                None,
                false,
            )
            .await.map_err(awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error)
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), stage_entered.notified())
            .await
            .expect("R1 initial Stage entered");
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                host.run_assembly.renew_due_session_realizations(
                    awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000,),
                ),
            )
            .await
            .expect("R1/E1 renewal does not wait for Stage")
            .expect("R1/E1 renewal succeeds"),
            1,
            "R1/E1"
        );
        assert_eq!(
            control
                .renewal_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "R1/E1 durable renewal advances once"
        );
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage"],
            "R1/E2"
        );

        release_stage.notify_one();
        initial
            .await
            .expect("R1 initial task")
            .expect("R1/E3 initial realization completes");
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "stage", "publish"],
            "R1/E2-E3 catches up before publishing only the current fence"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r
                    .execution()
                    .authority
                    .lease()
                    .map(|lease| lease.expires_at_unix_ms))
                .expect("installed Sessions")
                .flatten(),
            Some(renewed_expiry),
            "R1/E3"
        );
    }

    /// Driver-progress cause/effect graph: C1 Control returns five monotonic
    /// same-owner/same-epoch lease extensions; C2 each extension changes the
    /// exact Stage generation; C3 Control instead repeats an identical
    /// directive. Effects: E1 all six required Stages execute and only the
    /// newest generation publishes; E2 no arbitrary round cap rejects healthy
    /// progress; E3 an exact no-progress transition fails immediately without
    /// repeating an external effect. Rules D1=C1+C2=>E1+E2 and
    /// D2=C3=>E3. Five renewals intentionally exceed the removed four-round
    /// limit that failed the real Classroom workload.
    #[tokio::test]
    async fn sustained_renewal_progress_converges_and_exact_repetition_fails_closed() {
        let thread = "sustained-renewal-progress";
        let (projection, lease, stage) = renewal_stage_fixture(
            thread,
            awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
        );
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            renewal_calls: std::sync::atomic::AtomicUsize::new(0),
            renewals_on_activate_remaining: std::sync::atomic::AtomicUsize::new(5),
            repeat_stage_without_progress: false,
        });
        let realizer = Arc::new(RecordingMcpRealizer::default());
        let host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_session_realization_control(control.clone()))
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_mcp_attachment_realizer(realizer.clone()),
        ))
        .build()
        .expect("validated Host dependencies");
        let _managed = host.clone();
        drive_test_session_realization(
            &host,
            control.as_ref(),
            &awaken_session_types::SessionId::from(thread),
            awaken_session_realization_contract::SessionRealizationDirective {
                projection,
                authority: awaken_session_realization::test_support::test_lease_receipt(lease),
                action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            },
        )
        .await
        .expect("D1/E1-E2 monotonic renewal progress converges");
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            [
                "stage", "stage", "stage", "stage", "stage", "stage", "publish"
            ],
            "D1/E1 publishes only after all renewal catch-up rounds"
        );

        let thread = "repeated-renewal-directive";
        let (projection, lease, stage) = renewal_stage_fixture(
            thread,
            awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
        );
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            renewal_calls: std::sync::atomic::AtomicUsize::new(0),
            renewals_on_activate_remaining: std::sync::atomic::AtomicUsize::new(0),
            repeat_stage_without_progress: true,
        });
        let realizer = Arc::new(RecordingMcpRealizer::default());
        let host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_session_realization_control(control.clone()))
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_mcp_attachment_realizer(realizer.clone()),
        ))
        .build()
        .expect("validated Host dependencies");
        let _managed = host.clone();
        let error = drive_test_session_realization(
            &host,
            control.as_ref(),
            &awaken_session_types::SessionId::from(thread),
            awaken_session_realization_contract::SessionRealizationDirective {
                projection,
                authority: awaken_session_realization::test_support::test_lease_receipt(lease),
                action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            },
        )
        .await
        .expect_err("D2/E3 exact repetition fails closed");
        assert!(matches!(
            error,
            awaken_session_realization_contract::SessionRealizationDriveError::DidNotConverge
        ));
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage"],
            "D2/E3 detects no progress before duplicating the effect"
        );
    }

    /// R5/R6 share C1 one actual original Root/materialization, C2 successful
    /// Resource Stage/activation, C3 confirmed old execution stop plus exact
    /// inode loss, and C4 a separately committed consumer assignment. E1 the
    /// immutable original materialization survives C4; E2 the same Control
    /// recorder delegates all admission/receipt writes to that one real Root.
    /// The callers vary only rebuild-vs-continuity and retain their original
    /// physical-result/no-replacement assertions. No read increments an epoch
    /// or revision, and this test assignment is not production takeover proof.
    async fn missing_environment_recovery_fixture(
        storage: &std::path::Path,
        thread: &str,
        publication: &awaken_runtime_contract::ExecutableAgentSnapshot,
    ) -> (
        Arc<ExecutorComponents>,
        RecoveryControl,
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_provisioning_contract::SandboxHandle,
        Arc<ManagedSessionRootFixture>,
    ) {
        let roots = Arc::new(ManagedSessionRootFixture::new().await);
        let source_host = roots
            .configure(
                crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
                    .await
                    .with_store_dir(storage)
                    .await,
            )
            .build()
            .expect("C1 Root-backed original producer");
        let source = managed_test_host(source_host.clone());
        let mut initial = frozen_projection();
        initial.agent_publication = Some(publication.clone());
        let installed = roots
            .install_projection(&source, thread, initial.clone())
            .await
            .expect("C1 original Root before physical Create");
        let binding = super::test_support::available_local_environment_binding(
            &source_host,
            thread,
            &installed,
        )
        .await;
        let original_handle = serde_json::from_str(&binding).expect("C1 original typed handle");
        let resident = roots
            .read_projection(thread, &initial)
            .await
            .expect("C1 original materialization readback");
        let old_lease = source_host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions")
            .flatten()
            .expect("C1 actual original assignment");
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*source.run_assembly,
            thread,
            resident.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: awaken_session_realization::test_support::test_observed_lease(old_lease),
                prepare_session: true,
            },
        )
        .await
        .expect("C2 actual Resource Stage");
        let settled = roots
            .settle_resource_stage(&source, thread, resident)
            .await
            .expect("C2 activate only the actual prepared Resource generation");
        let original_materialization = settled.environment_materialization.clone();
        assert!(
            source_host
                .realization_executor
                .environments()
                .retire_session_environment_for_revocation(
                    thread,
                    (
                        &source_host.run_assembly.mcp,
                        source_host.run_assembly.installed_sessions.as_ref()
                    ),
                    None,
                )
                .await
                .expect("C3 revoke/stop exact old execution"),
            "C3 old execution confirmed closed"
        );
        super::test_support::remove_local_environment_root(&source_host, thread);
        let host = roots
            .configure(
                crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
                    .await
                    .with_store_dir(storage)
                    .await,
            )
            .build()
            .expect("C4 Root-backed cold consumer");
        let managed = managed_test_host(host.clone());
        let frozen = roots
            .reassign_closed(&source, &managed, thread, settled, ("worker-a", "worker-a"))
            .await
            .expect("C4 explicit current consumer through original assignment CAS");
        assert_eq!(
            frozen.environment_materialization, original_materialization,
            "E1 current assignment preserves immutable original materialization"
        );
        let lease = host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions")
            .flatten()
            .expect("C4 Store-installed consumer lease");
        assert_eq!(
            (lease.owner.as_str(), lease.runtime_incarnation.as_str()),
            ("worker-a", "worker-a"),
            "C4 consumer remains a separate Actor"
        );
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: frozen.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease),
            action: awaken_session_realization_contract::SessionRealizationAction::Complete,
        };
        let control = RecoveryControl::fixed(frozen);
        let delegate = host
            .realization_executor
            .environments()
            .environment_binding_sink()
            .expect("C4 already-configured actual Root authority");
        managed
            .run_assembly
            .install_test_environment_binding_sink(Arc::new(
                RecoveryEnvironmentBindingSink::rooted(&control, roots.clone(), delegate),
            ));
        (host, control, directive, original_handle, roots)
    }

    #[tokio::test]
    async fn cold_worker_adopts_the_frozen_environment_before_recovering_stdio_mcp() {
        /* Cold remote-Session recovery cause/effect graph: C1 Control returns a
         * frozen Session with an opaque resident-Environment binding; C2 the new
         * Worker process has no resident Runtime/Environment; C3 the claimed Run
         * carries the exact immutable Agent snapshot; C4 Control requires the
         * active sandbox-stdio MCP generation to be restaged; C5 a rebuild-mode
         * Run names a provider-created typed Environment handle whose exact
         * physical root was lost and is definitively unavailable; C6 the fixture
         * producer installs the same complete frozen projection used by cold
         * adoption before creating that handle; C7 a fresh consumer Host has no
         * resident slot or Runtime; C8 the first-use Managed binding sink commits
         * and reads back Resident; C9 cold replay carries that exact Store-read
         * effect id and generation, never a legacy binding-only repair. Effects:
         * E1 adopt the exact bound Environment
         * before MCP stage; E2 never consult current
         * Agent publication; E3 preserve the Sandbox handle and generation; E4 a
         * cold lease-only replay without the exact snapshot fails closed; E5
         * Managed rebuild stays pending until accepted Source closure/access
         * accompanies the claimed Run's explicit recovery policy;
         * E6 no MCP effect runs when a first-use stdio stage lacks that snapshot;
         * E7 C6 persists exactly once and MCP stage observes the Store-read Resident
         * owner rather than the earlier Unmaterialized projection.
         *
         * | Rule | binding | resident | snapshot | stdio | recovery | Effect |
         * |---|---|---|---|---|---|---|
         * | R1 | ready | no | yes | yes | either | E1 + E2 + E3 |
         * | R2 | ready | yes | no | yes | either | reuse resident Environment |
         * | R3 | ready | no | no | yes | either | E4 |
         * | R4 | no | no | yes | no | either | ordinary resume (covered by O1) |
         * R5/R6 run as separate cold_rebuild/cold_continuity tests below.
         * | R5 | missing | no | yes | no | rebuild | E5: pending, no accepted Source closure/access |
         * | R6 | missing | no | yes | no | continuity | fail closed |
         * | R7 | none yet | no | yes | yes | either | E7; install publication, then stage |
         * | R8 | none yet | no | no | yes | either | E4 + E6 |
         */
        let storage = tempfile::tempdir().expect("storage");
        let thread = "cold-frozen-environment";
        let activation = test_activation(thread, "run-cold-frozen-environment");
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let original = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies");
        let original_managed = managed_test_host(original.clone());
        let mut frozen = frozen_projection();
        let mut original_projection = frozen.clone();
        original_projection.agent_publication = Some(activation.snapshot.clone());
        let original_control = RecoveryControl::fixed(original_projection.clone());
        let original_binding_sink =
            Arc::new(RecoveryEnvironmentBindingSink::new(&original_control));
        original_managed
            .run_assembly
            .install_test_environment_binding_sink(original_binding_sink.clone());
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*original_managed.run_assembly,
            thread,
            original_projection,
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                prepare_session: true,
            },
        )
        .await
        .expect("R1 install exact original frozen projection");
        let original_ctx = original
            .run_assembly
            .ctx_for_snapshot(thread, Some("agent-a"), Some(activation.snapshot.clone()))
            .await
            .expect("R1 original Environment");
        let handle = original_ctx
            .env
            .as_ref()
            .expect("R1 eager Environment")
            .lifecycle()
            .handle();
        let binding = serde_json::to_string(&handle).expect("R1 durable binding");
        let committed_environment = original_control.current_projection().environment;
        assert_eq!(
            committed_environment.binding(),
            Some(binding.as_str()),
            "R1/C9 Store-read binding remains exact"
        );
        frozen.revision = original_control.current_projection().revision;
        frozen.environment = committed_environment;
        assert_eq!(
            original_binding_sink.calls(),
            1,
            "R1/C9 producer commits and reads one exact durable identity"
        );
        drop(original_ctx);
        drop(original_managed);
        drop(original);

        let stage = awaken_session_configuration_contract::mcp::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: crate::test_mcp::mcp_fence(thread, "browser", 1, "worker-a", 1, u64::MAX),
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "recover-browser-1".into(),
            name: "browser".into(),
            target: awaken_agent_contract::McpTarget::sandbox_stdio(
                "playwright-mcp",
                vec!["--headless".into()],
            )
            .expect("sandbox stdio target"),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: frozen.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease),
            action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![stage.clone()],
            },
        };
        let control = RecoveryControl::fixed(frozen.clone());
        let realizer = Arc::new(RecordingMcpRealizer::default());
        let host = ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .with_session_execution(
                crate::SessionExecutionDependencies::default()
                    .with_mcp_attachment_realizer(realizer.clone()),
            ))
        .build()
        .expect("validated Host dependencies");
        realizer
            .required_environment
            .set((Arc::downgrade(&host), thread.into()))
            .unwrap();
        let managed = super::test_support::complete_managed_test_host(host.clone());
        let binding_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        managed
            .run_assembly
            .install_test_environment_binding_sink(binding_sink);
        crate::application::worker_projection::realize_session(
            &host.run_assembly,
            &control,
            &awaken_session_types::SessionId::from(thread),
            directive.clone(),
            None,
            Some(&activation.snapshot),
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect("R1 frozen Environment recovery");
        assert_eq!(
            host.run_assembly.session_environment_handle(thread).await,
            Some(handle.clone()),
            "R1/E1-E3"
        );
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R1/E1"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .read_run(thread, |run| run.session_dispatch)
                .expect("installed Sessions")
                .unwrap_or(false),
            "R1/E1 Control synchronization publishes the existing Managed Session marker so a later empty compatibility envelope cannot drain this MCP generation"
        );

        // Publication deliberately evicts the rebuildable SessionCtx before the
        // final claimed resolve. A heartbeat may enter this exact gap; the
        // retained Environment remains the publication-authorized effect owner.
        host.realization_executor
            .environments()
            .evict_session_for_rebuild(thread)
            .await
            .expect("installed Sessions");
        assert!(
            host.run_assembly
                .installed_sessions
                .read(thread, |slot| slot.run.context.is_none()
                    && slot.realization.environment().owner.is_resident())
                .expect("installed Sessions")
                .unwrap_or(false),
            "R2 fixture is the publish-to-final-resolve gap"
        );
        let mut resident_directive = directive.clone();
        resident_directive.projection = control.current_projection();
        crate::application::worker_projection::realize_session(
            &host.run_assembly,
            &control,
            &awaken_session_types::SessionId::from(thread),
            resident_directive,
            None,
            None,
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect("R2 resident Environment is the already-installed publication authority");
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish", "stage", "publish"],
            "R2 reuses the resident Environment without another claimed snapshot"
        );

        // R9/R10 expand the graph with independent Worker/Sandbox failures:
        // C10 the Worker and exact Environment Arc survive, but the physical
        // root disappears; C11 Runtime is cached or evicted; C12 the directive
        // is Complete (no MCP stage or tool can force a hidden materialization).
        // R9=C10+C11+C12+continuity => error, retain exact Arc/binding, zero
        // replacement. R10=same cut+rebuild => observe, retire exact A, create B,
        // and publish its new handle. Cross both Runtime states with both
        // policies and Ready replay through the real driver, requiring the same B Arc.
        // C13 a failed synchronization closes that Session generation. R9a a
        // rebuild-policy change cannot revive it; R10 requires a separately
        // supplied fresh Control generation. The fixture supplies that receipt
        // explicitly, never edits Host authority to bypass closed-state admission.
        let mut recovery_authority = directive.authority.clone();
        for runtime_cached in [false, true] {
            if runtime_cached {
                host.run_assembly
                    .ctx_for_snapshot(thread, Some("agent-a"), Some(activation.snapshot.clone()))
                    .await
                    .expect("R9 warm Runtime");
            } else {
                host.realization_executor
                    .environments()
                    .evict_session_for_rebuild(thread)
                    .await
                    .expect("installed Sessions");
            }
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read_run(thread, |run| run.context.is_some())
                    .expect("installed Sessions")
                    .unwrap_or(false),
                runtime_cached,
                "R9 fixture independently establishes each Runtime state"
            );
            let retained = host
                .realization_executor
                .environments()
                .session_environment(thread)
                .await
                .expect("installed Sessions")
                .expect("R9 resident Arc");
            let old_handle = retained.lifecycle().handle();
            crate::host::worker_resolver::test_support::remove_local_environment_root(
                &host, thread,
            );
            let missing_directive =
                awaken_session_realization_contract::SessionRealizationDirective {
                    projection: control.current_projection(),
                    // R9 preserves the existing response-relative receipt, not a
                    // second Worker-local deadline synthesized from Control UTC.
                    authority: recovery_authority.clone(),
                    action: awaken_session_realization_contract::SessionRealizationAction::Complete,
                };
            let error = crate::application::worker_projection::realize_session(
                &host.run_assembly,
                &control,
                &awaken_session_types::SessionId::from(thread),
                missing_directive.clone(),
                None,
                Some(&activation.snapshot),
                false,
            )
            .await.map_err(awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error)
            .expect_err("R9 resident Arc cannot hide physical unavailability");
            assert!(
                error.to_string().contains("unavailable or terminal"),
                "R9: {error}"
            );
            assert!(
                Arc::ptr_eq(
                    &retained,
                    &host
                        .realization_executor
                        .environments()
                        .session_environment(thread)
                        .await
                        .expect("installed Sessions")
                        .unwrap()
                ),
                "R9 retains exact owner"
            );
            assert_eq!(
                control.current_projection().environment.binding(),
                Some(serde_json::to_string(&old_handle).unwrap().as_str()),
                "R9 zero replacement binding"
            );
            assert!(
                matches!(
                    host.run_assembly.installed_sessions.realizations().read_realization(thread, |r| r.execution().authority.clone()).expect("installed Sessions"),
                    Some(crate::realization_state::SessionRealizationAuthority::Closed(Some(ref closed)))
                        if closed == &recovery_authority.lease
                ),
                "R9 closes the failed generation"
            );
            let rejected_replay = crate::application::worker_projection::realize_session(
                &host.run_assembly,
                &control,
                &awaken_session_types::SessionId::from(thread),
                missing_directive.clone(),
                None,
                Some(&activation.snapshot),
                true,
            )
            .await.map_err(awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error)
            .expect_err("R9a policy alone cannot revive a closed grant");
            assert!(
                rejected_replay
                    .to_string()
                    .contains("time proof is not current")
            );
            assert!(Arc::ptr_eq(
                &retained,
                &host
                    .realization_executor
                    .environments()
                    .session_environment(thread)
                    .await
                    .expect("installed Sessions")
                    .unwrap()
            ));
            assert_eq!(
                control.current_projection().environment.binding(),
                Some(serde_json::to_string(&old_handle).unwrap().as_str())
            );
            let mut next_lease = recovery_authority.lease.clone();
            next_lease.epoch += 1;
            recovery_authority =
                awaken_session_realization::test_support::test_lease_receipt(next_lease);
            let missing_directive =
                awaken_session_realization_contract::SessionRealizationDirective {
                    authority: recovery_authority.clone(),
                    ..missing_directive
                };
            crate::application::worker_projection::realize_session(
                &host.run_assembly,
                &control,
                &awaken_session_types::SessionId::from(thread),
                missing_directive,
                None,
                Some(&activation.snapshot),
                true,
            )
            .await.map_err(awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error)
            .expect("R10 rebuild through the production synchronizer");
            let replacement = host
                .realization_executor
                .environments()
                .session_environment(thread)
                .await
                .expect("installed Sessions")
                .expect("R10 replacement");
            assert_ne!(
                replacement.lifecycle().handle(),
                old_handle,
                "R10 new physical incarnation"
            );
            let replay = awaken_session_realization_contract::SessionRealizationDirective {
                projection: control.current_projection(),
                authority: recovery_authority.clone(),
                action: awaken_session_realization_contract::SessionRealizationAction::Complete,
            };
            crate::application::worker_projection::realize_session(
                &host.run_assembly,
                &control,
                &awaken_session_types::SessionId::from(thread),
                replay,
                None,
                Some(&activation.snapshot),
                true,
            )
            .await.map_err(awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error)
            .expect("R10 Ready replay");
            assert!(
                Arc::ptr_eq(
                    &replacement,
                    &host
                        .realization_executor
                        .environments()
                        .session_environment(thread)
                        .await
                        .expect("installed Sessions")
                        .unwrap()
                ),
                "R10 no duplicate ready owner"
            );
        }

        let first_use_thread = "cold-first-use-environment";
        let first_use_activation = test_activation(first_use_thread, "run-cold-first-use");
        let first_use_projection = frozen_projection();
        let first_use_stage = awaken_session_configuration_contract::mcp::StageMcpAttachment {
            generation: stage
                .generation
                .with_persisted(|w| w.session_id = first_use_thread.into())
                .unwrap(),
            realization_id: "realize-browser-first-use".into(),
            stage_idempotency_key: "stage-browser-first-use".into(),
            ..stage.clone()
        };
        let first_use_directive =
            awaken_session_realization_contract::SessionRealizationDirective {
                projection: first_use_projection.clone(),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    awaken_session_configuration_contract::resource::SessionRealizationLease {
                        owner: "worker-a".into(),
                        runtime_incarnation: "worker-a".into(),
                        epoch: 1,
                        expires_at_unix_ms: u64::MAX,
                    },
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                    prepare_session: true,
                    mcp_stages: vec![first_use_stage],
                },
            };
        let first_use_realizer = Arc::new(RecordingMcpRealizer::default());
        let first_use_host =
            ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
                .await
                .with_store_dir(storage.path())
                .await)
                .with_session_execution(
                    crate::SessionExecutionDependencies::default()
                        .with_mcp_attachment_realizer(first_use_realizer.clone()),
                ))
            .build()
            .expect("validated Host dependencies");
        first_use_realizer
            .required_environment
            .set((Arc::downgrade(&first_use_host), first_use_thread.into()))
            .unwrap();
        let first_use_control = RecoveryControl::fixed(first_use_projection);
        let first_use_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&first_use_control));
        let first_use_managed = first_use_host.clone();
        first_use_managed
            .run_assembly
            .install_test_environment_binding_sink(first_use_sink.clone());
        let _managed = first_use_managed;
        crate::application::worker_projection::realize_session(
            &first_use_host.run_assembly,
            &first_use_control,
            &awaken_session_types::SessionId::from(first_use_thread),
            first_use_directive.clone(),
            None,
            Some(&first_use_activation.snapshot),
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect("R7 first-use Environment installs publication before MCP staging");
        assert_eq!(first_use_sink.calls(), 1, "R7 one aggregate binding CAS");
        assert_eq!(
            first_use_realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R7 MCP stage observed the Store-read Resident owner"
        );
        assert!(
            first_use_host
                .run_assembly
                .installed_sessions
                .realizations()
                .read_realization(first_use_thread, |r| r.environment().owner.is_resident())
                .expect("installed Sessions")
                .unwrap_or(false),
            "R7 Store-read identity remains Resident"
        );

        let unpinned_realizer = Arc::new(RecordingMcpRealizer::default());
        let unpinned_host =
            ((crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
                .await
                .with_store_dir(storage.path())
                .await)
                .with_session_execution(
                    crate::SessionExecutionDependencies::default()
                        .with_mcp_attachment_realizer(unpinned_realizer.clone()),
                ))
            .build()
            .expect("validated Host dependencies");

        let unpinned_managed = unpinned_host.clone();
        unpinned_managed
            .run_assembly
            .install_test_environment_binding_sink(Arc::new(ToggleBindingSink {
                fail: std::sync::atomic::AtomicBool::new(false),
                calls: std::sync::atomic::AtomicUsize::new(0),
                binding: Mutex::new(None),
            }));
        let error = crate::application::worker_projection::realize_session(
            &unpinned_host.run_assembly,
            &RecoveryControl::fixed(frozen_projection()),
            &awaken_session_types::SessionId::from(first_use_thread),
            first_use_directive,
            None,
            None,
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect_err("R8 first-use stdio MCP without a publication must fail closed");
        assert!(
            error.to_string().contains("exact claimed Agent snapshot"),
            "R8/E4: {error}"
        );
        assert!(
            unpinned_realizer.calls.lock().unwrap().is_empty(),
            "R8/E6: no MCP stage, publish, or drain may run"
        );
        assert!(
            unpinned_host
                .realization_executor
                .environments()
                .session_environment(first_use_thread)
                .await
                .expect("installed Sessions")
                .is_none(),
            "R8/E6: no unpinned Runtime may be installed"
        );

        let cold = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies");
        let error = crate::application::worker_projection::realize_session(
            &cold.run_assembly,
            &control,
            &awaken_session_types::SessionId::from(thread),
            directive,
            None,
            None,
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect_err("R3 cold lease-only recovery must fail closed");
        assert!(
            error.to_string().contains("exact claimed Agent snapshot"),
            "R3/E4: {error}"
        );
    }

    // These rows have independent Root/Host lifecycles. Separate test futures
    // keep nested cold construction within the default Windows test stack;
    // every original physical-result and authority assertion remains below.
    #[tokio::test]
    async fn cold_rebuild_preserves_original_materialization_without_source_closure() {
        let storage = tempfile::tempdir().expect("storage");
        let rebuild_thread = "cold-missing-environment";
        let rebuild_activation = test_activation(rebuild_thread, "run-cold-missing-environment");
        let (rebuild_host, rebuild_control, rebuild_directive, missing_handle, rebuild_roots) =
            Box::pin(missing_environment_recovery_fixture(
                storage.path(),
                rebuild_thread,
                &rebuild_activation.snapshot,
            ))
            .await;
        // R5 safety behavior changed: the old typed UID is definitively gone,
        // but local stop/inode loss is not the Root's missing Source closure/
        // domain-access witness. The old synthetic sink accepted a new handle;
        // canonical Managed Rebuild now stays MaterializationPending. A future
        // complete closure/access slice must add a separate real positive.
        // Rule: known loss + unknown accepted source witness => retryable exact
        // pending error, zero target Create/publication, original intent/binding
        // retained. No ignore/skip or fake grant substitutes for this boundary.
        let before_rebuild = rebuild_roots
            .read_projection(rebuild_thread, &rebuild_control.current_projection())
            .await
            .expect("R5 original Root before refused target effect");
        assert_eq!(
            before_rebuild.environment.binding(),
            Some(serde_json::to_string(&missing_handle).unwrap().as_str()),
            "R5 original source binding remains the typed lost incarnation"
        );
        let error = crate::application::worker_projection::realize_session(
            &rebuild_host.run_assembly,
            &rebuild_control,
            &awaken_session_types::SessionId::from(rebuild_thread),
            rebuild_directive,
            None,
            Some(&rebuild_activation.snapshot),
            true,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect_err("R5 canonical Managed Rebuild needs accepted Source closure/access");
        match error {
            awaken_dispatch_execution::Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(message),
            ) => assert_eq!(
                message,
                "run failed: Session materialization remains pending its exact original effect and source closure",
                "R5 exact retryable MaterializationPending receiver",
            ),
            other => {
                panic!("R5 expected retryable original materialization refusal: {other:?}")
            }
        }
        assert!(
            rebuild_host
                .run_assembly
                .session_environment_handle(rebuild_thread)
                .await
                .is_none(),
            "R5 pending target publishes no replacement Environment"
        );
        assert!(
            !rebuild_host
                .storage_dir()
                .unwrap()
                .join("sandboxes")
                .join(rebuild_thread)
                .exists(),
            "R5 refused authorization creates no target physical root"
        );
        let after_rebuild = rebuild_roots
            .read_projection(rebuild_thread, &before_rebuild)
            .await
            .expect("R5 original Root after refused target effect");
        assert_eq!(
            after_rebuild.environment, before_rebuild.environment,
            "R5 original source binding/effect/generation preserved"
        );
        assert_eq!(
            after_rebuild.environment_materialization, before_rebuild.environment_materialization,
            "R5 original Create intent remains captured; no target receipt fills history"
        );
    }

    #[tokio::test]
    async fn cold_continuity_rejects_a_physically_missing_environment() {
        let storage = tempfile::tempdir().expect("storage");
        let continuity_thread = "cold-continuity-missing";
        let continuity_activation =
            test_activation(continuity_thread, "run-cold-continuity-missing");
        let (continuity_host, continuity_control, continuity_directive, _, _) =
            Box::pin(missing_environment_recovery_fixture(
                storage.path(),
                continuity_thread,
                &continuity_activation.snapshot,
            ))
            .await;
        let error = crate::application::worker_projection::realize_session(
            &continuity_host.run_assembly,
            &continuity_control,
            &awaken_session_types::SessionId::from(continuity_thread),
            continuity_directive,
            None,
            Some(&continuity_activation.snapshot),
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .expect_err("R6 continuity policy rejects a missing Environment");
        assert!(
            error.to_string().contains("unavailable or terminal"),
            "R6 fails for exact physical unavailability, not fixture drift: {error}"
        );
        assert!(
            continuity_host
                .realization_executor
                .environments()
                .session_environment(continuity_thread)
                .await
                .expect("installed Sessions")
                .is_none(),
            "R6 continuity failure publishes no replacement Environment"
        );
    }
}
