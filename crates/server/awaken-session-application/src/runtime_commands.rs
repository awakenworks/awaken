// @formal surface requirement=runtime.pending_input_delegation.delegation_delivery
// @formal reason: This source implements the repository-controlled delegation product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Protocol-neutral Session command/query surface.
//!
//! The application owns access to Runtime and the aggregate repository. Public
//! adapters invoke these use-case methods and never reach through to either
//! collaborator, preventing a second orchestration boundary in protocol state.

use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_session_types::RunError;

/// One transitive count law; callers lend their already selected direct values.
/// The resolver owns exact/current selection, never an implicit pin fallback.
pub(super) fn validate_session_skill_total(
    queries: &crate::SessionQueries,
    workspace_id: &str,
    root_agent_id: &str,
    root_view: Option<&ExecutableAgentSessionProfile>,
    root_skills: &[awaken_agent_contract::AgentSkillBinding],
    captured: &[(String, ExecutableAgentSessionProfile)],
) -> Result<(), RunError> {
    const MAX_SESSION_SKILLS: usize = 500;
    let mut total: usize = 0;
    let mut add = |count| {
        total = total
            .checked_add(count)
            .filter(|sum| *sum <= MAX_SESSION_SKILLS)
            .ok_or_else(|| {
                RunError::bad_request("a session supports at most 500 skills across all agents")
            })?;
        Ok::<_, RunError>(())
    };
    add(root_skills.len())?;
    let mut seen = std::collections::BTreeSet::from([root_agent_id.to_string()]);
    let mut pending = root_view
        .into_iter()
        .flat_map(|view| view.delegates.iter().cloned())
        .collect::<std::collections::VecDeque<_>>();
    while let Some(delegate) = pending.pop_front() {
        if !seen.insert(delegate.agent_id.clone()) {
            continue;
        }
        let Some(view) = captured
            .iter()
            .find(|(id, _)| id == &delegate.agent_id)
            .map(|(_, selected)| selected.clone())
            .or_else(|| {
                queries.selected_session_profile(
                    workspace_id,
                    &delegate.agent_id,
                    delegate.source_revision,
                )
            })
        else {
            continue;
        };
        add(view.skills.len())?;
        pending.extend(view.delegates);
    }
    Ok(())
}

pub(super) fn session_input_error(error: crate::session_resource::SessionInputError) -> RunError {
    use awaken_resource_contract::ResourceRegistryError;
    use awaken_session_configuration_contract::resource::SessionInputError;
    match &error {
        SessionInputError::Registry(ResourceRegistryError::AuthorityUnavailable { .. })
        | SessionInputError::Registry(ResourceRegistryError::Indeterminate { .. }) => {
            RunError::unavailable(error.to_string())
        }
        SessionInputError::Registry(ResourceRegistryError::CorruptData(_)) => {
            RunError::internal(error.to_string())
        }
        _ => RunError::bad_request(error.to_string()),
    }
}

pub use awaken_session_execution_contract::{
    SessionExecutionObservation, SessionModelRequestObservation, SessionRunExecutionObservation,
    SessionToolUseObservation,
};

impl crate::SessionCredentials {
    pub async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError> {
        let Some(source) = self.credential_source() else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let exists = source
                .has_vault(workspace_id, vault_id)
                .await
                .map_err(|error| {
                    RunError::unavailable(format!("credential authority unavailable: {error}"))
                })?;
            if !exists {
                return Ok(Some(vault_id.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod input_error_tests {
    #[test]
    fn dependency_failure_is_not_reclassified_as_invalid_user_input() {
        // R1 dependency unavailable -> Unavailable; R2 corrupt persisted data
        // -> Internal; R3 invalid caller binding -> BadRequest. Identical
        // diagnostic text in every row proves the type owns the decision.
        use awaken_resource_contract::ResourceRegistryError;
        use awaken_session_configuration_contract::resource::SessionInputError;
        use awaken_session_types::RunErrorKind;
        for (error, expected) in [
            (
                SessionInputError::Registry(ResourceRegistryError::AuthorityUnavailable {
                    detail: "same".into(),
                }),
                RunErrorKind::Unavailable,
            ),
            (
                SessionInputError::Registry(ResourceRegistryError::Indeterminate {
                    detail: "same".into(),
                }),
                RunErrorKind::Unavailable,
            ),
            (
                SessionInputError::Registry(ResourceRegistryError::CorruptData("same".into())),
                RunErrorKind::Internal,
            ),
            (
                SessionInputError::InvalidBindingId("same".into()),
                RunErrorKind::BadRequest,
            ),
        ] {
            assert_eq!(super::session_input_error(error).kind, expected);
        }
    }
}
