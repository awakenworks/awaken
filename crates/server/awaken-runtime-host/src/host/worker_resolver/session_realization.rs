//! Worker-side projection and effect adapters for the canonical Session
//! realization driver.

use super::*;
mod projection_synchronizer;
pub(super) use projection_synchronizer::WorkerMcpEffects;
use projection_synchronizer::WorkerProjectionSynchronizer;
#[cfg(test)]
use projection_synchronizer::merge_legacy_claimed_environment_binding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionRealizationWorkerEffect {
    /// Keep the claim parked while the Session Work slot is occupied.
    Defer,
    /// Relinquish the claim through the retryable execution-failure path.
    Relinquish,
    /// Settle the still-current claim with an absorbing Run failure.
    Absorb,
}

pub(super) const fn session_realization_worker_effect(
    disposition: awaken_session_contract::SessionRealizationControlDisposition,
) -> SessionRealizationWorkerEffect {
    match disposition {
        awaken_session_contract::SessionRealizationControlDisposition::NotReady => {
            SessionRealizationWorkerEffect::Defer
        }
        awaken_session_contract::SessionRealizationControlDisposition::Retryable => {
            SessionRealizationWorkerEffect::Relinquish
        }
        awaken_session_contract::SessionRealizationControlDisposition::Terminal => {
            SessionRealizationWorkerEffect::Absorb
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_control_disposition_projects_exact_worker_effect() {
    let selector = kani::any::<u8>() % 3;
    let (disposition, expected) = match selector {
        0 => (
            awaken_session_contract::SessionRealizationControlDisposition::NotReady,
            SessionRealizationWorkerEffect::Defer,
        ),
        1 => (
            awaken_session_contract::SessionRealizationControlDisposition::Retryable,
            SessionRealizationWorkerEffect::Relinquish,
        ),
        _ => (
            awaken_session_contract::SessionRealizationControlDisposition::Terminal,
            SessionRealizationWorkerEffect::Absorb,
        ),
    };

    assert_eq!(session_realization_worker_effect(disposition), expected);
}

impl HostWorkerResolver {
    fn map_session_realization_drive_error(
        error: awaken_session_contract::SessionRealizationDriveError,
    ) -> awaken_run_ingress::Error {
        match error {
            awaken_session_contract::SessionRealizationDriveError::Effect(error)
                if error.kind == awaken_session_contract::RunErrorKind::Unavailable =>
            {
                Self::execution_error(error.to_string())
            }
            awaken_session_contract::SessionRealizationDriveError::Control(control) => {
                match session_realization_worker_effect(control.disposition()) {
                    SessionRealizationWorkerEffect::Defer => {
                        awaken_run_ingress::Error::ResolutionNotReady(
                            "Session realization is not ready".into(),
                        )
                    }
                    SessionRealizationWorkerEffect::Relinquish => {
                        Self::execution_error(control.to_string())
                    }
                    SessionRealizationWorkerEffect::Absorb => {
                        Self::terminal_resolution_error(control.to_string())
                    }
                }
            }
            retryable @ awaken_session_contract::SessionRealizationDriveError::DidNotConverge => {
                Self::execution_error(retryable.to_string())
            }
            error => Self::terminal_resolution_error(error.to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn realize_session(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
    ) -> Result<(), awaken_run_ingress::Error> {
        let realization = host.session_slots.realization_lock(session_id);
        let _realization = realization.lock().await;
        let synchronizer = WorkerProjectionSynchronizer::for_directive(
            host,
            &directive,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
            None,
        );
        Self::drive_session_realization_with_synchronizer(
            control,
            session_id,
            directive,
            synchronizer,
        )
        .await
        .map_err(Self::map_session_realization_drive_error)
    }

    /// Drive one already-serialized directive under the Session's realization
    /// lock so no caller can create a parallel phase driver.
    pub(crate) async fn drive_session_realization(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claimed: Option<&awaken_run_ingress::Claimed>,
    ) -> Result<(), awaken_run_ingress::Error> {
        let claim = claimed.map(|claimed| awaken_run_ingress::RunClaim::from(&claimed.lease));
        let published_snapshot = claimed.and_then(|claimed| {
            (claimed.request.thread_id().0 == session_id)
                .then_some(&claimed.request.activation.snapshot)
        });
        let rebuild_unavailable_environment = claimed.is_some_and(|claimed| {
            claimed.request.placement.recovery
                == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth
        });
        let claimed_environment_binding = claimed.and_then(|claimed| claimed.sandbox.as_deref());
        let synchronizer = WorkerProjectionSynchronizer::for_directive(
            host,
            &directive,
            claim.as_ref(),
            published_snapshot,
            rebuild_unavailable_environment,
            claimed_environment_binding,
        );
        Self::drive_session_realization_with_synchronizer(
            control,
            session_id,
            directive,
            synchronizer,
        )
        .await
        .map_err(Self::map_session_realization_drive_error)
    }

    /// Sole Runtime-to-contract effect path. The production entry and the
    /// test-only projection seam differ only in how they construct this adapter.
    async fn drive_session_realization_with_synchronizer(
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        synchronizer: WorkerProjectionSynchronizer<'_>,
    ) -> Result<(), awaken_session_contract::SessionRealizationDriveError> {
        let host = synchronizer.host;
        awaken_session_contract::drive_session_realization(
            session_id,
            synchronizer.claim.map(|claim| claim.run_id.clone()),
            control,
            &synchronizer,
            &WorkerMcpEffects(host),
            directive,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, ToggleBindingSink, committed_environment, deferred_environment,
        eager_environment, install_complete_projection_for_snapshot, managed_test_host,
        test_activation, with_empty_session_resources,
    };
    use awaken_session_contract::SessionRuntime as _;
    use std::sync::{Arc, Mutex};

    /// Legacy raw-claim merge cause/effect graph: C1 the durable Environment
    /// phase; C2 a raw RunDispatch sandbox cache is present. E1 only a genuinely
    /// legacy Unmaterialized root is upgraded to Resident; E2 every resident or
    /// continuation phase remains byte-for-byte root-owned; E3 absence is a no-op.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | L1 | Unmaterialized | present | E1 |
    /// | L2 | Resident/Suspending | present | E2 |
    /// | L3 | Hibernated/Restoring | present | E2; never resurrect source |
    /// | L4 | any | absent | E3 |
    #[test]
    fn legacy_raw_claim_only_repairs_an_unmaterialized_environment() {
        let generation = awaken_session_contract::SandboxGeneration::new(
            "legacy-claim",
            1,
            u64::MAX,
            "environment",
            "image",
        );
        let operation = awaken_session_contract::SessionEnvironmentOperation::new(
            "workspace",
            "legacy-claim",
            "suspend",
            &generation,
            1,
            None,
            None,
        );
        let checkpoint = awaken_provisioning_contract::SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "awaken-fs-v1".into(),
            digest: "digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "image".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: operation.effect_id.clone(),
        };
        let root_binding = "{\"provider_kind\":\"root\"}";
        let raw_claim = "{\"provider_kind\":\"claim\"}";

        let mut legacy = awaken_session_contract::SessionEnvironmentState::Unmaterialized;
        merge_legacy_claimed_environment_binding(&mut legacy, Some(raw_claim));
        assert!(
            matches!(
                legacy,
                awaken_session_contract::SessionEnvironmentState::Resident {
                    ref binding,
                    effect_id: None,
                    generation: None,
                    idle_since_unix_ms: None,
                } if binding == raw_claim
            ),
            "L1"
        );

        let protected = [
            awaken_session_contract::SessionEnvironmentState::Resident {
                binding: root_binding.into(),
                effect_id: Some("effect".into()),
                generation: Some(generation.clone()),
                idle_since_unix_ms: None,
            },
            awaken_session_contract::SessionEnvironmentState::Suspending {
                operation: operation.clone(),
                source_effect_id: Box::new("source-effect".into()),
                source_binding: root_binding.into(),
                generation: generation.clone(),
                suspend_phase: awaken_session_contract::SuspendPhase::Quiescing,
                checkpoint: None,
                source_release_preparation: None,
            },
            awaken_session_contract::SessionEnvironmentState::Hibernated {
                checkpoint: checkpoint.clone(),
                generation: generation.clone(),
            },
            awaken_session_contract::SessionEnvironmentState::Restoring {
                operation,
                checkpoint,
                generation,
            },
        ];
        for expected in protected {
            let mut actual = expected.clone();
            merge_legacy_claimed_environment_binding(&mut actual, Some(raw_claim));
            assert_eq!(actual, expected, "L2/L3");
        }

        let mut absent = awaken_session_contract::SessionEnvironmentState::Unmaterialized;
        merge_legacy_claimed_environment_binding(&mut absent, None);
        assert_eq!(
            absent,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized,
            "L4"
        );
    }

    /// C1-C3: a cold Worker must derive eager-vs-deferred provisioning only from
    /// the immutable dispatch envelope. A complete eager Runtime plus exact empty
    /// Resource projection realizes eagerly, an exact on-tool-use projection stays
    /// sandbox-free during Brain resolution, and a malformed projection is rejected
    /// by claim admission before any Worker or Sandbox effect. Moving C3 earlier
    /// preserves fail-closed behavior while keeping one projection decoder at the
    /// run-ingress contract boundary.
    #[tokio::test]
    async fn cold_worker_runtime_projection_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue, WorkerResolver as _};

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let managed = managed_test_host(host.clone());
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let eager_environment = eager_environment();
        let eager_activation = test_activation("cold-legacy", "run-legacy");
        install_complete_projection_for_snapshot(
            &managed,
            "cold-legacy",
            host.local_workspace(),
            eager_environment.clone(),
            &eager_activation.snapshot,
        )
        .await;
        let eager_runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
            eager_environment,
            Some(Default::default()),
            Vec::new(),
        )
        .expect("encode eager Runtime projection");
        store
            .enqueue(with_empty_session_resources(
                awaken_run_ingress::RunDispatch::new(eager_activation)
                    .with_session_runtime(eager_runtime),
                host.local_workspace(),
            ))
            .await
            .expect("enqueue legacy projection with exact empty Resources");
        let legacy = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim legacy projection")
            .expect("legacy projection available");
        resolver
            .worker_for_claimed(&legacy)
            .await
            .expect("C1 complete eager projection remains eager");
        assert!(
            host.session_environment("cold-legacy").await.is_some(),
            "C1"
        );

        let deferred_environment = deferred_environment();
        let deferred_activation = test_activation("cold-deferred", "run-deferred");
        install_complete_projection_for_snapshot(
            &managed,
            "cold-deferred",
            host.local_workspace(),
            deferred_environment.clone(),
            &deferred_activation.snapshot,
        )
        .await;
        let runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
            deferred_environment,
            Some(Default::default()),
            Vec::new(),
        )
        .expect("encode runtime projection");
        store
            .enqueue(with_empty_session_resources(
                awaken_run_ingress::RunDispatch::new(deferred_activation)
                    .with_session_runtime(runtime),
                host.local_workspace(),
            ))
            .await
            .expect("enqueue deferred projection");
        let deferred = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim deferred projection")
            .expect("deferred projection available");
        resolver
            .worker_for_claimed(&deferred)
            .await
            .expect("C2 cold Brain resolution stays deferred");
        assert!(
            host.session_environment("cold-deferred").await.is_none(),
            "C2"
        );
        assert!(
            host.session_slots
                .read("cold-deferred", |slot| slot.deferred_executor.is_some()
                    && slot.tools.as_ref().is_some_and(|tools| {
                        tools == &awaken_session_contract::SessionToolConfiguration::default()
                    }))
                .unwrap_or(false),
            "C2"
        );

        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-invalid",
                    "run-invalid",
                ))
                .with_session_runtime(awaken_run_ingress::SessionRuntimeEnvelope::new("{")),
            )
            .await
            .expect("enqueue invalid projection");
        let error = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect_err("C3 malformed runtime projection must fail claim admission");
        assert!(
            error
                .to_string()
                .contains("Session runtime credential projection is invalid")
        );
        assert!(
            host.session_environment("cold-invalid").await.is_none(),
            "C3"
        );
    }

    #[test]
    fn session_realization_preserves_retryable_and_absorbing_failure_classes() {
        // Cause/effect graph: C1 the canonical driver classifies an effect as
        // Unavailable, C2 Control reports transient ownership/readiness, or C3
        // the phase is permanently invalid. Effects: C1/C2 relinquish or defer
        // the WorkQueue claim; only C3 becomes TerminalResolution. This preserves
        // the Session aggregate's already-persisted retry decision instead of
        // creating a second host-side policy.
        //
        // | Rule | driver cause | effect |
        // | R1 | unavailable effect | retryable Execution |
        // | R2 | Control unavailable/conflict | retryable Execution |
        // | R3 | Control not ready | ResolutionNotReady |
        // | R4 | permanent effect/invalid Control | TerminalResolution |
        let retryable_effect = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_contract::SessionRealizationDriveError::Effect(
                awaken_session_contract::RunError::unavailable_classified(
                    "mcp_material_source_unavailable",
                    "materializer unavailable",
                ),
            ),
        );
        assert!(matches!(
            retryable_effect,
            awaken_run_ingress::Error::Execution(_)
        ));

        let retryable_control = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_contract::SessionRealizationDriveError::Control(
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                    "repository unavailable".into(),
                ),
            ),
        );
        assert!(matches!(
            retryable_control,
            awaken_run_ingress::Error::Execution(_)
        ));

        let not_ready = HostWorkerResolver::map_session_realization_drive_error(
            awaken_session_contract::SessionRealizationDriveError::Control(
                awaken_session_contract::SessionRealizationControlFailure::NotReady,
            ),
        );
        assert!(matches!(
            not_ready,
            awaken_run_ingress::Error::ResolutionNotReady(_)
        ));

        for permanent in [
            awaken_session_contract::SessionRealizationDriveError::Effect(
                awaken_session_contract::RunError::classified(
                    "mcp_credential_revision_mismatch",
                    "invalid revision",
                ),
            ),
            awaken_session_contract::SessionRealizationDriveError::Control(
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    "invalid phase".into(),
                ),
            ),
        ] {
            assert!(matches!(
                HostWorkerResolver::map_session_realization_drive_error(permanent),
                awaken_run_ingress::Error::TerminalResolution(_)
            ));
        }
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: Mutex<Vec<&'static str>>,
        required_environment: Option<(std::sync::Weak<SharedHost>, String)>,
        stage_entered: Option<Arc<tokio::sync::Notify>>,
        release_stage: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
        {
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
                .as_ref()
                .is_none_or(|(host, thread)| {
                    host.upgrade().is_some_and(|host| {
                        host.session_slots
                            .read(thread, |slot| slot.environment_owner.is_resident())
                            .unwrap_or(false)
                    })
                });
            if !resident {
                return Err(awaken_session_contract::RunError::classified(
                    "test_session_runtime_missing",
                    "MCP stage ran before the frozen Environment was resident",
                ));
            }
            Ok(awaken_session_contract::McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    pub(super) struct RecoveryControl {
        projection: Arc<Mutex<awaken_session_contract::FrozenSessionProjection>>,
    }

    impl RecoveryControl {
        pub(super) fn fixed(projection: awaken_session_contract::FrozenSessionProjection) -> Self {
            Self {
                projection: Arc::new(Mutex::new(projection)),
            }
        }

        pub(super) fn current_projection(
            &self,
        ) -> awaken_session_contract::FrozenSessionProjection {
            self.projection.lock().unwrap().clone()
        }
    }

    pub(super) struct RecoveryEnvironmentBindingSink {
        projection: Arc<Mutex<awaken_session_contract::FrozenSessionProjection>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RecoveryEnvironmentBindingSink {
        pub(super) fn new(control: &RecoveryControl) -> Self {
            Self {
                projection: control.projection.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        pub(super) fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionEnvironmentBindingSink for RecoveryEnvironmentBindingSink {
        async fn authorize(
            &self,
            intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
        ) -> Result<
            awaken_session_contract::SessionEnvironmentEffectAuthorization,
            awaken_session_contract::RunError,
        > {
            // Fixture cause/effect rule: C1 Control owns the current frozen
            // Environment state and C2 Runtime submits one exact intent. E1
            // delegate the decision to the contract state machine; never
            // approximate AlreadyApplied from a cached binding.
            let projection = self.projection.lock().unwrap();
            projection
                .environment
                .authorize_effect(
                    intent,
                    &projection.baseline.environment.config_fingerprint.0,
                    None,
                )
                .map_err(|error| {
                    awaken_session_contract::RunError::classified(
                        "test_environment_effect_rejected",
                        error.to_string(),
                    )
                })
        }

        async fn persist(
            &self,
            receipt: awaken_session_contract::SessionEnvironmentReceipt,
        ) -> Result<
            awaken_session_contract::SessionEnvironmentState,
            awaken_session_contract::RunError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let committed = committed_environment(receipt);
            self.projection.lock().unwrap().environment = committed.clone();
            Ok(committed)
        }
    }

    struct RenewalDuringStageControl {
        projection: awaken_session_contract::FrozenSessionProjection,
        stage: awaken_session_contract::StageMcpAttachment,
        lease: Mutex<awaken_session_contract::SessionRealizationLease>,
        renewal_calls: std::sync::atomic::AtomicUsize,
        renewals_on_activate_remaining: std::sync::atomic::AtomicUsize,
        repeat_stage_without_progress: bool,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RenewalDuringStageControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn renew_session_realization(
            &self,
            command: awaken_session_contract::RenewSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationLease,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.renewal_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut lease = self.lease.lock().unwrap();
            if !awaken_session_contract::realization_lease_generation_authorizes(
                &lease,
                &command.asserted_lease,
            ) || command.requested_expires_at_unix_ms < lease.expires_at_unix_ms
            {
                return Err(
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership,
                );
            }
            lease.expires_at_unix_ms = command.requested_expires_at_unix_ms;
            Ok(lease.clone())
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            if self.repeat_stage_without_progress {
                return Ok(awaken_session_contract::SessionRealizationDirective {
                    projection: self.projection.clone(),
                    lease: self.lease.lock().unwrap().clone(),
                    action: awaken_session_contract::SessionRealizationAction::Stage {
                        prepare_session: false,
                        mcp_stages: vec![self.stage.clone()],
                    },
                });
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
                receipt.generation.lease_expires_at_unix_ms < lease.expires_at_unix_ms
            });
            let action = if needs_renewal_stage {
                let mut stage = self.stage.clone();
                stage.generation.lease_expires_at_unix_ms = lease.expires_at_unix_ms;
                stage.stage_idempotency_key = format!("renew:{}", lease.expires_at_unix_ms);
                awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                }
            } else {
                awaken_session_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                }
            };
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.projection.clone(),
                lease,
                action,
            })
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            let lease = self.lease.lock().unwrap().clone();
            let action =
                if command.published.iter().any(|generation| {
                    generation.lease_expires_at_unix_ms < lease.expires_at_unix_ms
                }) {
                    let mut stage = self.stage.clone();
                    stage.generation.lease_expires_at_unix_ms = lease.expires_at_unix_ms;
                    stage.stage_idempotency_key = format!("renew:{}", lease.expires_at_unix_ms);
                    awaken_session_contract::SessionRealizationAction::Stage {
                        prepare_session: false,
                        mcp_stages: vec![stage],
                    }
                } else {
                    awaken_session_contract::SessionRealizationAction::Complete
                };
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.projection.clone(),
                lease,
                action,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_run_ingress_contract::ClaimedSessionControl for RenewalDuringStageControl {
        async fn resume_frozen(
            &self,
            _claim: &awaken_run_ingress::RunClaim,
            _session_id: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionRealizationDirective>,
            awaken_run_ingress_contract::ClaimedSessionControlError,
        > {
            Err(
                awaken_run_ingress_contract::ClaimedSessionControlError::new(
                    "not used by the renewal concurrency fixture",
                ),
            )
        }
    }

    fn renewal_stage_fixture(
        thread: &str,
        expires_at_unix_ms: u64,
    ) -> (
        awaken_session_contract::FrozenSessionProjection,
        awaken_session_contract::SessionRealizationLease,
        awaken_session_contract::StageMcpAttachment,
    ) {
        let mut projection = frozen_projection();
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms,
        };
        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: expires_at_unix_ms,
            },
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "stage-browser-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::parse_http(
                "https://browser.example.test/mcp",
            )
            .expect("HTTP MCP target"),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };
        projection
            .mcp
            .push(awaken_session_contract::SessionMcpAttachment {
                attachment_id: stage.generation.attachment_id.clone(),
                name: stage.name.clone(),
                generation: stage.generation.generation,
                target: stage.target.clone(),
                prompts_as_skills: stage.prompts_as_skills,
                origin: awaken_session_contract::McpAttachmentOrigin::Agent,
                credential: stage.credential.clone(),
                selected_plaintext_holder: stage.selected_plaintext_holder.clone(),
                state: awaken_session_contract::McpAttachmentState::Realizing,
                publication_acknowledged: false,
                realization: Some(awaken_session_contract::McpRealizationClaim {
                    realization_id: stage.realization_id.clone(),
                    runtime_incarnation: stage.generation.runtime_incarnation.clone(),
                    lease_epoch: stage.generation.lease_epoch,
                    lease_expires_at_unix_ms: stage.generation.lease_expires_at_unix_ms,
                    stage_idempotency_key: stage.stage_idempotency_key.clone(),
                }),
                attempts: 1,
                last_error: None,
            });
        (projection, lease, stage)
    }

    /// Test-only typed-outcome seam. Production callers enter through
    /// `drive_session_realization`; these progress tests construct the same
    /// synchronizer and call the sole driver directly so they can assert its
    /// un-erased `DidNotConverge` result.
    async fn drive_test_session_realization(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
    ) -> Result<(), awaken_session_contract::SessionRealizationDriveError> {
        let synchronizer =
            WorkerProjectionSynchronizer::for_directive(host, &directive, None, None, false, None);
        HostWorkerResolver::drive_session_realization_with_synchronizer(
            control,
            session_id,
            directive,
            synchronizer,
        )
        .await
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RecoveryControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.current_projection(),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                },
            })
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.current_projection(),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            Ok(())
        }
    }

    pub(super) fn frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
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
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            previous_resource_manifest: Some(
                awaken_session_contract::SessionResourceManifest::new(
                    "workspace",
                    awaken_session_contract::ResolvedSessionResources::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    /// Build the one Store-read Resident fixture through the shared binding
    /// projection. Binding-only `set_resident` is legacy decode evidence and
    /// cannot represent current cold recovery because it drops the effect
    /// identity and generation used by rebuild fencing.
    fn committed_resident_environment(
        thread: &str,
        binding: String,
    ) -> awaken_session_contract::SessionEnvironmentState {
        committed_environment(awaken_session_contract::SessionEnvironmentReceipt::new(
            thread,
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
            binding,
            None,
        ))
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
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "resident-renewal-resources";
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let resident = frozen_projection();
        host.install_frozen_session_projection(thread, resident.clone(), None, true, None)
            .await
            .expect("M1 establish resident baseline");
        let mut remote = resident;
        remote.resource_revision = 1;
        remote.resources = remote
            .resources
            .with_skills(vec![awaken_session_contract::ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: "design".into(),
                version: 36,
                bundle_sha256: "sha256-design-v36".into(),
            }])
            .unwrap();
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let synchronizer = WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        };
        synchronizer
            .synchronize_session_projection(thread, &remote, &lease, false)
            .await
            .expect("M1 lease-only renewal reuses resident projection");
        assert!(host.thread_resource_manifest(thread).is_none(), "M1");
        assert_eq!(
            host.session_slots
                .read(thread, |slot| slot.realization_lease.clone())
                .flatten(),
            Some(lease.clone()),
            "M1 lease still advances"
        );

        let cold = WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection("cold-renewal-resources", &remote, &lease, false)
        .await
        .expect_err("M2 cold renewal must fail closed");
        assert!(cold.to_string().contains("cannot cold-materialize"), "M2");

        let preparing = synchronizer
            .synchronize_session_projection(thread, &remote, &lease, true)
            .await
            .expect_err("M3 preparation needs a material source");
        assert!(preparing.to_string().contains("Skill"), "M3: {preparing}");
    }

    /// Dynamic-publication cause/effect graph: C1 the Coordinator projects the
    /// exact publication before any Run can be claimed; C2 a compatibility
    /// renewal omits it; C3 a claimed Run carries a Session-effective overlay
    /// with the same frozen coordinates; C4 claimed coordinates differ. E1
    /// retain the immutable publication; E2 reuse it; E3 accept the overlay
    /// without replacing E1; E4 reject the foreign Run and preserve E1.
    /// Decision rules: P1 C1=>E1, P2 E1+C2=>E2, P3 E1+C3=>E3, P4 C4=>E4.
    #[tokio::test]
    async fn session_projection_delivers_publication_before_first_run_claim() {
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "dynamic-publication-renewal";
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let mut projection = frozen_projection();
        projection.baseline.agent_revision = Some(7);
        projection.baseline.runtime = Some("acp:claude".into());
        let mut snapshot = test_activation(thread, "dynamic-publication").snapshot;
        snapshot.metadata.source.revision = 7;
        let mut binding = snapshot.resolved_spec.model_binding.binding().clone();
        binding.backend_ref = "acp:claude".into();
        snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
        projection.agent_publication = Some(snapshot.clone());
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(thread, &projection, &lease, true)
        .await
        .expect("projection publication realizes the Session before a Run claim");

        let mut compatibility_renewal = projection.clone();
        compatibility_renewal.agent_publication = None;
        WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(thread, &compatibility_renewal, &lease, false)
        .await
        .expect("lease renewal reuses the retained immutable publication");
        let retained = host
            .session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten();
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
            host: host.as_ref(),
            claim: None,
            published_snapshot: Some(&effective),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(thread, &projection, &lease, false)
        .await
        .expect("P3 Session-effective Run snapshot shares the frozen coordinates");
        assert_eq!(
            host.session_slots
                .read(thread, |slot| slot.published_snapshot.clone())
                .flatten()
                .as_ref(),
            Some(&snapshot),
            "P3"
        );

        let mut foreign = effective;
        foreign.metadata.source.revision += 1;
        let error = WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: Some(&foreign),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection(thread, &projection, &lease, false)
        .await
        .expect_err("P4 foreign claimed coordinates are rejected");
        assert_eq!(error.code, "session_runtime_publication_conflict", "P4");

        let mut missing = projection.clone();
        missing.agent_publication = None;
        let error = WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
            claimed_environment_binding: None,
        }
        .synchronize_session_projection("publication-missing", &missing, &lease, true)
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
                    let mut binding = delivered.resolved_spec.model_binding.binding().clone();
                    binding.backend_ref = "native:other".into();
                    delivered.resolved_spec.model_binding =
                        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
                }
            }
            let error = WorkerProjectionSynchronizer {
                host: host.as_ref(),
                claim: None,
                published_snapshot: None,
                rebuild_unavailable_environment: false,
                requires_runtime_before_effects: false,
                claimed_environment_binding: None,
            }
            .synchronize_session_projection(session_id, &conflicting, &lease, true)
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
        let initial_expiry = 1_000;
        let renewed_expiry = 2_000;
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
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_session_control(control.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        let directive = awaken_session_contract::SessionRealizationDirective {
            projection,
            lease,
            action: awaken_session_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![stage],
            },
        };

        let initial_host = host.clone();
        let initial_control = control.clone();
        let initial = tokio::spawn(async move {
            HostWorkerResolver::realize_session(
                &initial_host,
                initial_control.as_ref(),
                thread,
                directive,
                None,
                None,
                false,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), stage_entered.notified())
            .await
            .expect("R1 initial Stage entered");
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                host.renew_due_session_realizations(
                    0,
                    awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(
                        renewed_expiry,
                    ),
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
            host.session_slots
                .read(thread, |slot| slot
                    .realization_lease
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms))
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
        let (projection, lease, stage) = renewal_stage_fixture(thread, 1_000);
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            renewal_calls: std::sync::atomic::AtomicUsize::new(0),
            renewals_on_activate_remaining: std::sync::atomic::AtomicUsize::new(5),
            repeat_stage_without_progress: false,
        });
        let realizer = Arc::new(RecordingMcpRealizer::default());
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_session_control(control.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        drive_test_session_realization(
            &host,
            control.as_ref(),
            thread,
            awaken_session_contract::SessionRealizationDirective {
                projection,
                lease,
                action: awaken_session_contract::SessionRealizationAction::Stage {
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
        let (projection, lease, stage) = renewal_stage_fixture(thread, 1_000);
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            renewal_calls: std::sync::atomic::AtomicUsize::new(0),
            renewals_on_activate_remaining: std::sync::atomic::AtomicUsize::new(0),
            repeat_stage_without_progress: true,
        });
        let realizer = Arc::new(RecordingMcpRealizer::default());
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_session_control(control.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        let error = drive_test_session_realization(
            &host,
            control.as_ref(),
            thread,
            awaken_session_contract::SessionRealizationDirective {
                projection,
                lease,
                action: awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            },
        )
        .await
        .expect_err("D2/E3 exact repetition fails closed");
        assert!(matches!(
            error,
            awaken_session_contract::SessionRealizationDriveError::DidNotConverge
        ));
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage"],
            "D2/E3 detects no progress before duplicating the effect"
        );
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
         * rebuild only when the claimed Run's explicit recovery policy permits it;
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
         * | R5 | missing | no | yes | no | rebuild | E5 |
         * | R6 | missing | no | yes | no | continuity | fail closed |
         * | R7 | none yet | no | yes | yes | either | E7; install publication, then stage |
         * | R8 | none yet | no | no | yes | either | E4 + E6 |
         */
        let storage = tempfile::tempdir().expect("storage");
        let thread = "cold-frozen-environment";
        let activation = test_activation(thread, "run-cold-frozen-environment");
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let original = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let original_managed = managed_test_host(original.clone());
        let mut frozen = frozen_projection();
        let mut original_projection = frozen.clone();
        original_projection.agent_publication = Some(activation.snapshot.clone());
        let original_control = RecoveryControl::fixed(original_projection.clone());
        let original_binding_sink =
            Arc::new(RecoveryEnvironmentBindingSink::new(&original_control));
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &original_managed,
            original_binding_sink.clone(),
        );
        awaken_session_contract::SessionRuntime::install_session_projection(
            &original_managed,
            thread,
            original_projection,
            awaken_session_contract::SessionProjectionInstallMode::Realization {
                lease: lease.clone(),
                prepare_session: true,
            },
        )
        .await
        .expect("R1 install exact original frozen projection");
        let original_ctx = original
            .ctx_for_snapshot(thread, Some("agent-a"), Some(activation.snapshot.clone()))
            .await
            .expect("R1 original Environment");
        let handle = original_ctx
            .env
            .as_ref()
            .expect("R1 eager Environment")
            .handle();
        let binding = serde_json::to_string(&handle).expect("R1 durable binding");
        let committed_environment = original_control.current_projection().environment;
        assert_eq!(
            committed_environment.binding(),
            Some(binding.as_str()),
            "R1/C9 Store-read binding remains exact"
        );
        frozen.environment = committed_environment;
        assert_eq!(
            original_binding_sink.calls(),
            1,
            "R1/C9 producer commits and reads one exact durable identity"
        );
        drop(original_ctx);
        drop(original_managed);
        drop(original);

        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "worker-a".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "recover-browser-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::sandbox_stdio(
                "playwright-mcp",
                vec!["--headless".into()],
            )
            .expect("sandbox stdio target"),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };
        let directive = awaken_session_contract::SessionRealizationDirective {
            projection: frozen.clone(),
            lease,
            action: awaken_session_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![stage.clone()],
            },
        };
        let control = RecoveryControl::fixed(frozen.clone());
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let realizer = Arc::new(RecordingMcpRealizer {
            required_environment: Some((Arc::downgrade(&host), thread.into())),
            ..Default::default()
        });
        let managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        let binding_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &managed,
            binding_sink,
        );
        HostWorkerResolver::realize_session(
            &host,
            &control,
            thread,
            directive.clone(),
            None,
            Some(&activation.snapshot),
            false,
        )
        .await
        .expect("R1 frozen Environment recovery");
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(handle.clone()),
            "R1/E1-E3"
        );
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R1/E1"
        );

        // Publication deliberately evicts the rebuildable SessionCtx before the
        // final claimed resolve. A heartbeat may enter this exact gap; the
        // retained Environment remains the publication-authorized effect owner.
        host.evict_session_for_rebuild(thread).await;
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_none()
                    && slot.environment_owner.is_resident())
                .unwrap_or(false),
            "R2 fixture is the publish-to-final-resolve gap"
        );
        let mut resident_directive = directive.clone();
        resident_directive.projection = control.current_projection();
        HostWorkerResolver::realize_session(
            &host,
            &control,
            thread,
            resident_directive,
            None,
            None,
            false,
        )
        .await
        .expect("R2 resident Environment is the already-installed publication authority");
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish", "stage", "publish"],
            "R2 reuses the resident Environment without another claimed snapshot"
        );

        let first_use_thread = "cold-first-use-environment";
        let first_use_activation = test_activation(first_use_thread, "run-cold-first-use");
        let first_use_projection = frozen_projection();
        let first_use_stage = awaken_session_contract::StageMcpAttachment {
            generation: awaken_session_contract::McpGenerationRef {
                session_id: first_use_thread.into(),
                ..stage.generation.clone()
            },
            realization_id: "realize-browser-first-use".into(),
            stage_idempotency_key: "stage-browser-first-use".into(),
            ..stage.clone()
        };
        let first_use_directive = awaken_session_contract::SessionRealizationDirective {
            projection: first_use_projection.clone(),
            lease: awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
            action: awaken_session_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![first_use_stage],
            },
        };
        let first_use_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let first_use_realizer = Arc::new(RecordingMcpRealizer {
            required_environment: Some((Arc::downgrade(&first_use_host), first_use_thread.into())),
            ..Default::default()
        });
        let first_use_control = RecoveryControl::fixed(first_use_projection);
        let first_use_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&first_use_control));
        let first_use_managed = crate::ManagedHost::new(first_use_host.clone())
            .with_mcp_attachment_realizer(first_use_realizer.clone());
        first_use_managed.install_environment_binding_sink(first_use_sink.clone());
        let _managed = first_use_managed.install_dispatch_session_runtime();
        HostWorkerResolver::realize_session(
            &first_use_host,
            &first_use_control,
            first_use_thread,
            first_use_directive.clone(),
            None,
            Some(&first_use_activation.snapshot),
            false,
        )
        .await
        .expect("R7 first-use Environment installs publication before MCP staging");
        assert_eq!(first_use_sink.calls(), 1, "R7 one aggregate binding CAS");
        assert_eq!(
            first_use_realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R7 MCP stage observed the Store-read Resident owner"
        );
        assert!(
            first_use_host
                .session_slots
                .read(first_use_thread, |slot| slot
                    .environment_owner
                    .is_resident())
                .unwrap_or(false),
            "R7 Store-read identity remains Resident"
        );

        let unpinned_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let unpinned_realizer = Arc::new(RecordingMcpRealizer::default());
        let unpinned_managed = crate::ManagedHost::new(unpinned_host.clone())
            .with_mcp_attachment_realizer(unpinned_realizer.clone())
            .install_dispatch_session_runtime();
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &unpinned_managed,
            Arc::new(ToggleBindingSink {
                fail: std::sync::atomic::AtomicBool::new(false),
                calls: std::sync::atomic::AtomicUsize::new(0),
                binding: Mutex::new(None),
            }),
        );
        let error = HostWorkerResolver::realize_session(
            &unpinned_host,
            &RecoveryControl::fixed(frozen_projection()),
            first_use_thread,
            first_use_directive,
            None,
            None,
            false,
        )
        .await
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
                .session_environment(first_use_thread)
                .await
                .is_none(),
            "R8/E6: no unpinned Runtime may be installed"
        );

        let cold = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let error = HostWorkerResolver::realize_session(
            &cold, &control, thread, directive, None, None, false,
        )
        .await
        .expect_err("R3 cold lease-only recovery must fail closed");
        assert!(
            error.to_string().contains("exact claimed Agent snapshot"),
            "R3/E4: {error}"
        );

        let rebuild_thread = "cold-missing-environment";
        let rebuild_activation = test_activation(rebuild_thread, "run-cold-missing-environment");
        let rebuild_fixture_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let mut rebuild_frozen = frozen_projection();
        rebuild_frozen.agent_publication = Some(rebuild_activation.snapshot.clone());
        let missing =
            crate::host::worker_resolver::test_support::unavailable_local_environment_binding(
                &rebuild_fixture_host,
                rebuild_thread,
                &rebuild_frozen,
            )
            .await;
        let missing_handle: awaken_provisioning_contract::SandboxHandle =
            serde_json::from_str(&missing).expect("R5 exact unavailable handle");
        drop(rebuild_fixture_host);
        rebuild_frozen.environment = committed_resident_environment(rebuild_thread, missing);
        let rebuild_directive = awaken_session_contract::SessionRealizationDirective {
            projection: rebuild_frozen.clone(),
            lease: awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 2,
                expires_at_unix_ms: u64::MAX,
            },
            action: awaken_session_contract::SessionRealizationAction::Complete,
        };
        let rebuild_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let rebuild_managed =
            crate::ManagedHost::new(rebuild_host.clone()).install_dispatch_session_runtime();
        let rebuild_control = RecoveryControl::fixed(rebuild_frozen.clone());
        let rebuild_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&rebuild_control));
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &rebuild_managed,
            rebuild_sink,
        );
        HostWorkerResolver::realize_session(
            &rebuild_host,
            &rebuild_control,
            rebuild_thread,
            rebuild_directive,
            None,
            Some(&rebuild_activation.snapshot),
            true,
        )
        .await
        .expect("R5 rebuild policy replaces the unavailable Environment");
        assert_ne!(
            rebuild_host
                .session_environment_handle(rebuild_thread)
                .await
                .expect("R5 rebuilt Environment handle"),
            missing_handle,
            "R5/E5 rebuild publishes a new physical Environment"
        );

        let continuity_thread = "cold-continuity-missing";
        let continuity_activation =
            test_activation(continuity_thread, "run-cold-continuity-missing");
        let continuity_fixture_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let mut continuity_frozen = frozen_projection();
        continuity_frozen.agent_publication = Some(continuity_activation.snapshot.clone());
        let missing =
            crate::host::worker_resolver::test_support::unavailable_local_environment_binding(
                &continuity_fixture_host,
                continuity_thread,
                &continuity_frozen,
            )
            .await;
        drop(continuity_fixture_host);
        continuity_frozen.environment = committed_resident_environment(continuity_thread, missing);
        let continuity_directive = awaken_session_contract::SessionRealizationDirective {
            projection: continuity_frozen.clone(),
            lease: awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 3,
                expires_at_unix_ms: u64::MAX,
            },
            action: awaken_session_contract::SessionRealizationAction::Complete,
        };
        let continuity_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let continuity_managed =
            crate::ManagedHost::new(continuity_host.clone()).install_dispatch_session_runtime();
        let continuity_control = RecoveryControl::fixed(continuity_frozen.clone());
        let continuity_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&continuity_control));
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &continuity_managed,
            continuity_sink,
        );
        let error = HostWorkerResolver::realize_session(
            &continuity_host,
            &continuity_control,
            continuity_thread,
            continuity_directive,
            None,
            Some(&continuity_activation.snapshot),
            false,
        )
        .await
        .expect_err("R6 continuity policy rejects a missing Environment");
        assert!(
            error.to_string().contains("unavailable or terminal"),
            "R6 fails for exact physical unavailability, not fixture drift: {error}"
        );
        assert!(
            continuity_host
                .session_environment(continuity_thread)
                .await
                .is_none(),
            "R6 continuity failure publishes no replacement Environment"
        );
    }
}
