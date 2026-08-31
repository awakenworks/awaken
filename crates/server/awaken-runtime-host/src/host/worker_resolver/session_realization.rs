//! Worker-side projection and effect adapters for the canonical Session
//! realization driver.

use super::*;

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

struct WorkerProjectionSynchronizer<'a> {
    host: &'a SharedHost,
    claim: Option<&'a awaken_run_ingress::RunClaim>,
    published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
    rebuild_unavailable_environment: bool,
    requires_runtime_before_effects: bool,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for WorkerProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &awaken_session_contract::SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), awaken_session_contract::RunError> {
        let retained_publication = self
            .host
            .session_slots
            .read(session_id, |slot| slot.published_snapshot.clone())
            .flatten();
        if let Some(claimed) = self.published_snapshot
            && !matches!(
                awaken_session_contract::frozen_agent_publication_decision(
                    &projection.baseline,
                    Some(claimed),
                ),
                awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
                    | awaken_session_contract::FrozenAgentPublicationDecision::Exact
            )
        {
            return Err(awaken_session_contract::RunError::classified(
                "session_runtime_publication_conflict",
                "claimed Run does not match the frozen Session Agent publication",
            ));
        }
        let delivered_publication = projection
            .agent_publication
            .as_ref()
            .or(self.published_snapshot);
        let published_snapshot = delivered_publication.or(retained_publication.as_ref());
        match awaken_session_contract::frozen_agent_publication_decision(
            &projection.baseline,
            published_snapshot,
        ) {
            awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_publication_missing",
                    "Worker Session realization requires its exact frozen Agent publication",
                ));
            }
            awaken_session_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_publication_conflict",
                    "Worker Agent publication does not match the frozen Session identity, revision, or runtime",
                ));
            }
        }
        // A claim authorizes live Resource revalidation. A preparation Stage
        // authorizes local realization. Lease-only MCP renewal has neither and
        // must reuse the already-resident Resource/Skill projection instead of
        // opening an unclaimed remote materialization path.
        let synchronize_resources = self.claim.is_some() || prepare_session;
        // Control materializes current context on every directive. Install the
        // supplied projection even for a lease-only Complete action so a
        // Session command accepted between Runs cannot leave a warm slot stale.
        let mut projection = projection.clone();
        if projection.agent_publication.is_none() {
            projection.agent_publication = published_snapshot.cloned();
        }
        self.host
            .install_frozen_session_projection(
                session_id,
                projection.clone(),
                self.claim,
                synchronize_resources,
                Some(lease.clone()),
            )
            .await
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        let environment_absent = self.host.session_environment(session_id).await.is_none();
        let has_environment_binding =
            environment_absent && projection.environment.binding().is_some();
        let runtime_authority_resident = self
            .host
            .session_slots
            .read(session_id, |slot| {
                slot.runtime.is_some() || slot.environment_owner.is_resident()
            })
            .unwrap_or(false);
        if self.requires_runtime_before_effects
            && published_snapshot.is_none()
            && !runtime_authority_resident
        {
            return Err(awaken_session_contract::RunError::classified(
                "session_runtime_publication_missing",
                "sandbox stdio MCP realization requires the exact claimed Agent snapshot before effects",
            ));
        }
        let (adopted, rebuild_binding) = if environment_absent
            && let Some(binding) = projection.environment.binding()
        {
            let published_snapshot = published_snapshot.ok_or_else(|| {
                awaken_session_contract::RunError::classified(
                    "session_environment_recovery_authority_missing",
                    "a cold Worker needs the exact claimed Agent snapshot to adopt a durable Session Environment",
                )
            })?;
            self.host
                .adopt_bound_session_environment(
                    session_id,
                    Some(binding),
                    published_snapshot
                        .resolved_spec
                        .model_binding
                        .provisioning(),
                    self.rebuild_unavailable_environment,
                )
                .await
                .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?
        } else {
            (None, false)
        };
        if rebuild_binding {
            // The only admissible replacement path is the claim-bound recovery
            // decision above. Clear the process-local expectation only after the
            // provider has proved the exact durable binding unavailable; the new
            // Environment receipt must still pass the ordinary durable sink.
            debug_assert!(
                self.host
                    .durable_session_environment_binding(session_id)
                    .is_none(),
                "terminated recovery owner clears its durable binding"
            );
        }
        // Synchronization is the single ordering boundary between Control's
        // frozen projection and MCP effects. A first-use Environment has no
        // durable binding to adopt yet, but its stage still needs the exact Run
        // publication installed before it may realize sandbox stdio. Only the
        // physical substrate is needed here; parent Agent plugins remain owned
        // by the exact root attempt and must not be constructed as a side effect.
        if published_snapshot.is_some()
            && (has_environment_binding || self.requires_runtime_before_effects)
        {
            self.host
                .session_child_execution_substrate(session_id, adopted, published_snapshot)
                .await
                .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        }
        Ok(())
    }
}

