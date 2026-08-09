//! DeploymentRun idempotency at the existing Managed Session boundary.

use super::*;

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

        req.metadata.insert(
            "awaken.deployment_run_id".into(),
            deployment_run_id.to_owned(),
        );
        req.metadata.insert(
            "awaken.deployment_launch_fingerprint".into(),
            launch_fingerprint.to_owned(),
        );
        let initial_events = req.initial_events.clone();
        let created = self
            .create_session_with_identity(req, Some(workspace_id.clone()), Some(session_id.clone()))
            .await;
        let mut session = match created {
            Ok(session) => session,
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
                return Err(error);
            }
        };
        if !initial_events.is_empty() {
            self.start_initial_events(&session.id, initial_events)?;
            session.status = "running";
        }
        Ok(session)
    }

    async fn replay_deployment_session(
        self: &Arc<Self>,
        session_id: &str,
        deployment_run_id: &str,
        launch_fingerprint: &str,
        workspace_id: &str,
    ) -> Result<Option<Session>, StateError> {
        let Some(persisted) = self.application.session_repository().get(session_id).await else {
            return Ok(None);
        };
        let owner = self
            .application
            .session_repository()
            .owner(session_id)
            .await;
        if owner.as_deref() != Some(workspace_id)
            || persisted
                .metadata
                .get("awaken.deployment_run_id")
                .is_none_or(|value| value != deployment_run_id)
            || persisted
                .metadata
                .get("awaken.deployment_launch_fingerprint")
                .is_none_or(|value| value != launch_fingerprint)
        {
            return Err(StateError::IdempotencyMismatch);
        }
        self.ensure_session(session_id).await?;
        self.get_session(session_id).map(Some)
    }
}
