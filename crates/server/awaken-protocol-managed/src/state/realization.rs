//! Managed wire-cache coordination around the Session application realization owner.

use super::*;
use awaken_session_contract::{
    BeginSessionRealization, McpAttachmentState, PersistedSession, RunError,
    SessionRealizationControl, SessionRealizationControlFailure, SessionRealizationLease,
    SessionRuntime,
};

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

struct LocalProjectionSynchronizer<'a> {
    runtime: &'a dyn SessionRuntime,
    environment_binding: Option<&'a str>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for LocalProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError> {
        if !prepare_session {
            return Ok(());
        }
        self.runtime
            .install_session_realization_lease(session_id, lease.clone());
        self.runtime
            .prepare_session(session_id, projection.session_init())
            .await?;
        if let Some(binding) = self.environment_binding {
            self.runtime
                .restore_session_environment(&projection.baseline.agent_id, session_id, binding)
                .await?;
        }
        Ok(())
    }
}

impl ManagedState {
    /// Install the frozen facts required to construct a durable Run dispatch,
    /// without acquiring the Session realization lease. In a Worker-only
    /// placement the physical projection is owned exclusively by the claim-time
    /// realization driver.
    pub(super) async fn install_dispatch_projection(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), StateError> {
        let projection = awaken_session_application::SessionApplication::frozen_session_projection(
            owner_scope.to_string(),
            session,
        )
        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        self.application
            .runtime()
            .prepare_session(&session.session_id, projection.session_init())
            .await
            .map_err(StateError::Run)
    }
}

impl ManagedState {
    fn map_realization_failure(error: SessionRealizationControlFailure) -> StateError {
        match error {
            SessionRealizationControlFailure::NotFound => StateError::NotFound,
            SessionRealizationControlFailure::Conflict => StateError::Conflict,
            SessionRealizationControlFailure::Invalid(message) => {
                StateError::Run(RunError::bad_request(message))
            }
            error => StateError::Run(RunError::internal(error.to_string())),
        }
    }