pub(super) struct WorkerMcpEffects<'a>(pub(super) &'a SharedHost);

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
    {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.drain_dispatched_mcp(generation).await
    }
}

impl HostWorkerResolver {
    /// Install one terminal recovery assignment through the exact same frozen
    /// projection synchronizer used by claimed Runs and lease renewal. The
    /// assignment carries no cleanup commands; after this returns the Host polls
    /// the aggregate-owned command projection through Session Control.
    pub(crate) async fn install_terminal_cleanup_assignment(
        host: &SharedHost,
        assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
    ) -> Result<(), awaken_session_contract::RunError> {
        let realization = host.session_slots.realization_lock(&assignment.session_id);
        let _realization = realization.lock().await;
        let mut teardown_projection = assignment.projection.clone();
        // Terminal recovery has root cleanup authority, never a Run claim. It
        // installs the exact frozen baseline/publication/environment needed to
        // dispose the process-local realization without revalidating or
        // rematerializing the active Resource generation being destroyed.
        // Resource lifecycle remains owned by the Session aggregate and its
        // catalog receipts after Runtime cleanup completes.
        teardown_projection.resources = Default::default();
        awaken_session_contract::SessionProjectionSynchronizer::synchronize_session_projection(
            &WorkerProjectionSynchronizer {
                host,
                claim: None,
                published_snapshot: None,
                rebuild_unavailable_environment: false,
                requires_runtime_before_effects: false,
            },
            &assignment.session_id,
            &teardown_projection,
            &assignment.lease,
            true,
        )
        .await
    }

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
        Self::drive_session_realization(
            host,
            control,
            session_id,
            directive,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
        )
        .await
    }

    /// Drive one already-serialized directive. Callers that coordinate a lease
    /// renewal acquire the Session's realization lock before entering here so a
    /// heartbeat never creates a parallel phase driver.
    pub(crate) async fn drive_session_realization(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
    ) -> Result<(), awaken_run_ingress::Error> {
        Self::drive_session_realization_raw(
            host,
            control,
            session_id,
            directive,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
        )
        .await
        .map_err(Self::map_session_realization_drive_error)
    }

    /// Execute the canonical realization driver without erasing its typed
    /// Control failure. Ordinary claimed Runs use the mapped wrapper above;
    /// lease renewal needs the exact `Conflict` variant so it can refresh one
    /// concurrently advanced aggregate instead of revoking a still-owned
    /// process-local projection.
    pub(crate) async fn drive_session_realization_raw(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
    ) -> Result<(), awaken_session_contract::SessionRealizationDriveError> {
        let requires_runtime_before_effects = matches!(
            &directive.action,
            awaken_session_contract::SessionRealizationAction::Stage { mcp_stages, .. }
                if mcp_stages
                    .iter()
                    .any(|stage| stage.target.sandbox_stdio_target().is_some())
        );
        awaken_session_contract::drive_session_realization(
            session_id,
            claim.map(|claim| claim.run_id.clone()),
            control,
            &WorkerProjectionSynchronizer {
                host,
                claim,
                published_snapshot,
                rebuild_unavailable_environment,
                requires_runtime_before_effects,
            },
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
        AdoptionModel, claim, committed_environment, deferred_environment, test_activation,
    };
    use awaken_session_contract::SessionRuntime as _;
    use std::sync::{Arc, Mutex};

    /// C1-C3: a cold Worker must derive eager-vs-deferred provisioning only from
    /// the immutable dispatch envelope. Legacy absence remains eager, an exact
    /// on-tool-use projection stays sandbox-free during Brain resolution, and a
    /// malformed projection is rejected by claim admission before any Worker or
    /// Sandbox effect. Moving C3 earlier preserves fail-closed behavior while
    /// keeping one projection decoder at the run-ingress contract boundary.
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
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let legacy = claim(&store, "cold-legacy", "run-legacy", "worker-a", now).await;
        resolver
            .worker_for_claimed(&legacy)
            .await
            .expect("C1 legacy projection remains eager");
        assert!(
            host.session_environment("cold-legacy").await.is_some(),
            "C1"
        );

        let runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
            deferred_environment(),
            Some(Default::default()),
            Vec::new(),
        )
        .expect("encode runtime projection");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-deferred",
                    "run-deferred",
                ))
                .with_session_runtime(runtime),
            )
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

    struct RecoveryControl {
        projection: Arc<Mutex<awaken_session_contract::FrozenSessionProjection>>,
    }

    impl RecoveryControl {
        fn fixed(projection: awaken_session_contract::FrozenSessionProjection) -> Self {
            Self {
                projection: Arc::new(Mutex::new(projection)),
            }
        }

        fn current_projection(&self) -> awaken_session_contract::FrozenSessionProjection {
            self.projection.lock().unwrap().clone()
        }
    }

    struct RecoveryEnvironmentBindingSink {
        projection: Arc<Mutex<awaken_session_contract::FrozenSessionProjection>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RecoveryEnvironmentBindingSink {
        fn new(control: &RecoveryControl) -> Self {
            Self {
                projection: control.projection.clone(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionEnvironmentBindingSink for RecoveryEnvironmentBindingSink {
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
        begin_calls: std::sync::atomic::AtomicUsize,
        acknowledge_conflicts_remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RenewalDuringStageControl {
        async fn begin_session_realization(
            &self,
            command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.begin_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut lease = self.lease.lock().unwrap();
            lease.expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
            let mut stage = self.stage.clone();
            stage.generation.lease_expires_at_unix_ms = lease.expires_at_unix_ms;
            stage.stage_idempotency_key = format!("renew:{}", lease.expires_at_unix_ms);
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.projection.clone(),
                lease: lease.clone(),
                action: awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            })
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
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
            if self
                .acknowledge_conflicts_remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                return Err(awaken_session_contract::SessionRealizationControlFailure::Conflict);
            }
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

    fn frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
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
        let _managed = crate::ManagedHost::new(host.clone());
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
    /// exact generation up before completion; E4 a concurrent aggregate CAS
    /// refreshes through the same driver while retaining the resident projection;
    /// E5 repeated conflict exhausts the bound and revokes the unprovable local
    /// projection. Replacement fencing is owned by the contract authorization
    /// table; ordinary idle renewal is W6 in the parent resolver tests.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | R1 | yes | yes | yes | E1 + E2 + E3 |
    /// | R2 | no | no | yes | canonical renewal driver (W6) |
    /// | R3 | any | any | replacement | fence/revoke (authorization A2/W7) |
    /// | R4 | no | no | same owner plus one Control conflict | refresh once; E2 + E4 |
    /// | R5 | no | no | repeated Control conflict | bounded failure; E5 |
    #[tokio::test]
    async fn heartbeat_renewal_does_not_duplicate_an_in_flight_realization_driver() {
        let thread = "renew-during-stage";
        let initial_expiry = 1_000;
        let renewed_expiry = 2_000;
        let mut projection = frozen_projection();
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: initial_expiry,
        };
        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: initial_expiry,
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
        let control = Arc::new(RenewalDuringStageControl {
            projection: projection.clone(),
            stage: stage.clone(),
            lease: Mutex::new(lease.clone()),
            begin_calls: std::sync::atomic::AtomicUsize::new(0),
            acknowledge_conflicts_remaining: std::sync::atomic::AtomicUsize::new(0),
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
                .begin_calls
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

        let conflict_expiry = renewed_expiry + 1_000;
        control
            .acknowledge_conflicts_remaining
            .store(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            host.renew_due_session_realizations(
                renewed_expiry,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(
                    conflict_expiry - renewed_expiry,
                ),
            )
            .await
            .expect("R4 concurrent aggregate change refreshes"),
            1,
            "R4/E4"
        );
        assert_eq!(
            control
                .begin_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "R4 initial attempt plus one bounded refresh"
        );
        assert!(
            host.session_slots
                .read(thread, |slot| {
                    slot.baseline.is_some()
                        && slot
                            .realization_lease
                            .as_ref()
                            .is_some_and(|lease| lease.expires_at_unix_ms == conflict_expiry)
                })
                .unwrap_or(false),
            "R4/E2/E4 keeps one complete resident projection"
        );

        let exhausted_expiry = conflict_expiry + 1_000;
        control
            .acknowledge_conflicts_remaining
            .store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            host.renew_due_session_realizations(
                conflict_expiry,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(
                    exhausted_expiry - conflict_expiry,
                ),
            )
            .await
            .expect("R5 renewal scan isolates the failed projection"),
            0,
            "R5/E5"
        );
        assert_eq!(
            control
                .begin_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            5,
            "R5 stops after the bounded refresh"
        );
        assert!(!host.session_slots.contains(thread), "R5/E5");
    }

    #[tokio::test]
    async fn cold_worker_adopts_the_frozen_environment_before_recovering_stdio_mcp() {
        /* Cold remote-Session recovery cause/effect graph: C1 Control returns a
         * frozen Session with an opaque resident-Environment binding; C2 the new
         * Worker process has no resident Runtime/Environment; C3 the claimed Run
         * carries the exact immutable Agent snapshot; C4 Control requires the
         * active sandbox-stdio MCP generation to be restaged; C5 a rebuild-mode
         * Run names a provider-valid typed durable Environment whose physical root
         * is no longer available; C6 the first-use Managed binding sink commits and
         * reads back Resident. Effects:
         * E1 adopt the exact bound Environment before MCP stage; E2 never consult
         * current Agent publication; E3 preserve the Sandbox handle and generation;
         * E4 a cold lease-only replay without the exact snapshot fails closed; E5
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
        let original =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let original_ctx = original
            .ctx_for_snapshot_with_sandbox(
                thread,
                Some("agent-a"),
                Some(activation.snapshot.clone()),
                None,
            )
            .await
            .expect("R1 original Environment");
        let handle = original_ctx
            .env
            .as_ref()
            .expect("R1 eager Environment")
            .handle();
        let binding = serde_json::to_string(&handle).expect("R1 durable binding");
        drop(original_ctx);
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
        let mut frozen = frozen_projection();
        frozen.environment.set_resident(binding);
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
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
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
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
        HostWorkerResolver::realize_session(
            &host,
            &control,
            thread,
            directive.clone(),
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
        let _managed = crate::ManagedHost::new(unpinned_host.clone())
            .with_mcp_attachment_realizer(unpinned_realizer.clone())
            .install_dispatch_session_runtime();
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
        let unavailable_local_handle = |thread: &str| {
            awaken_provisioning_contract::SandboxHandle::local(
                thread,
                handle
                    .local_payload()
                    .expect("R5/R6 original typed local payload")
                    .clone(),
            )
        };
        let missing = unavailable_local_handle(rebuild_thread);
        let mut rebuild_frozen = frozen_projection();
        rebuild_frozen
            .environment
            .set_resident(serde_json::to_string(&missing).expect("R5 missing durable binding"));
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
        let _managed =
            crate::ManagedHost::new(rebuild_host.clone()).install_dispatch_session_runtime();
        HostWorkerResolver::realize_session(
            &rebuild_host,
            &RecoveryControl::fixed(rebuild_frozen),
            rebuild_thread,
            rebuild_directive,
            None,
            Some(&rebuild_activation.snapshot),
            true,
        )
        .await
        .expect("R5 rebuild policy replaces the unavailable Environment");
        assert!(
            rebuild_host
                .session_environment(rebuild_thread)
                .await
                .is_some(),
            "R5/E5"
        );

        let continuity_thread = "cold-continuity-missing";
        let continuity_activation =
            test_activation(continuity_thread, "run-cold-continuity-missing");
        let mut continuity_frozen = frozen_projection();
        continuity_frozen.environment.set_resident(
            serde_json::to_string(&unavailable_local_handle(continuity_thread))
                .expect("R6 missing durable binding"),
        );
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
        let _managed =
            crate::ManagedHost::new(continuity_host.clone()).install_dispatch_session_runtime();
        HostWorkerResolver::realize_session(
            &continuity_host,
            &RecoveryControl::fixed(continuity_frozen),
            continuity_thread,
            continuity_directive,
            None,
            Some(&continuity_activation.snapshot),
            false,
        )
        .await
        .expect_err("R6 continuity policy rejects a missing Environment");
    }
}
