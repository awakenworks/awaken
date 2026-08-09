//! Session-owned runtime environment identity persistence.
//!
//! The runtime materializes an opaque binding, while the Session aggregate owns
//! its durable identity. This module contains only that narrow root-CAS command;
//! Environment authoring and immutable snapshot compilation remain in
//! `routes::environments`.

use super::*;

pub(crate) struct RepositoryEnvironmentBindingSink {
    repo: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
}

impl RepositoryEnvironmentBindingSink {
    pub(crate) fn new(repo: Arc<dyn awaken_session_contract::ManagedSessionRepository>) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for RepositoryEnvironmentBindingSink {
    async fn owns(&self, session_id: &str) -> bool {
        self.repo.owner(session_id).await.is_some()
    }

    async fn persist(&self, session_id: &str, binding: &str) -> Result<(), RunError> {
        for attempt in 0..ManagedState::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.repo.owner(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            let mut session = self.repo.get(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            if session.environment.binding() == Some(binding) {
                return Ok(());
            }
            session.environment.set_resident(binding);
            let expected_revision = session.revision;
            let payload = awaken_session_contract::SessionMutationPayload::Replace(session);
            let payload_hash = payload.stable_hash();
            let mutation = awaken_session_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_contract::IdempotencyRecord {
                    key: format!(
                        "managed:bind-environment-immediate:{session_id}:{}:{payload_hash}",
                        expected_revision.0
                    ),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            };
            match self
                .repo
                .commit_mutation(&owner_scope, mutation)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?
            {
                awaken_session_contract::SessionMutationResult::Applied { .. }
                | awaken_session_contract::SessionMutationResult::Replayed { .. } => return Ok(()),
                awaken_session_contract::SessionMutationResult::Conflict { .. }
                    if attempt + 1 < ManagedState::ROOT_CAS_ATTEMPTS => {}
                awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                    return Err(RunError::internal(
                        "Session environment binding CAS exhausted",
                    ));
                }
                awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                    return Err(RunError::internal(
                        "Session environment binding idempotency mismatch",
                    ));
                }
            }
        }
        Err(RunError::internal(
            "Session environment binding CAS exhausted",
        ))
    }
}

impl ManagedState {
    pub(crate) async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<crate::env_registry::EnvItem>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        self.environments.get(environment_id).await
    }

    pub(crate) fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    pub(crate) fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.config_source
            .as_ref()
            .and_then(|source| source.unavailable_delegate_in(workspace_id, agent_id))
    }

    /// Resolve one exact executable Environment snapshot for a new Session.
    ///
    /// An explicit Session selection follows the current Environment revision;
    /// an Agent publication binding follows its exact immutable revision. Both
    /// paths apply the current availability deny overlay in the execution
    /// catalog before the Session baseline is committed.
    pub(crate) async fn resolve_session_environment(
        &self,
        requested_environment_id: Option<&str>,
        published_environment: Option<
            &awaken_executable_agent_contract::ExecutableAgentEnvironment,
        >,
        published_backend_ref: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<(String, awaken_session_contract::EnvironmentSnapshot, bool), StateError> {
        let environment_id = requested_environment_id
            .map(str::to_owned)
            .or_else(|| published_environment.map(|binding| binding.environment_id.clone()))
            .unwrap_or_else(|| "env_local".to_string());
        let snapshot = match (requested_environment_id, published_environment) {
            (None, Some(binding)) => {
                self.environments
                    .resolve_exact_for_session(
                        &binding.environment_id,
                        binding.revision,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
            _ => {
                self.environments
                    .resolve_current_for_session(
                        &environment_id,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
        }
        .map_err(|error| StateError::Run(RunError::unavailable(error.to_string())))?
        .ok_or_else(|| {
            StateError::Run(RunError::bad_request(format!(
                "environment `{environment_id}` is unavailable"
            )))
        })?;
        super::sandbox_provisioning::validate_sandbox_provisioning_runtime(
            snapshot.snapshot.sandbox_provisioning,
            published_backend_ref,
        )
        .map_err(StateError::Run)?;
        Ok((environment_id, snapshot.snapshot, snapshot.self_hosted))
    }
}