    /// Sole in-process topology adapter for the Control-owned realization
    /// protocol. Create, hot replacement, and recovery all call this driver;
    /// it owns Runtime I/O but never mutates Session desired state directly.
    pub async fn realize_application_session(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, StateError> {
        let lease_expires_at_unix_ms = now_unix_ms()
            .checked_add(300_000)
            .ok_or_else(|| StateError::Run(RunError::internal("lease expiry overflow")))?;
        let directive = self
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: session_id.to_string(),
                target: awaken_session_contract::SessionRealizationTarget {
                    // A process incarnation is the physical local owner. A
                    // deployment-wide constant would let two active replicas
                    // immediately fence each other while both sandboxes live.
                    owner: self.application.runtime_incarnation().to_string(),
                    runtime_incarnation: self.application.runtime_incarnation().to_string(),
                    lease_expires_at_unix_ms,
                    renew_existing_lease: false,
                },
            })
            .await
            .map_err(Self::map_realization_failure)?;
        self.drive_local_realization(session_id, directive).await?;
        self.application
            .session_repository()
            .get(session_id)
            .await
            .ok_or(StateError::NotFound)
    }

    pub(super) async fn realize_session_locally(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, StateError> {
        self.realize_application_session(session_id).await
    }

    async fn drive_local_realization(
        &self,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
    ) -> Result<(), StateError> {
        let environment_binding = self
            .application
            .session_repository()
            .get(session_id)
            .await
            .and_then(|session| session.environment.binding().map(str::to_string));
        awaken_session_contract::drive_session_realization(
            session_id,
            self.application.as_ref(),
            &LocalProjectionSynchronizer {
                runtime: self.application.runtime(),
                environment_binding: environment_binding.as_deref(),
            },
            self.application.mcp_realizer(),
            directive,
        )
        .await
        .map_err(|error| match error {
            awaken_session_contract::SessionRealizationDriveError::Effect(error) => {
                StateError::Run(error)
            }
            awaken_session_contract::SessionRealizationDriveError::Control(error) => {
                Self::map_realization_failure(error)
            }
            awaken_session_contract::SessionRealizationDriveError::DidNotConverge => {
                StateError::Run(RunError::internal(error.to_string()))
            }
        })
    }

    /// Renew due local projections through the same root-CAS phase protocol.
    /// Composition roots call this periodically; it owns no second registry or
    /// relay-specific timer.
    pub async fn renew_due_session_realizations(
        &self,
        now_unix_ms: u64,
    ) -> Result<usize, StateError> {
        const RENEW_BEFORE_MS: u64 = 150_000;
        const LEASE_MS: u64 = 300_000;
        let renew_before = now_unix_ms.saturating_add(RENEW_BEFORE_MS);
        let requested_expiry = now_unix_ms.saturating_add(LEASE_MS);
        let sessions = self
            .application
            .session_repository()
            .reconcilable_sessions()
            .await;
        let mut renewed = 0;
        for scoped in sessions {
            let Some(lease) = scoped.session.realization.clone() else {
                continue;
            };
            if scoped.session.status == "deleted"
                || lease.owner != self.application.runtime_incarnation()
                || lease.runtime_incarnation != self.application.runtime_incarnation()
                || lease.expires_at_unix_ms > renew_before
                || !scoped
                    .session
                    .mcp
                    .attachments
                    .iter()
                    .any(|attachment| attachment.state == McpAttachmentState::Active)
            {
                continue;
            }
            let directive = self
                .application
                .begin_session_realization(BeginSessionRealization {
                    session_id: scoped.session.session_id.clone(),
                    target: awaken_session_contract::SessionRealizationTarget {
                        owner: lease.owner,
                        runtime_incarnation: lease.runtime_incarnation,
                        lease_expires_at_unix_ms: requested_expiry,
                        renew_existing_lease: true,
                    },
                })
                .await
                .map_err(Self::map_realization_failure)?;
            self.drive_local_realization(&scoped.session.session_id, directive)
                .await?;
            renewed += 1;
        }
        Ok(renewed)
    }

    async fn reconcile_pending_session_state(&self) {
        let reconciled_resources = self.reconcile_resource_activations().await;
        if reconciled_resources > 0 {
            tracing::info!(
                reconciled_resources,
                "reconciled durable Session resource activations"
            );
        }
        let reconciled_mcp = self.reconcile_mcp_attachments().await;
        if reconciled_mcp > 0 {
            tracing::info!(reconciled_mcp, "reconciled durable Session MCP projections");
        }
    }

    /// Start the one local Session lifecycle supervisor when a Tokio runtime is
    /// available. Recovery runs immediately in a non-overlapping background task,
    /// so a slow external sandbox cannot hold the HTTP readiness boundary. The
    /// same timer retries completed recovery passes and drives lease/residency;
    /// repeated MCP- or resource-specific supervisors are forbidden.
    #[must_use]
    pub fn spawn_realization_lease_supervisor(
        self: &Arc<Self>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        if !self.application.claim_lifecycle_supervisor() {
            return None;
        }
        let state = self.clone();
        Some(runtime.spawn(async move {
            let recovery_state = state.clone();
            let mut recovery = tokio::spawn(async move {
                recovery_state.reconcile_pending_session_state().await;
            });
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            state.reconcile_work_dispatches().await;
            loop {
                interval.tick().await;
                if recovery.is_finished() {
                    if let Err(error) = recovery.await {
                        tracing::warn!(
                            error = ?error,
                            "Session state reconciliation task failed"
                        );
                    }
                    let recovery_state = state.clone();
                    recovery = tokio::spawn(async move {
                        recovery_state.reconcile_pending_session_state().await;
                    });
                }
                let now = now_unix_ms();
                if let Err(error) = state.renew_due_session_realizations(now).await {
                    tracing::warn!(
                        error = ?error,
                        "Session realization lease renewal remains pending"
                    );
                }
                state.reconcile_work_dispatches().await;
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_credential_contract::{
        CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::{
        AcknowledgeSessionRealization, ActivateSessionRealization, ApplicationContributionReceipt,
        EnvironmentFingerprint, EnvironmentSnapshot, FailSessionRealization, IdempotencyRecord,
        McpAttachmentDraft, McpAttachmentOrigin, McpGenerationRef, McpRealizationReceipt,
        McpTarget, OutcomeReport, SessionBaseline, SessionBaselineInputs, SessionBaselineState,
        SessionMcpAttachmentSet, SessionNetworkPolicy, SessionRealizationAction,
        SessionResourceState, SessionRevision, SessionRuntime, StageMcpAttachment, StepOutcome,
        ToolPermissionDecision,
    };

    struct NoopRuntime;

    #[async_trait::async_trait]
    impl SessionRuntime for NoopRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            unreachable!()
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for NoopRuntime {
        async fn stage_mcp_attachment(
            &self,
            request: StageMcpAttachment,
        ) -> Result<McpRealizationReceipt, RunError> {
            Ok(McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }
    }

    fn holder() -> PlaintextHolder {
        PlaintextHolder::new(PlaintextBoundary::Worker, "test.worker")
    }

    fn persisted_session(id: &str) -> PersistedSession {
        let application_input = awaken_session_contract::ApplicationSessionInput::default();
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment: EnvironmentSnapshot {
                environment_id: "env".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: EnvironmentFingerprint("env-fingerprint".into()),
                sandbox: serde_json::json!({}),
                sandbox_provisioning: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: SessionNetworkPolicy::Unrestricted,
                credential_realization: CredentialRealizationProfile {
                    inference_holder: holder(),
                    mcp_holder: holder(),
                    resource_holder: holder(),
                },
            },
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            model: "model".into(),
            runtime: None,
            application: Some(ApplicationContributionReceipt::from_input(
                "application-plan".into(),
                &application_input,
            )),
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
        });
        let mut resources = SessionResourceState::default();
        resources
            .prepare(id, Default::default())
            .expect("prepare generation 1 resources");
        let mcp = SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "docs".into(),
                target: McpTarget::parse_http("https://mcp.example.test").unwrap(),
                credential: None,
                prompts_as_skills: false,
                origin: McpAttachmentOrigin::Application,
            }],
            Some(holder()),
        )
        .unwrap();
        PersistedSession {
            session_id: id.into(),
            revision: SessionRevision(0),
            baseline: SessionBaselineState::Frozen(baseline),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp,
            resources,
            realization: None,
            status: "preparing".into(),
            archived_at: None,
        }
    }

    async fn harness_for(
        session: PersistedSession,
    ) -> (ManagedState, Arc<dyn ManagedSessionRepository>) {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let id = session.session_id.clone();
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        repo.create(
            "workspace-a",
            session,
            IdempotencyRecord {
                key: format!("test:create:{id}"),
                payload_hash,
            },
            Vec::new(),
        )
        .await
        .unwrap();
        (
            ManagedState::new_with_mcp(NoopRuntime).with_session_repo(repo.clone()),
            repo,
        )
    }

    async fn harness(id: &str) -> (ManagedState, Arc<dyn ManagedSessionRepository>) {
        harness_for(persisted_session(id)).await
    }

    #[tokio::test]
    async fn lifecycle_supervisor_is_single_owner_and_starts_recovery_off_path() {
        // Cause/effect decision table: R1 Tokio runtime + first start -> return
        // one background supervisor immediately; R2 the same ManagedState starts
        // again -> return None and create no overlapping recovery/timer; R3 no
        // runtime -> existing contract returns None. Slow recovery is owned by the
        // spawned child, so component construction never awaits sandbox I/O.
        let state = Arc::new(ManagedState::new_with_mcp(NoopRuntime));
        let supervisor = state
            .spawn_realization_lease_supervisor()
            .expect("R1 starts the sole supervisor");
        assert!(
            state.spawn_realization_lease_supervisor().is_none(),
            "R2 rejects a parallel supervisor"
        );
        tokio::task::yield_now().await;
        supervisor.abort();
    }

    fn exact_receipts(action: &SessionRealizationAction) -> Vec<McpRealizationReceipt> {
        let SessionRealizationAction::Stage { mcp_stages, .. } = action else {
            panic!("expected stage action")
        };
        mcp_stages
            .iter()
            .map(|request| McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            })
            .collect()
    }

    #[tokio::test]
    async fn realization_phase_cases_are_generated_from_the_decision_table() {
        // Cause graph:
        // Frozen Session + valid target -> durable lease/claims -> exact complete
        // receipts -> durable activation -> exact publish/drain acknowledgement ->
        // Complete. A stale owner, missing/mismatched receipt, or wrong ack set
        // terminates before the next root mutation. Exact retries replay.
        //
        // | Rule | Phase | lease | receipt set | ack set | Effect |
        // |---|---|---|---|---|---|
        // | Q1 | begin | new valid | - | - | Stage + durable claims |
        // | Q2 | begin replay | exact live | - | - | same Stage/no revision |
        // | Q3 | begin | other live owner | - | - | stale/no mutation |
        // | Q4 | activate | exact | missing | - | invalid/no mutation |
        // | Q5 | activate | exact | fingerprint mismatch | - | invalid/no mutation |
        // | Q6 | activate | exact | exact complete | - | Publish + durable Active |
        // | Q7 | activate replay | exact | exact complete | - | same Publish/no revision |
        // | Q8 | acknowledge | exact | - | missing | invalid/no mutation |
        // | Q9 | acknowledge | exact | - | duplicate | invalid/no mutation |
        // | Q10 | acknowledge | exact | - | exact | Complete + durable idle |
        // | Q11 | acknowledge replay | exact | - | exact | Complete/no revision |
        // | Q12 | renew | same owner/incarnation | - | later expiry | Stage same generation |
        // | Q13 | renew complete | exact | exact | exact | Complete + extended fence |
        let (state, repo) = harness("session-phase").await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX - 1,
            renew_existing_lease: false,
        };
        let begin = BeginSessionRealization {
            session_id: "session-phase".into(),
            target: target.clone(),
        };
        let staged = state
            .application
            .begin_session_realization(begin.clone())
            .await
            .expect("Q1");
        assert!(matches!(
            staged.action,
            SessionRealizationAction::Stage { .. }
        ));
        let after_begin = repo.get("session-phase").await.unwrap();
        assert_eq!(
            after_begin.mcp.attachments[0].state,
            McpAttachmentState::Realizing,
            "Q1"
        );
        assert!(
            after_begin.resources.activations.is_empty(),
            "Q1 empty resources"
        );

        let replay = state
            .application
            .begin_session_realization(begin)
            .await
            .expect("Q2");
        assert_eq!(replay, staged, "Q2");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q2"
        );

        let stale = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-phase".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-b".into(),
                    ..target
                },
            })
            .await;
        assert_eq!(
            stale.unwrap_err(),
            SessionRealizationControlFailure::StaleOwnership,
            "Q3"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q3"
        );

        let missing = state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: Vec::new(),
            })
            .await;
        assert!(
            matches!(missing, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q4"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q4"
        );

        let receipts = exact_receipts(&staged.action);
        let mut mismatched = receipts.clone();
        mismatched[0].receipt_fingerprint = "another-request".into();
        let mismatch = state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: mismatched,
            })
            .await;
        assert!(
            matches!(mismatch, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q5"
        );

        let activated = state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: receipts.clone(),
            })
            .await
            .expect("Q6");
        let (publish, drain) = match &activated.action {
            SessionRealizationAction::Publish { publish, drain } => {
                (publish.clone(), drain.clone())
            }
            _ => panic!("Q6 expected publish"),
        };
        assert_eq!(publish.len(), 1, "Q6");
        assert!(drain.is_empty(), "Q6");
        let after_activate = repo.get("session-phase").await.unwrap();
        assert_eq!(
            after_activate.mcp.attachments[0].state,
            McpAttachmentState::Active,
            "Q6"
        );
        assert!(after_activate.resources.pending.is_none(), "Q6");

        let activate_replay = state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: receipts,
            })
            .await
            .expect("Q7");
        assert_eq!(activate_replay, activated, "Q7");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q7"
        );

        let wrong_ack = state
            .application
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                published: Vec::new(),
                drained: Vec::new(),
            })
            .await;
        assert!(
            matches!(wrong_ack, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q8"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q8"
        );

        let duplicate_ack = state
            .application
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                published: vec![publish[0].clone(), publish[0].clone()],
                drained: Vec::new(),
            })
            .await;
        assert!(
            matches!(
                duplicate_ack,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "Q9"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q9"
        );

        let acknowledgement = AcknowledgeSessionRealization {
            session_id: "session-phase".into(),
            lease: staged.lease,
            published: publish,
            drained: drain,
        };
        let complete = state
            .application
            .acknowledge_session_realization(acknowledgement.clone())
            .await
            .expect("Q10");
        assert_eq!(complete.action, SessionRealizationAction::Complete, "Q10");
        let after_ack = repo.get("session-phase").await.unwrap();
        assert_eq!(after_ack.status, "idle", "Q10");
        assert!(after_ack.mcp.attachments[0].publication_acknowledged, "Q10");

        let ack_replay = state
            .application
            .acknowledge_session_realization(acknowledgement)
            .await
            .expect("Q11");
        assert_eq!(ack_replay.action, SessionRealizationAction::Complete, "Q11");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_ack.revision,
            "Q11"
        );

        let renewal = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-phase".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-a".into(),
                    runtime_incarnation: "worker-a/incarnation-1".into(),
                    lease_expires_at_unix_ms: u64::MAX,
                    renew_existing_lease: true,
                },
            })
            .await
            .expect("Q12");
        let SessionRealizationAction::Stage { mcp_stages, .. } = &renewal.action else {
            panic!("Q12 expected renewal stage")
        };
        assert_eq!(mcp_stages.len(), 1, "Q12");
        assert_eq!(
            mcp_stages[0].generation.generation,
            awaken_session_contract::McpGeneration(1),
            "Q12"
        );
        assert_eq!(
            mcp_stages[0].generation.lease_expires_at_unix_ms,
            u64::MAX,
            "Q12"
        );
        let publish = state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: renewal.lease.clone(),
                mcp_receipts: exact_receipts(&renewal.action),
            })
            .await
            .expect("Q13 publish");
        let SessionRealizationAction::Publish { publish, drain } = publish.action else {
            panic!("Q13 expected publish")
        };
        let complete = state
            .application
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: renewal.lease,
                published: publish,
                drained: drain,
            })
            .await
            .expect("Q13");
        assert_eq!(complete.action, SessionRealizationAction::Complete, "Q13");
        let renewed = repo.get("session-phase").await.unwrap();
        assert_eq!(
            renewed.realization.unwrap().expires_at_unix_ms,
            u64::MAX,
            "Q13"
        );
        assert!(renewed.mcp.attachments[0].publication_acknowledged, "Q13");
    }

    #[tokio::test]
    async fn local_lease_supervision_cases_follow_the_decision_table() {
        // Cause graph: active generation AND local owner/incarnation AND expiry
        // within the renewal window -> run the canonical phase driver with a
        // later exact fence. A false due-window cause is a no-op; there is no
        // relay-local timer or second desired-state mutation.
        //
        // | Rule | Active | Owner/incarnation | Due | Effect |
        // |---|---|---|---|---|
        // | S1 | yes | exact local | yes | same generation/epoch, later expiry |
        // | S2 | yes | exact local | no | no mutation |
        let (state, repo) = harness("session-supervised").await;
        state
            .realize_session_locally("session-supervised")
            .await
            .expect("S1 setup");
        let before = repo.get("session-supervised").await.unwrap();
        let lease = before.realization.clone().unwrap();
        let original_generation = before.mcp.attachments[0].generation;
        let original_epoch = lease.epoch;
        let supervision_now = lease.expires_at_unix_ms.saturating_sub(100_000);
        assert_eq!(
            state
                .renew_due_session_realizations(supervision_now)
                .await
                .expect("S1"),
            1,
            "S1"
        );
        let renewed = repo.get("session-supervised").await.unwrap();
        let renewed_lease = renewed.realization.as_ref().unwrap();
        assert!(
            renewed_lease.expires_at_unix_ms > lease.expires_at_unix_ms,
            "S1"
        );
        assert_eq!(renewed_lease.epoch, original_epoch, "S1");
        assert_eq!(
            renewed.mcp.attachments[0].generation, original_generation,
            "S1"
        );
        assert!(renewed.mcp.attachments[0].publication_acknowledged, "S1");
        let revision = renewed.revision;
        assert_eq!(
            state
                .renew_due_session_realizations(supervision_now)
                .await
                .expect("S2"),
            0,
            "S2"
        );
        assert_eq!(
            repo.get("session-supervised").await.unwrap().revision,
            revision,
            "S2 no mutation"
        );
    }

    #[tokio::test]
    async fn live_lease_takeover_follows_owner_and_incarnation_decision_table() {
        // Cause-effect graph:
        // live lease + exact owner + exact incarnation -> replay;
        // live lease + exact owner + new incarnation -> fence old process and
        // allocate epoch N+1; live lease + different owner -> stale.
        //
        // | Rule | lease | owner | incarnation | Effect |
        // | O1 | live | same | same | replay epoch N |
        // | O2 | live | same | new | claim epoch N+1 |
        // | O3 | live | other | any | StaleOwnership |
        let (state, repo) = harness("session-owner-restart").await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX,
            renew_existing_lease: false,
        };
        let first = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: target.clone(),
            })
            .await
            .expect("O1 initial claim");
        let replay = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: target.clone(),
            })
            .await
            .expect("O1 replay");
        assert_eq!(replay.lease, first.lease, "O1");

        let restarted = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    runtime_incarnation: "worker-a/incarnation-2".into(),
                    ..target.clone()
                },
            })
            .await
            .expect("O2");
        assert_eq!(restarted.lease.epoch, first.lease.epoch + 1, "O2");
        assert_eq!(
            restarted.lease.runtime_incarnation, "worker-a/incarnation-2",
            "O2"
        );
        let persisted = repo.get("session-owner-restart").await.unwrap();
        assert_eq!(
            persisted.mcp.attachments[0]
                .realization
                .as_ref()
                .unwrap()
                .runtime_incarnation,
            "worker-a/incarnation-2",
            "O2 exact generation was reclaimed"
        );

        let other = state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-b".into(),
                    runtime_incarnation: "worker-b/incarnation-1".into(),
                    ..target
                },
            })
            .await;
        assert_eq!(
            other.unwrap_err(),
            SessionRealizationControlFailure::StaleOwnership,
            "O3"
        );
    }

    #[tokio::test]
    async fn empty_and_failure_cases_are_generated_from_the_decision_table() {
        // Cause graph: pending Resources without MCP still requires
        // Stage/activation/ack. An exact failure terminalizes Realizing MCP and
        // records retryable Resource evidence once; re-delivery is a no-op.
        //
        // | Rule | Pending resources | MCP | Command | Effect |
        // |---|---|---|---|---|
        // | F0 | F | empty | first assignment | Stage(prepare=true) |
        // | F1 | T | empty | begin | Stage(prepare=true) |
        // | F2 | T | empty | activate+ack | idle, one commit per phase |
        // | F3 | T | Realizing | fail | Failed + activation_failed |
        // | F4 | T | Failed | same fail | replay/no revision |
        let mut baseline_only = persisted_session("session-baseline-only");
        baseline_only.resources = Default::default();
        baseline_only.mcp = SessionMcpAttachmentSet::default();
        let (baseline_state, _) = harness_for(baseline_only).await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX,
            renew_existing_lease: false,
        };
        let baseline_stage = baseline_state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-baseline-only".into(),
                target: target.clone(),
            })
            .await
            .expect("F0");
        assert!(matches!(
            baseline_stage.action,
            SessionRealizationAction::Stage {
                prepare_session: true,
                ref mcp_stages,
            } if mcp_stages.is_empty()
        ));

        let mut empty = persisted_session("session-empty");
        empty.mcp = SessionMcpAttachmentSet::default();
        let (empty_state, empty_repo) = harness_for(empty).await;
        let staged = empty_state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-empty".into(),
                target: target.clone(),
            })
            .await
            .expect("F1");
        assert!(
            matches!(
                staged.action,
                SessionRealizationAction::Stage {
                    prepare_session: true,
                    ref mcp_stages,
                } if mcp_stages.is_empty()
            ),
            "F1"
        );
        let activated = empty_state
            .application
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-empty".into(),
                lease: staged.lease.clone(),
                mcp_receipts: Vec::new(),
            })
            .await
            .expect("F2 activate");
        assert!(
            matches!(
                activated.action,
                SessionRealizationAction::Publish {
                    ref publish,
                    ref drain,
                } if publish.is_empty() && drain.is_empty()
            ),
            "F2"
        );
        empty_state
            .application
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-empty".into(),
                lease: staged.lease,
                published: Vec::new(),
                drained: Vec::new(),
            })
            .await
            .expect("F2 acknowledge");
        assert_eq!(
            empty_repo.get("session-empty").await.unwrap().status,
            "idle",
            "F2"
        );

        let (failed_state, failed_repo) = harness("session-failed").await;
        let staged = failed_state
            .application
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-failed".into(),
                target,
            })
            .await
            .unwrap();
        let failure = FailSessionRealization {
            session_id: "session-failed".into(),
            lease: staged.lease,
            reason: "stage failed".into(),
        };
        failed_state
            .application
            .fail_session_realization(failure.clone())
            .await
            .expect("F3");
        let after_failure = failed_repo.get("session-failed").await.unwrap();
        assert_eq!(after_failure.status, "activation_failed", "F3");
        assert_eq!(
            after_failure.mcp.attachments[0].state,
            McpAttachmentState::Failed,
            "F3"
        );
        failed_state
            .application
            .fail_session_realization(failure)
            .await
            .expect("F4");
        assert_eq!(
            failed_repo.get("session-failed").await.unwrap().revision,
            after_failure.revision,
            "F4"
        );
    }
}
