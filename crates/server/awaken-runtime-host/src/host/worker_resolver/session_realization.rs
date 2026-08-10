//! Worker-side projection and effect adapters for the canonical Session
//! realization driver.

use super::*;

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
        let resolved_publication;
        let retained_publication = self
            .host
            .session_slots
            .read(session_id, |slot| slot.published_snapshot.clone())
            .flatten();
        let published_snapshot = match self.published_snapshot {
            Some(snapshot) => Some(snapshot),
            None if retained_publication.is_some() => retained_publication.as_ref(),
            None => {
                resolved_publication = self
                    .host
                    .resolve_session_publication(
                        session_id,
                        Some(&projection.baseline.agent_id),
                        None,
                    )
                    .map_err(|error| {
                        awaken_session_contract::RunError::internal(error.to_string())
                    })?
                    .2;
                resolved_publication.as_ref()
            }
        };
        if let Some(snapshot) = published_snapshot {
            let conflict = self.host.session_slots.update(session_id, |slot| {
                if slot
                    .published_snapshot
                    .as_ref()
                    .is_some_and(|retained| retained != snapshot)
                {
                    true
                } else {
                    slot.published_snapshot = Some(snapshot.clone());
                    false
                }
            });
            if conflict {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_publication_conflict",
                    "Session realization cannot replace its immutable Agent publication",
                ));
            }
        }
        // A claim authorizes live Resource revalidation. A preparation Stage
        // authorizes local realization. Lease-only MCP renewal has neither and
        // must reuse the already-resident Resource/Skill projection instead of
        // opening an unclaimed remote materialization path.
        let synchronize_resources = self.claim.is_some() || prepare_session;
        self.host
            .install_frozen_session_projection(
                session_id,
                projection.clone(),
                self.claim,
                synchronize_resources,
            )
            .await
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        self.host
            .install_session_realization_lease(session_id, lease.clone());
        let environment_absent = self.host.session_environment(session_id).await.is_none();
        let has_environment_binding =
            environment_absent && projection.environment.binding().is_some();
        let runtime_authority_resident = self
            .host
            .session_slots
            .read(session_id, |slot| {
                slot.runtime.is_some() || slot.environment.is_some()
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
                    &published_snapshot.resolved_spec.model_binding.provisioning,
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
            self.host
                .install_expected_environment_binding(session_id, None)
                .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        }
        // Synchronization is the single ordering boundary between Control's
        // frozen projection and MCP effects. A first-use Environment has no
        // durable binding to adopt yet, but its stage still needs the exact Run
        // publication installed before it may realize sandbox stdio.
        if let Some(published_snapshot) = published_snapshot
            && (has_environment_binding || self.requires_runtime_before_effects)
        {
            self.host
                .ctx_for_snapshot_with_sandbox(
                    session_id,
                    Some(&projection.baseline.agent_id),
                    Some(published_snapshot.clone()),
                    adopted,
                )
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
    pub(crate) async fn realize_application_session(
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
        Self::drive_application_session(
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
    pub(crate) async fn drive_application_session(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
    ) -> Result<(), awaken_run_ingress::Error> {
        let requires_runtime_before_effects = matches!(
            &directive.action,
            awaken_session_contract::SessionRealizationAction::Stage { mcp_stages, .. }
                if mcp_stages
                    .iter()
                    .any(|stage| stage.target.sandbox_stdio_target().is_some())
        );
        awaken_session_contract::drive_session_realization(
            session_id,
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
        // The canonical driver has already delivered `fail_session_realization`
        // before returning an error, so this is the narrow absorbing failure
        // class that the Run claim may terminalize immediately. Environment
        // adoption and other resolver failures remain ordinary retryable errors.
        .map_err(|error| Self::terminal_resolution_error(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{AdoptionModel, test_activation};
    use std::sync::{Arc, Mutex};

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
                            .read(thread, |slot| slot.environment.is_some())
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
        projection: awaken_session_contract::FrozenSessionProjection,
    }

    struct RenewalDuringStageControl {
        projection: awaken_session_contract::FrozenSessionProjection,
        stage: awaken_session_contract::StageMcpAttachment,
        lease: Mutex<awaken_session_contract::SessionRealizationLease>,
        begin_calls: std::sync::atomic::AtomicUsize,
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

        async fn contribute(
            &self,
            _claim: &awaken_run_ingress::RunClaim,
            _contribution: awaken_session_contract::ApplicationSessionContribution,
        ) -> Result<
            awaken_run_ingress_contract::ClaimedSessionContributionReceipt,
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
                projection: self.projection.clone(),
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
                projection: self.projection.clone(),
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
                    sandbox: serde_json::json!({}),
                    sandbox_provisioning: Default::default(),
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
                model: "model".into(),
                runtime: None,
                application: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            toolsets: Vec::new(),
            mcp: Vec::new(),
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
        let _managed = crate::ManagedHost::new(host.clone());
        let resident = frozen_projection();
        host.install_frozen_session_projection(thread, resident.clone(), None, true)
            .await
            .expect("M1 establish resident baseline");
        let mut remote = resident;
        remote.resource_revision = 1;
        remote.resources.skills = Some(vec![awaken_session_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "design".into(),
            version: 36,
            bundle_sha256: "sha256-design-v36".into(),
        }]);
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

    /// Dynamic-publication cause/effect graph: C1 initial claimed snapshot is
    /// authoritative; C2 renewal omits a snapshot; C3 a later caller supplies a
    /// different snapshot. E1 retain the whole immutable snapshot in the slot;
    /// E2 reuse it on lease-only renewal; E3 reject replacement and preserve E1.
    /// Decision rules: P1 C1=>E1, P2 E1+C2=>E2, P3 E1+C3=>E3.
    #[tokio::test]
    async fn lease_only_renewal_reuses_a_dynamic_claimed_publication() {
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "dynamic-publication-renewal";
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone());
        let mut projection = frozen_projection();
        projection.baseline.runtime = Some("acp:claude".into());
        let mut snapshot = test_activation(thread, "dynamic-publication").snapshot;
        snapshot.resolved_spec.model_binding.backend_ref = "acp:claude".into();
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: Some(&snapshot),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
        }
        .synchronize_session_projection(thread, &projection, &lease, true)
        .await
        .expect("initial claimed publication is retained");

        WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: None,
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
        }
        .synchronize_session_projection(thread, &projection, &lease, false)
        .await
        .expect("lease renewal reuses the retained immutable publication");
        let retained = host
            .session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten();
        assert_eq!(retained.as_ref(), Some(&snapshot), "P1/P2");

        let mut replacement = snapshot.clone();
        replacement.resolved_spec.model_binding.backend_ref = "acp:replacement".into();
        let error = WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: None,
            published_snapshot: Some(&replacement),
            rebuild_unavailable_environment: false,
            requires_runtime_before_effects: false,
        }
        .synchronize_session_projection(thread, &projection, &lease, false)
        .await
        .expect_err("P3 immutable publication replacement is rejected");
        assert_eq!(error.code, "session_runtime_publication_conflict", "P3");
        assert_eq!(
            host.session_slots
                .read(thread, |slot| slot.published_snapshot.clone())
                .flatten()
                .as_ref(),
            Some(&snapshot),
            "P3"
        );
    }

    /// Concurrent-renewal cause/effect graph: C1 an initial phase driver owns
    /// the Session realization lock; C2 its MCP Stage is still pending; C3 a
    /// heartbeat requests the same owner/incarnation/epoch lease extension.
    /// Effects: E1 extend durable and local authority without blocking; E2 never
    /// start a second Stage/Publish driver; E3 the current driver catches its
    /// exact generation up before completion. Replacement
    /// fencing is owned by the contract authorization table; ordinary idle
    /// renewal is W6 in the parent resolver tests.
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
        });
        let stage_entered = Arc::new(tokio::sync::Notify::new());
        let release_stage = Arc::new(tokio::sync::Notify::new());
        let realizer = Arc::new(RecordingMcpRealizer {
            stage_entered: Some(stage_entered.clone()),
            release_stage: Some(release_stage.clone()),
            ..Default::default()
        });
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_application_session_control(control.clone()),
        );
        let managed =
            crate::ManagedHost::new(host.clone()).with_mcp_attachment_realizer(realizer.clone());
        drop(managed);
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
            HostWorkerResolver::realize_application_session(
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
                host.renew_due_session_realizations(initial_expiry, renewed_expiry),
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
    }

    #[tokio::test]
    async fn cold_worker_adopts_the_frozen_environment_before_recovering_stdio_mcp() {
        /* Cold remote-Session recovery cause/effect graph: C1 Control returns a
         * frozen Session with an opaque resident-Environment binding; C2 the new
         * Worker process has no resident Runtime/Environment; C3 the claimed Run
         * carries the exact immutable Agent snapshot; C4 Control requires the
         * active sandbox-stdio MCP generation to be restaged; C5 a rebuild-mode
         * Run names a durable Environment that is no longer available. Effects: E1 adopt
         * the exact bound Environment before MCP stage; E2 never consult current
         * Agent publication; E3 preserve the Sandbox handle and generation; E4 a
         * cold lease-only replay without the exact snapshot fails closed; E5
         * rebuild only when the claimed Run's explicit recovery policy permits it;
         * E6 no MCP effect runs when a first-use stdio stage lacks that snapshot.
         *
         * | Rule | binding | resident | snapshot | stdio | recovery | Effect |
         * |---|---|---|---|---|---|---|
         * | R1 | ready | no | yes | yes | either | E1 + E2 + E3 |
         * | R2 | ready | yes | no | yes | either | reuse resident Environment |
         * | R3 | ready | no | no | yes | either | E4 |
         * | R4 | no | no | yes | no | either | ordinary resume (covered by O1) |
         * | R5 | missing | no | yes | no | rebuild | E5 |
         * | R6 | missing | no | yes | no | continuity | fail closed |
         * | R7 | none yet | no | yes | yes | either | install publication, then stage |
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
        let control = RecoveryControl {
            projection: frozen.clone(),
        };
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let realizer = Arc::new(RecordingMcpRealizer {
            required_environment: Some((Arc::downgrade(&host), thread.into())),
            ..Default::default()
        });
        let managed =
            crate::ManagedHost::new(host.clone()).with_mcp_attachment_realizer(realizer.clone());
        drop(managed);
        HostWorkerResolver::realize_application_session(
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
                    && slot.environment.is_some())
                .unwrap_or(false),
            "R2 fixture is the publish-to-final-resolve gap"
        );
        HostWorkerResolver::realize_application_session(
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
        let managed = crate::ManagedHost::new(first_use_host.clone())
            .with_mcp_attachment_realizer(first_use_realizer.clone());
        drop(managed);
        HostWorkerResolver::realize_application_session(
            &first_use_host,
            &RecoveryControl {
                projection: first_use_projection,
            },
            first_use_thread,
            first_use_directive.clone(),
            None,
            Some(&first_use_activation.snapshot),
            false,
        )
        .await
        .expect("R7 first-use Environment installs publication before MCP staging");
        assert_eq!(
            first_use_realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R7"
        );

        let unpinned_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let unpinned_realizer = Arc::new(RecordingMcpRealizer::default());
        let managed = crate::ManagedHost::new(unpinned_host.clone())
            .with_mcp_attachment_realizer(unpinned_realizer.clone());
        drop(managed);
        let error = HostWorkerResolver::realize_application_session(
            &unpinned_host,
            &RecoveryControl {
                projection: frozen_projection(),
            },
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
        let error = HostWorkerResolver::realize_application_session(
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
        let missing =
            awaken_provisioning_contract::SandboxHandle::new(handle.provider_kind, rebuild_thread);
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
        let managed = crate::ManagedHost::new(rebuild_host.clone());
        drop(managed);
        HostWorkerResolver::realize_application_session(
            &rebuild_host,
            &RecoveryControl {
                projection: rebuild_frozen,
            },
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
            serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
                missing.provider_kind,
                continuity_thread,
            ))
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
        let managed = crate::ManagedHost::new(continuity_host.clone());
        drop(managed);
        HostWorkerResolver::realize_application_session(
            &continuity_host,
            &RecoveryControl {
                projection: continuity_frozen,
            },
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
