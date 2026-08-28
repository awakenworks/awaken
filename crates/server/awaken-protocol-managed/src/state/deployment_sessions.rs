//! DeploymentRun idempotency at the existing Managed Session boundary.

use super::*;

const DEPLOYMENT_RUN_ID: &str = "awaken.deployment_run_id";
const DEPLOYMENT_LAUNCH_FINGERPRINT: &str = "awaken.deployment_launch_fingerprint";

impl ManagedState {
    /// Create or replay the one Session owned by a durable DeploymentRun.
    ///
    /// The DeploymentRun is already the command identity, so this seam derives a
    /// stable Session id and stores the exact launch fingerprint in the ordinary
    /// Session aggregate. No launch receipt table or remote-only state machine is
    /// needed. A response-loss retry therefore reads the canonical Session; reuse
    /// of the run id with another payload or owner fails closed.
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
            awaken_session_contract::stable_fingerprint(&("deployment-run", deployment_run_id))
        );
        if let Some(session) = self
            .replay_deployment_session(
                &session_id,
                deployment_run_id,
                launch_fingerprint,
                &workspace_id,
            )
            .await?
        {
            return Ok(session);
        }

        req.metadata
            .insert(DEPLOYMENT_RUN_ID.into(), deployment_run_id.to_owned());
        req.metadata.insert(
            DEPLOYMENT_LAUNCH_FINGERPRINT.into(),
            launch_fingerprint.to_owned(),
        );
        // Deployment initial Events have the wider official union (including
        // `system.message`) while public Session creation intentionally admits
        // only User messages and Outcomes. The same create owner validates and
        // installs this lowered union in the original Session root.
        let initial_events = std::mem::take(&mut req.initial_events);
        let created = self
            .create_session_with_identity_from(
                req,
                Some(workspace_id.clone()),
                Some(session_id.clone()),
                Some(initial_events),
                false,
            )
            .await;
        match created {
            Ok(session) => Ok(session),
            Err(error) => {
                // A peer may have won the deterministic Session insert between
                // the read above and our create. Re-read only the same canonical
                // aggregate and accept it only when owner and payload match.
                if let Some(session) = self
                    .replay_deployment_session(
                        &session_id,
                        deployment_run_id,
                        launch_fingerprint,
                        &workspace_id,
                    )
                    .await?
                {
                    return Ok(session);
                }
                Err(error)
            }
        }
    }

    async fn replay_deployment_session(
        self: &Arc<Self>,
        session_id: &str,
        deployment_run_id: &str,
        launch_fingerprint: &str,
        workspace_id: &str,
    ) -> Result<Option<Session>, StateError> {
        self.replay_session_with_metadata(
            session_id,
            workspace_id,
            &[
                (DEPLOYMENT_RUN_ID, deployment_run_id),
                (DEPLOYMENT_LAUNCH_FINGERPRINT, launch_fingerprint),
            ],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::RehydrateFake;
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
