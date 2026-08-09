//! Cold reconstruction of disposable Managed Session projections.

use super::*;

impl ManagedState {
    /// Recover a session whose in-memory record was lost from durable truth (a
    /// process restart, ADR-0039). If the store holds a committed transcript for
    /// `id`, rebuild the record — the projected history plus a reconstructed
    /// session object — so a resume can continue the awaiting run. A thread with no
    /// committed truth stays `NotFound` (fail closed): the store is authoritative.
    pub(crate) async fn ensure_session(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        // Install the persisted, already-resolved resource snapshot BEFORE opening
        // runtime history. Opening a thread constructs its context; doing that first
        // would transiently resolve today's Agent/Skill configuration and could both
        // drift from the Session pin and mutate its sandbox before the pin is known.
        let persisted = self.application.session_repository().get(id).await;
        let owner_scope = self
            .application
            .session_repository()
            .owner(id)
            .await
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let mut persisted = match persisted {
            Some(session) => Some(
                self.application
                    .reconcile_persisted_resources(&owner_scope, session)
                    .await
                    .map_err(Self::map_preparation_error)?,
            ),
            None => None,
        };
        // Terminated Sessions remain readable tombstones; deleted and failed
        // activation rows are hidden from the public read model.
        if persisted.as_ref().is_some_and(|session| {
            matches!(session.status.as_str(), "deleted" | "activation_failed")
        }) {
            return Err(StateError::NotFound);
        }
        if let Some(session) = persisted.clone() {
            session.frozen_baseline().ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "cannot realize a Session whose baseline is still preparing",
                ))
            })?;
            // Reconciliation already crosses the canonical projection synchronizer,
            // which prepares the frozen facts and restores the durable Environment
            // before staging MCP. Calling either operation here as well would stage
            // resources or adopt the sandbox twice after cache loss. A settled
            // Session has no reconciliation phase, so it performs that same order
            // directly through the Runtime port.
            let worker_owned_realization = self.application.requires_external_realization(&session);
            let recovered = if !worker_owned_realization {
                // Cold local recovery always crosses ownership assignment before
                // adopting or rebuilding physical state. A new process must not
                // bypass a still-live predecessor merely because MCP is settled.
                self.realize_session_locally(id).await?
            } else {
                self.application
                    .install_dispatch_projection(&owner_scope, &session)
                    .await
                    .map_err(Self::map_realization_application_error)?;
                session
            };
            persisted = Some(recovered.clone());
        }
        let pending = self
            .application
            .runtime()
            .pending_tool(id)
            .await
            .map_err(StateError::Run)?;
        let messages = self.application.runtime().committed_messages(id).await;
        if messages.is_empty() && persisted.is_none() {
            return Err(StateError::NotFound);
        }
        let projected_message_ids = messages
            .iter()
            .map(|message| message.id.0.clone())
            .collect();
        let pending = pending.as_ref();
        let events: Vec<Event> = project_messages(&messages, pending)
            .into_iter()
            .map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            })
            .collect();
        let agent_id = persisted
            .as_ref()
            .and_then(PersistedSession::agent_id)
            .map_or_else(|| "assistant".to_string(), str::to_string);
        let resource_state = persisted
            .as_ref()
            .map(|session| session.resources.clone())
            .unwrap_or_default();
        let session = self.rehydrated_session(id, persisted)?;
        let delegated_runs = self
            .application
            .runtime()
            .delegated_runs(id)
            .await
            .map_err(StateError::Run)?;
        let mut record = SessionRecord::new(
            agent_id,
            session,
            resource_state,
            events,
            projected_message_ids,
        );
        self.append_delegation_projections(&mut record, &delegated_runs);
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        self.owners
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(owner_scope);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{
        RehydrateFake, create_session_fixture, ephemeral_session_repo,
    };
    use crate::state::tests::{sample_inputs, sample_persisted};
    use awaken_session_contract::ManagedSessionRepository;

    #[tokio::test]
    async fn coordinator_rehydrate_does_not_adopt_a_worker_owned_environment() {
        // Cause/effect graph: C1 durable Session cache is cold; C2 an opaque
        // Environment binding exists; C3 an immutable Runtime/application fact
        // may require a Worker; C4 a legacy row may omit that fact and is resolved
        // by the Session application's registered-Worker configuration; C5,
        // independently, the durable lease owner is absent, local, Worker-live,
        // or Worker-expired and cannot change placement; C6 MCP projection is
        // settled or requires recovery; C7 Resource projection is stable or pending.
        // Effects: E1 frozen projection/history become readable; E2 only an
        // locally realized Session may call local physical adoption; E3 either
        // Worker-owned path leaves adoption, MCP staging, and Resource projection
        // to claimed-dispatch recovery; E4 both background Coordinator reconcilers
        // skip it.
        //
        // | Rule | ownership | lease | MCP | Resource | local effects | read |
        // | R1 | local | absent/local | settled/required | stable/pending | canonical local effects | yes |
        // | R2 | application | any | settled/required | stable/pending | none | yes |
        // | R3 | Environment WorkQueue only | any | settled/required | stable/pending | canonical local effects | yes |
        // | R4 | deployment-frozen Worker Runtime | any | settled/required | stable/pending | none | yes |
        // | R5 | any | any | any | any | no adopt if no binding | yes |
        // | R6 | legacy + registered process | any | settled/required | stable/pending | none | yes |
        //
        // R1 is covered by `ensure_session_rehydrates_from_repo_after_cache_loss`;
        // R5 follows the same guarded branch and existing binding-absent cases.
        // This test generates the complete external R2/R4/R6 cross-product with an adapter
        // that fails if any Coordinator-local physical effect is attempted. An
        // Lease owner/liveness never overrides the frozen custody fact. R3 is
        // covered by local Session creation and Resource activation tests.
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(ephemeral_session_repo());
        let mut cases = Vec::new();
        for (owner_name, placement, application_contributed) in [
            (
                "application",
                awaken_session_contract::SessionRuntimePlacement::Local,
                true,
            ),
            (
                "topology",
                awaken_session_contract::SessionRuntimePlacement::Worker,
                false,
            ),
            (
                "legacy",
                awaken_session_contract::SessionRuntimePlacement::LegacyUnspecified,
                false,
            ),
        ] {
            for (lease_name, realization) in [
                ("absent", None),
                (
                    "local_live",
                    Some(awaken_session_contract::SessionRealizationLease {
                        owner: "managed-runtime/boot-1".into(),
                        runtime_incarnation: "managed-runtime/boot-1".into(),
                        epoch: 4,
                        expires_at_unix_ms: u64::MAX,
                    }),
                ),
                (
                    "worker_live",
                    Some(awaken_session_contract::SessionRealizationLease {
                        owner: "worker-a".into(),
                        runtime_incarnation: "worker-a/boot-1".into(),
                        epoch: 4,
                        expires_at_unix_ms: u64::MAX,
                    }),
                ),
                (
                    "worker_expired",
                    Some(awaken_session_contract::SessionRealizationLease {
                        owner: "worker-a".into(),
                        runtime_incarnation: "worker-a/boot-1".into(),
                        epoch: 4,
                        expires_at_unix_ms: 0,
                    }),
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
                                .prepare(&id, sample_inputs())
                                .expect("pending Worker Resource projection");
                        }
                        persisted.environment.set_resident("worker-opaque-binding");
                        let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
                            &mut persisted.baseline
                        else {
                            unreachable!("sample baseline is frozen")
                        };
                        baseline.environment.self_hosted = false;
                        baseline.runtime_placement = placement;
                        baseline.application = application_contributed.then(|| {
                            awaken_session_contract::ApplicationContributionReceipt::from_input(
                                "worker-plan".into(),
                                &Default::default(),
                            )
                        });
                        persisted.realization = realization.clone();
                        create_session_fixture(repo.as_ref(), DEFAULT_SCOPE, persisted).await;
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
        let application = awaken_session_application::SessionApplication::new_with_configuration(
            runtime.clone(),
            runtime,
            repo,
            Arc::new(crate::routes::environments::EnvironmentExecutionState::default()),
            awaken_session_application::SessionApplicationConfiguration {
                execution_placement:
                    awaken_session_application::SessionExecutionPlacement::RegisteredWorker,
            },
        );
        let restarted = ManagedState::from_application(application);

        assert_eq!(restarted.reconcile_mcp_attachments().await, 0, "R2-R6/E4");
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
                    .application
                    .session_repository()
                    .get(id)
                    .await
                    .unwrap()
                    .mcp
                    .needs_reconciliation(),
                *mcp_recovery,
                "{id}/E3: Coordinator must not mutate Worker MCP state"
            );
            assert_eq!(
                restarted
                    .application
                    .session_repository()
                    .get(id)
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
        assert_eq!(restored_runtimes.lock().unwrap().len(), 48, "R2-R6/E1");
    }
}
