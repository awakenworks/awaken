// @formal surface obligation=remote_worker.recovery_projection_is_committed_prefix
// @formal surface obligation=session.visible_row_always_owned
// @formal reason: Rehydrates disposable projections from canonical Session truth.
//! Cold reconstruction of disposable Managed Session projections.

use super::*;
use awaken_session_application::SessionRecoveryPurpose;

#[cfg(test)]
pub(super) struct ColdPublicationPause {
    pub(super) published: tokio::sync::oneshot::Sender<()>,
    pub(super) resume: tokio::sync::oneshot::Receiver<()>,
}

impl ManagedState {
    pub(super) async fn project_replayed_session(
        &self,
        session_id: &str,
    ) -> Result<Session, StateError> {
        self.ensure_session(session_id).await?;
        self.get_session(session_id)
    }

    /// Project an exact product create receipt even when realization has
    /// reached a durable failed state. The Session root is the asynchronous
    /// operation, so retry returns that resource and never starts a replacement
    /// operation or repeats physical effects.
    pub(crate) async fn replay_accepted_session_with_receipt(
        &self,
        session_id: &str,
        workspace_id: &str,
        idempotency: &awaken_session_state_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, StateError> {
        self.roles
            .creation
            .replay_accepted_session_create(
                workspace_id,
                &awaken_session_types::SessionId::from(session_id),
                idempotency,
            )
            .await
            .map_err(Self::map_create_replay_error)
    }

    /// Read the existing Session application projection without requiring any
    /// Runtime transcript or executable publication. Resource metadata and cold
    /// reconstruction share its visibility/owner/finalization/error rules.
    pub(super) async fn read_persisted_session_projection(
        &self,
        id: &str,
    ) -> Result<awaken_session_application::RecoveredSessionProjection, StateError> {
        let recovered = self
            .roles
            .projection
            .read_session_projection(id, None)
            .await
            .map_err(|error| match error {
                awaken_session_application::SessionProjectionRecoveryError::NotFound => {
                    StateError::NotFound
                }
                awaken_session_application::SessionProjectionRecoveryError::Rejected(error) => {
                    StateError::Run(error)
                }
                awaken_session_application::SessionProjectionRecoveryError::Unavailable(
                    message,
                ) => StateError::Run(RunError::unavailable(message)),
            })?;
        // A Thread without its durable Session root cannot manufacture identity.
        recovered.ok_or(StateError::NotFound)
    }

    /// Ensure only the base disposable Session record exists.  Runtime-derived
    /// child links and lifecycle are deliberately absent here: both cold and
    /// warm callers consume those facts through `refresh_committed_projection`,
    /// so recovery cannot grow a second projector.
    pub(super) async fn ensure_session_record(&self, id: &str) -> Result<(), StateError> {
        self.hydrate_session_record(id, SessionRecoveryPurpose::Interactive)
            .await
    }

    /// The one cold path from a durable Session root to the disposable wire
    /// record. Interactive recovery refreshes executable projections and
    /// enters their admission first; frozen control (interrupt, archive,
    /// delete) must not prepare a Runtime, so it projects only the durable
    /// baseline. Both share visibility, owner, finalization and identity rules;
    /// the record's Agent is the frozen baseline's, never a placeholder.
    pub(super) async fn hydrate_session_record(
        &self,
        id: &str,
        purpose: SessionRecoveryPurpose,
    ) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        let recovered = self.read_persisted_session_projection(id).await?;
        let owner_scope = recovered.owner_scope;
        let persisted = recovered.session;
        // `read_persisted_session_projection` admits only finalized roots, so
        // a missing Agent is a damaged root: fail closed rather than bind the
        // record to an Agent the operator never published.
        let agent_id = persisted.agent_id().map(str::to_string).ok_or_else(|| {
            StateError::Run(RunError::unavailable(
                "Session root is not finalized and cannot be rehydrated",
            ))
        })?;
        let session_revision = persisted.revision;
        let root_time = RootTimeEvidence::from(&persisted);
        let resource_state = persisted.resources.clone();
        let session = match purpose {
            SessionRecoveryPurpose::Interactive => {
                let admission = self
                    .roles
                    .creation
                    .refresh_executable_projections()
                    .await
                    .map_err(|error| {
                        StateError::Run(RunError::unavailable_classified(
                            "executable_projection_refresh_failed",
                            format!("Executable projections could not be refreshed: {error}"),
                        ))
                    })?;
                admission.enter(|| self.rehydrated_session(id, &owner_scope, persisted))?
            }
            SessionRecoveryPurpose::CollectionRead | SessionRecoveryPurpose::FrozenControl => {
                self.rehydrated_session_for(id, &owner_scope, persisted, purpose)?
            }
        };
        let record = SessionRecord::new(
            owner_scope,
            agent_id,
            session,
            session_revision,
            resource_state,
            root_time,
            Vec::new(),
        );
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        // The lock publishes owner and content together; no reader can observe
        // a warm record while its authorization projection is still missing.
        #[cfg(test)]
        {
            let pause = self.cold_publication_pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.published.send(()).expect("cold publication observer");
                pause.resume.await.expect("resume cold publication");
            }
        }
        Ok(())
    }

    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). Only a durable Session aggregate authorizes
    /// rebuilding the disposable wire record. A transcript without that root is
    /// an orphan and stays `NotFound`; it can never manufacture Session identity,
    /// ownership, configuration, or mutation authority.
    pub(crate) async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        self.ensure_session_record(id).await?;
        self.refresh_committed_projection(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{
        RehydrateFake, create_session_fixture, ephemeral_session_repo,
    };
    use crate::state::tests::{sample_inputs, sample_persisted};

    #[tokio::test]
    async fn orphan_transcript_cannot_manufacture_a_session_root() {
        // Cause/effect graph: C1 the disposable Managed cache is cold; C2 the
        // Runtime has committed Thread messages; C3 the authoritative Session
        // repository has no aggregate. Effects: E1 recovery returns NotFound;
        // E2 no owner, Session DTO, or mutable cache entry is synthesized; E3
        // transcript projection is not consulted as an existence fallback.
        // Decision rules: R1=C1+C2+!C3=>E1+E2+E3; the existing durable recovery
        // tests cover R2=C1+C2+C3=>exact root projection.
        let runtime = RehydrateFake::default();
        *runtime.committed.lock().unwrap() =
            Some(vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("orphan-message".into()),
                awaken_agent_contract::agent::message::Role::User,
                "orphan transcript",
            )]);
        let order = runtime.order.clone();
        let state = ManagedState::new(runtime).await;

        assert!(
            matches!(
                state.ensure_session("orphan-session").await,
                Err(StateError::NotFound)
            ),
            "R1/E1"
        );
        assert!(state.list_sessions().is_empty(), "R1/E2");
        assert!(order.lock().unwrap().is_empty(), "R1/E3");
    }

    #[tokio::test]
    async fn profiled_create_replay_projects_only_the_repository_receipt_and_owner() {
        // Cause/effect graph: C1 identity absent/present; C2 repository receipt
        // absent/present; C3 payload hash matches; C4 Workspace owner matches;
        // C5 activation is live/failed. Effects: E1 absent identity+receipt means
        // create may proceed; E2 an occupied identity without this receipt,
        // mismatched hash, or mismatched owner is an idempotency conflict; E3 an
        // exact live or failed receipt returns the durable asynchronous-operation
        // root without rehydration. No metadata key participates in any rule.
        // Decision rules R1=!C1+!C2=>E1, R2=C1+(!C2|!C3|!C4)=>E2,
        // R3=C1+C2+C3+C4+live=>E3, R4=same+failed=>E3.
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let id = "profiled-receipt-live";
        let receipt = awaken_session_state_contract::IdempotencyRecord {
            key: format!("profiled:create:{id}"),
            payload_hash: "request-a".into(),
        };
        let live = sample_persisted(id);
        repo.create("workspace-a", live, receipt.clone(), Vec::new())
            .await
            .expect("persist exact receipt");
        let durable_before = repo
            .get(&awaken_session_types::SessionId::from(id))
            .await
            .unwrap();
        let restarted = {
            let session_fixture_runtime = RehydrateFake::default();
            let session_fixture_clock = awaken_time::system_clock();
            crate::state::test_support::managed_state_from_parts(
                std::sync::Arc::new(session_fixture_runtime),
                None,
                repo.clone(),
                awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                    .execution,
                session_fixture_clock,
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
        };

        assert!(
            restarted
                .replay_accepted_session_with_receipt(
                    "profiled-receipt-absent",
                    "workspace-a",
                    &awaken_session_state_contract::IdempotencyRecord {
                        key: "profiled:create:profiled-receipt-absent".into(),
                        payload_hash: "request-a".into(),
                    },
                )
                .await
                .expect("R1/E1")
                .is_none(),
            "R1/E1"
        );
        for (rule, owner, candidate) in [
            (
                "R2-no-receipt",
                "workspace-a",
                awaken_session_state_contract::IdempotencyRecord {
                    key: format!("profiled:create:{id}:other"),
                    payload_hash: "request-a".into(),
                },
            ),
            (
                "R2-hash",
                "workspace-a",
                awaken_session_state_contract::IdempotencyRecord {
                    payload_hash: "request-b".into(),
                    ..receipt.clone()
                },
            ),
            ("R2-owner", "workspace-b", receipt.clone()),
        ] {
            assert!(
                matches!(
                    restarted
                        .replay_accepted_session_with_receipt(id, owner, &candidate)
                        .await,
                    Err(StateError::IdempotencyMismatch)
                ),
                "{rule}/E2"
            );
        }
        let replayed = restarted
            .replay_accepted_session_with_receipt(id, "workspace-a", &receipt)
            .await
            .expect("R3 exact receipt")
            .expect("R3/E3 durable Session root");
        assert_eq!(replayed.session_id, id, "R3/E3");
        assert!(restarted.list_sessions().is_empty(), "R3/E3 no wire cache");
        assert_eq!(
            repo.get(&awaken_session_types::SessionId::from(id))
                .await
                .unwrap(),
            durable_before,
            "R1-R3 no mutation"
        );

        let failed_id = "profiled-receipt-failed";
        let failed_receipt = awaken_session_state_contract::IdempotencyRecord {
            key: format!("profiled:create:{failed_id}"),
            payload_hash: "request-failed".into(),
        };
        let mut failed = sample_persisted(failed_id);
        failed.execution = SessionExecutionState::ActivationFailed;
        repo.create("workspace-a", failed, failed_receipt.clone(), Vec::new())
            .await
            .expect("persist failed receipt");
        let failed_replay = restarted
            .replay_accepted_session_with_receipt(failed_id, "workspace-a", &failed_receipt)
            .await
            .expect("R4 exact failed receipt")
            .expect("R4/E3 durable failed root");
        assert_eq!(
            failed_replay.execution,
            SessionExecutionState::ActivationFailed,
            "R4/E3"
        );
        assert!(restarted.list_sessions().is_empty(), "R4/E3 no wire cache");
    }

    #[tokio::test]
    async fn coordinator_rehydrate_does_not_adopt_a_worker_owned_environment() {
        // Cause/effect graph: C1 durable Session cache is cold; C2 an opaque
        // Environment binding exists; C3 an immutable Runtime placement may
        // require a Worker; C4 a legacy row may omit that fact and is resolved
        // by the Session application's registered-Worker configuration; C5,
        // independently, the durable lease owner is absent, local, Worker-live,
        // or Worker-expired and cannot change placement; C6 MCP projection is
        // settled or requires recovery; C7 Resource projection is stable or pending.
        // Effects: E1 frozen projection/history become readable without physical
        // effects; E2 execution admission/reconciliation (not this read) may
        // realize a local Session; E3 either Worker-owned path leaves adoption,
        // MCP staging, and Resource projection to claimed-dispatch recovery; E4
        // both background Coordinator reconcilers skip Worker-owned rows.
        //
        // | Rule | ownership | lease | MCP | Resource | local effects | read |
        // | R1 | local | absent/local | settled/required | stable/pending | none on read | yes |
        // | R2 | Environment WorkQueue | any | settled/required | stable/pending | none on read | yes |
        // | R3 | deployment-frozen Worker Runtime | any | settled/required | stable/pending | none | yes |
        // | R4 | any | any | any | any | no adopt if no binding | yes |
        //
        // R1 is covered by `ensure_session_rehydrates_from_repo_after_cache_loss`;
        // This test generates the complete external R3 cross-product with an adapter
        // that fails if any Coordinator-local physical effect is attempted. An
        // Lease owner/liveness never overrides the frozen custody fact. R3 is
        // covered by local Session creation and Resource activation tests.
        // Constraints/invariants: frozen Runtime placement, not cache warmth or
        // lease liveness, owns physical custody; rehydration may rebuild only the
        // read/projection state and cannot adopt or stage Worker-owned effects.
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let mut cases = Vec::new();
        for (owner_name, placement) in [(
            "topology",
            awaken_session_types::SessionRuntimePlacement::Worker,
        )] {
            for (lease_name, realization) in [
                ("absent", None),
                (
                    "local_live",
                    Some(
                        awaken_session_configuration_contract::resource::SessionRealizationLease {
                            owner: "managed-runtime/boot-1".into(),
                            runtime_incarnation: "managed-runtime/boot-1".into(),
                            epoch: 4,
                            expires_at_unix_ms: u64::MAX,
                        },
                    ),
                ),
                (
                    "worker_live",
                    Some(
                        awaken_session_configuration_contract::resource::SessionRealizationLease {
                            owner: "worker-a".into(),
                            runtime_incarnation: "worker-a/boot-1".into(),
                            epoch: 4,
                            expires_at_unix_ms: u64::MAX,
                        },
                    ),
                ),
                (
                    "worker_expired",
                    Some(
                        awaken_session_configuration_contract::resource::SessionRealizationLease {
                            owner: "worker-a".into(),
                            runtime_incarnation: "worker-a/boot-1".into(),
                            epoch: 4,
                            expires_at_unix_ms: 0,
                        },
                    ),
                ),
            ] {
                for (mcp_name, mcp_recovery) in [("settled", false), ("pending", true)] {
                    for (resource_name, resource_recovery) in [("stable", false), ("pending", true)]
                    {
                        let id =
                            format!("sesn_{owner_name}_{lease_name}_{mcp_name}_{resource_name}");
                        let mut persisted = sample_persisted(&id);
                        if !mcp_recovery {
                            persisted.mcp = Default::default();
                        }
                        if resource_recovery {
                            persisted.resources = Default::default();
                            persisted
                                .resources
                                .prepare(
                                    &awaken_session_types::SessionId::from(id.as_str()),
                                    sample_inputs(),
                                )
                                .expect("pending Worker Resource projection");
                        }
                        persisted.environment.set_resident("worker-opaque-binding");
                        let awaken_session_configuration_contract::SessionBaselineState::Frozen(
                            baseline,
                        ) = &mut persisted.baseline
                        else {
                            unreachable!("sample baseline is frozen")
                        };
                        baseline.environment.self_hosted = false;
                        baseline.runtime_placement = placement;
                        persisted.realization = realization.clone();
                        create_session_fixture(
                            repo.as_ref(),
                            awaken_tenancy::DEFAULT_WORKSPACE_ID,
                            persisted,
                        )
                        .await;
                        cases.push((id, mcp_recovery, resource_recovery));
                    }
                }
            }
        }

        let runtime = RehydrateFake::default();
        runtime
            .reject_environment_adoption
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let restored_environments = runtime.restored_environments.clone();
        let restored_runtimes = runtime.restored_runtimes.clone();
        let restored_inputs = runtime.restored.clone();
        let runtime = Arc::new(runtime);
        let environments =
            awaken_protocol_managed_testkit::environment_fixture(awaken_time::system_clock())
                .execution;
        let application = awaken_session_application::SessionApplicationBuilder::fixture(
            runtime.clone(),
            runtime,
            awaken_session_application::SessionPersistence::new(repo, awaken_time::system_clock()),
            environments.clone(),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .configuration(
            awaken_session_application::SessionApplicationConfiguration {
                execution_placement:
                    awaken_session_application::SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
        )
        .build()
        .expect("valid fixture Session application");
        let restarted = ManagedState::from_application(
            Arc::new(application),
            awaken_data_governance_contract::CapturePolicy::default(),
        );

        assert_eq!(
            restarted.reconcile_session_realizations().await,
            0,
            "R2-R6/E4"
        );
        assert_eq!(
            restarted.reconcile_resource_activations().await,
            0,
            "R2-R6/E4"
        );
        for (id, mcp_recovery, resource_recovery) in &cases {
            restarted
                .ensure_session(id)
                .await
                .unwrap_or_else(|error| panic!("{id}/E1: {error}"));
            assert!(restarted.get_session(id).is_ok(), "{id}/E1");
            assert_eq!(
                restarted
                    .roles
                    .roots
                    .session(&awaken_session_types::SessionId::from(id))
                    .await
                    .unwrap()
                    .mcp
                    .needs_reconciliation(),
                *mcp_recovery,
                "{id}/E3: Coordinator must not mutate Worker MCP state"
            );
            assert_eq!(
                restarted
                    .roles
                    .roots
                    .session(&awaken_session_types::SessionId::from(id))
                    .await
                    .unwrap()
                    .resources
                    .pending
                    .is_some(),
                *resource_recovery,
                "{id}/E3: Coordinator must not mutate Worker Resource state"
            );
        }
        assert!(restored_environments.lock().unwrap().is_empty(), "R2-R6/E3");
        assert!(restored_inputs.lock().unwrap().is_empty(), "R2-R6/E3");
        assert!(restored_runtimes.lock().unwrap().is_empty(), "R2-R6/E1");
    }

    #[tokio::test]
    async fn one_cold_path_binds_the_frozen_agent_for_every_purpose() {
        // Cause/effect graph: C1 cold wire cache; C2 purpose Interactive or
        // FrozenControl; C3 the durable root is finalized and visible, hidden,
        // or absent. Effects: E1 the record binds the frozen baseline's Agent
        // (never a placeholder) and the owner; E2 NotFound with no record.
        // | Rule | C2 | C3 | Effect |
        // |---|---|---|---|
        // | H1 | Interactive | finalized | E1 |
        // | H2 | FrozenControl | finalized | E1 |
        // | H3 | either | Deleted (hidden) | E2 |
        // | H4 | either | absent | E2 |
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let mut hidden = sample_persisted("hydrate-hidden");
        hidden.disposition = awaken_session_state_contract::SessionDisposition::Deleted;
        for session in [
            sample_persisted("hydrate-interactive"),
            sample_persisted("hydrate-frozen"),
            hidden,
        ] {
            let id = session.session_id.clone();
            repo.create(
                "workspace-a",
                session,
                awaken_session_state_contract::IdempotencyRecord {
                    key: format!("hydrate:{}", id.as_str()),
                    payload_hash: "fixture".into(),
                },
                Vec::new(),
            )
            .await
            .expect("persist durable root");
        }
        let state = {
            let session_fixture_runtime = RehydrateFake::default();
            let session_fixture_clock = awaken_time::system_clock();
            crate::state::test_support::managed_state_from_parts(
                std::sync::Arc::new(session_fixture_runtime),
                None,
                repo,
                awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                    .execution,
                session_fixture_clock,
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
        };
        for (rule, id, purpose) in [
            (
                "H1",
                "hydrate-interactive",
                SessionRecoveryPurpose::Interactive,
            ),
            (
                "H2",
                "hydrate-frozen",
                SessionRecoveryPurpose::FrozenControl,
            ),
        ] {
            state
                .hydrate_session_record(id, purpose)
                .await
                .unwrap_or_else(|error| panic!("{rule}: {error:?}"));
            let agent_id = state
                .sessions
                .lock()
                .unwrap()
                .get(id)
                .map(|record| record.agent_id.clone());
            assert_eq!(agent_id.as_deref(), Some("coder"), "{rule}/E1");
            assert_eq!(
                state.owner_scope(id).as_deref(),
                Some("workspace-a"),
                "{rule}/E1"
            );
        }
        for (rule, id) in [("H3", "hydrate-hidden"), ("H4", "hydrate-absent")] {
            for purpose in [
                SessionRecoveryPurpose::Interactive,
                SessionRecoveryPurpose::FrozenControl,
            ] {
                assert!(
                    matches!(
                        state.hydrate_session_record(id, purpose).await,
                        Err(StateError::NotFound)
                    ),
                    "{rule}/E2 {purpose:?}"
                );
                assert!(
                    !state.sessions.lock().unwrap().contains_key(id),
                    "{rule}/E2 no record"
                );
                assert!(state.owner_scope(id).is_none(), "{rule}/E2 no owner");
            }
        }
    }
}

#[cfg(test)]
mod cache_publication_tests;
