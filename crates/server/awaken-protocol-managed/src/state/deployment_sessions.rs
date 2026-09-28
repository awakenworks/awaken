// @formal entrypoint session_creation.repository_receipt_is_the_only_post_create_effect_gate
//! DeploymentRun idempotency at the existing Managed Session boundary.

use super::*;

const DEPLOYMENT_RUN_ID: &str = "awaken.deployment_run_id";

impl ManagedState {
    /// Create or replay the one Session owned by a durable DeploymentRun.
    ///
    /// The DeploymentRun is already the command identity, so this seam derives a
    /// stable Session id and lowers the exact launch fingerprint into the
    /// repository's existing create receipt. No metadata identity, launch table,
    /// or remote-only state machine is needed.
    pub async fn create_deployment_session_with_initial_events(
        self: &Arc<Self>,
        deployment_run_id: &str,
        launch_fingerprint: &str,
        mut req: SessionCreateParams,
        workspace_id: String,
    ) -> Result<Session, StateError> {
        if deployment_run_id.trim().is_empty() || launch_fingerprint.trim().is_empty() {
            return Err(StateError::Run(RunError::bad_request(
                "deployment_run_id and launch fingerprint are required",
            )));
        }
        let session_id = format!(
            "sesn_{}",
            awaken_agent_contract::stable_fingerprint(&("deployment-run", deployment_run_id))
        );
        let idempotency = awaken_session_state_contract::IdempotencyRecord {
            key: format!("managed:deployment-session:{session_id}"),
            payload_hash: launch_fingerprint.to_owned(),
        };
        if let Some(persisted) = self
            .application
            .replay_session_create(
                &workspace_id,
                &awaken_session_types::SessionId::from(&session_id),
                &idempotency,
            )
            .await
            .map_err(Self::map_create_replay_error)?
        {
            return self.project_replayed_session(&session_id, persisted).await;
        }

        req.metadata
            .insert(DEPLOYMENT_RUN_ID.into(), deployment_run_id.to_owned());
        // Deployment initial Events have the wider official union (including
        // `system.message`) while public Session creation intentionally admits
        // only User messages and Outcomes. The same create owner validates and
        // installs this lowered union in the original Session root.
        let initial_events = std::mem::take(&mut req.initial_events);
        self.create_session_with_identity_from(
            req,
            &workspace_id,
            Some(session_id),
            Some(idempotency),
            Some(initial_events),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{RehydrateFake, ephemeral_session_repo};
    use async_trait::async_trait;
    use awaken_deployment_application::{
        DeploymentAgent, DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher,
    };
    use std::collections::BTreeMap;

    struct UnavailableEnvironmentSource;

    #[async_trait]
    impl awaken_executable_environment_contract::ExecutableEnvironmentRegistrationSource
        for UnavailableEnvironmentSource
    {
        async fn current_registration(
            &self,
            _environment_id: &str,
        ) -> Result<
            Option<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
            awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
        > {
            Err(
                awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError::Unavailable(
                    "catalog offline".into(),
                ),
            )
        }

        async fn registration_at_revision(
            &self,
            _environment_id: &str,
            _revision: awaken_environment_contract::EnvironmentRevision,
        ) -> Result<
            Option<awaken_executable_environment_contract::ExecutableEnvironmentRegistration>,
            awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
        > {
            Err(
                awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError::Unavailable(
                    "catalog offline".into(),
                ),
            )
        }
    }

    #[tokio::test]
    async fn deployment_launch_replays_only_the_exact_repository_receipt() {
        // Deployment-create cause/effect graph. C1 a DeploymentRun Session is
        // absent/present; C2 owner matches/differs; C3 launch fingerprint
        // matches/differs. Effects: E1 first launch creates one Session and
        // retains the run id as business metadata; E2 exact replay returns the
        // same durable root without replacing request fields; E3 a different
        // launch or owner is an idempotency mismatch; E4 no private launch
        // fingerprint is stored in mutable Session metadata.
        //
        // | Rule | Root | Owner | Launch | Effect |
        // |---|---|---|---|---|
        // | D1 | absent | exact | exact | E1+E4 |
        // | D2 | present | exact | exact | E2+E4 |
        // | D3 | present | exact | different | E3 |
        // | D4 | present | different | exact | E3 |
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo());
        let state =
            Arc::new(ManagedState::new(RehydrateFake::default()).with_session_repo(repo.clone()));
        let request = |title: &str| {
            let mut request = SessionCreateParams::new(
                "deployment-agent",
                awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            );
            request.title = Some(title.into());
            request.initial_events = vec![InboundEvent::UserMessage {
                content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                    "launch",
                )],
            }];
            request
        };

        let created = state
            .create_deployment_session_with_initial_events(
                "deployment-run-receipt",
                "launch-a",
                request("first"),
                "workspace-a".into(),
            )
            .await
            .expect("D1/E1");
        assert_eq!(
            created.metadata.get(DEPLOYMENT_RUN_ID).map(String::as_str),
            Some("deployment-run-receipt"),
            "D1/E1"
        );
        assert_eq!(created.metadata.len(), 1, "D1/E4");

        drop(state);
        let state =
            Arc::new(ManagedState::new(RehydrateFake::default()).with_session_repo(repo.clone()));
        assert!(state.list_sessions().is_empty(), "D2 cold cache");
        let replayed = state
            .create_deployment_session_with_initial_events(
                "deployment-run-receipt",
                "launch-a",
                request("must not replace"),
                "workspace-a".into(),
            )
            .await
            .expect("D2/E2");
        assert_eq!(replayed.id, created.id, "D2/E2");
        assert_eq!(replayed.title, created.title, "D2/E2");
        assert_eq!(replayed.status, created.status, "D2/E2");
        assert_eq!(state.list_sessions().len(), 1, "D2/E2");

        for (rule, owner, launch) in [
            ("D3", "workspace-a", "launch-b"),
            ("D4", "workspace-b", "launch-a"),
        ] {
            assert!(
                matches!(
                    state
                        .create_deployment_session_with_initial_events(
                            "deployment-run-receipt",
                            launch,
                            request("conflicting"),
                            owner.into(),
                        )
                        .await,
                    Err(StateError::IdempotencyMismatch)
                ),
                "{rule}/E3"
            );
        }
        assert_eq!(
            repo.get(&awaken_session_types::SessionId::from(created.id))
                .await
                .unwrap()
                .title
                .as_deref(),
            Some("first")
        );
    }

    #[tokio::test]
    async fn deployment_launch_preserves_missing_and_unavailable_environment_outcomes() {
        // FMECA: collapsing a missing Environment and an unavailable catalog
        // would either retry a terminal authoring error forever or discard a
        // recoverable Deployment run. Cause/effect graph: C1=registration present, C2=registration absent,
        // C3=catalog read fails (mutually exclusive). Effects are E1=Some,
        // E2=None, E3=typed error. Decision rules exercised here are R2 C2→E2
        // and R3 C3→E3; R1 is covered by Environment execution conformance.
        // This distinction lets Deployment make absence terminal while retaining
        // a pending run for an indeterminate catalog outage.
        let request = |environment_id: &str| DeploymentLaunch {
            deployment_id: "depl_a".into(),
            deployment_run_id: "deprun_a".into(),
            workspace_id: "workspace_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            environment_id: environment_id.into(),
            metadata: BTreeMap::new(),
            initial_events: Vec::new(),
            resources: Vec::new(),
            vault_ids: Vec::new(),
            budget_max_list_cost_minor: None,
        };
        let missing = Arc::new(ManagedState::new(RehydrateFake::default()));
        let missing_launcher = crate::ManagedDeploymentSessionLauncher::new(missing);
        assert!(
            matches!(
                DeploymentSessionLauncher::launch(&missing_launcher, request("env_missing"))
                    .await,
                DeploymentLaunchOutcome::Failed {
                    error: awaken_deployment_application::DeploymentRunFailure::EnvironmentNotFoundError { .. }
                }
            ),
            "R2"
        );

        let unavailable = Arc::new(
            ManagedState::new(RehydrateFake::default()).with_environments(Arc::new(
                awaken_environment_execution_application::EnvironmentExecutionApplication::new(
                    Arc::new(awaken_work_store::InMemoryWorkQueue::new()),
                    Arc::new(UnavailableEnvironmentSource),
                ),
            )),
        );
        let unavailable_launcher = crate::ManagedDeploymentSessionLauncher::new(unavailable);
        assert!(
            matches!(
                DeploymentSessionLauncher::launch(&unavailable_launcher, request("env_a"))
                    .await,
                DeploymentLaunchOutcome::Unavailable { message }
                    if message.contains("catalog offline")
            ),
            "R3"
        );
    }

    #[tokio::test]
    async fn deployment_launch_never_downgrades_an_unrepresentable_agent_version_to_latest() {
        // FMECA/cause-effect rule: C1 authored Agent revision exceeds the wire
        // projection range; E1 launch fails before Session creation. Decision
        // D1: C1 => E1. Falling back to the latest revision would execute a
        // different immutable Agent than the Deployment selected.
        let state = Arc::new(ManagedState::new(RehydrateFake::default()));
        let launcher = crate::ManagedDeploymentSessionLauncher::new(state);
        let request = DeploymentLaunch {
            deployment_id: "depl_version".into(),
            deployment_run_id: "deprun_version".into(),
            workspace_id: "workspace_a".into(),
            agent: DeploymentAgent::new("agent_a", u64::from(u32::MAX) + 1),
            environment_id: awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID.into(),
            metadata: BTreeMap::new(),
            initial_events: Vec::new(),
            resources: Vec::new(),
            vault_ids: Vec::new(),
            budget_max_list_cost_minor: None,
        };

        assert!(matches!(
            DeploymentSessionLauncher::launch(&launcher, request).await,
            DeploymentLaunchOutcome::Failed {
                error: awaken_deployment_application::DeploymentRunFailure::SessionCreationRejectedError { message }
            } if message.contains("exceeds the Managed Session protocol range")
        ));
    }
}
