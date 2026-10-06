// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This source implements the repository-controlled session product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Public Session-create retry identity.
//!
//! The repository remains the only payload-match/replay authority. This module
//! only lowers an external owner-scoped idempotency key into the stable opaque
//! Session identity needed to address that existing receipt.

use super::{ManagedState, StateError};
use crate::types::resource::ResourceInput;
use crate::types::{AgentRef, AgentRefObject, ModelEffortInput, Session, SessionCreateParams};

/// Derive the one public Session address selected by an owner-scoped create
/// idempotency key.
///
/// This is an address prediction only. The Managed Session repository remains
/// the payload-match and replay authority, and callers must still create or
/// retrieve the Session through the ordinary Managed API.
#[must_use]
pub fn managed_session_id_from_idempotency(owner_scope: &str, idempotency_key: &str) -> String {
    format!(
        "sesn_{}",
        awaken_agent_contract::stable_fingerprint(&(
            "managed-session-create-idempotency",
            owner_scope,
            idempotency_key,
        ))
    )
}

fn agent_fingerprint(agent: &AgentRef) -> String {
    match agent {
        AgentRef::Id(id) => awaken_agent_contract::stable_fingerprint(&("id", id)),
        AgentRef::Object(object) => match object.as_ref() {
            AgentRefObject::Agent { id, version } => {
                awaken_agent_contract::stable_fingerprint(&("agent", id, version))
            }
            AgentRefObject::AgentWithOverrides {
                id,
                mcp_servers,
                model,
                skills,
                system,
                tools,
                version,
            } => {
                let model = model.as_ref().map(|model| match model {
                    crate::types::agent::ModelInput::Id(id) => {
                        awaken_agent_contract::stable_fingerprint(&("id", id))
                    }
                    crate::types::agent::ModelInput::Config(config) => {
                        awaken_agent_contract::stable_fingerprint(&(
                            "config",
                            &config.id,
                            config.speed,
                            config.effort.map(ModelEffortInput::resolved),
                            config.inference_geo,
                        ))
                    }
                });
                awaken_agent_contract::stable_fingerprint(&(
                    "agent_with_overrides",
                    id,
                    version,
                    mcp_servers,
                    model,
                    skills,
                    system,
                    tools,
                ))
            }
        },
    }
}

fn request_fingerprint(
    request: &SessionCreateParams,
    session_id: &str,
) -> Result<String, StateError> {
    let resources = request
        .resources
        .iter()
        .map(ResourceInput::idempotency_fingerprint)
        .collect::<Vec<_>>();
    let initial_events = crate::types::initial_event::compile_session_initial_event_plan(
        session_id,
        &request.initial_events,
    )
    .map_err(|message| StateError::Run(super::RunError::bad_request(message)))?
    .map(|plan| plan.batch);
    Ok(awaken_agent_contract::stable_fingerprint(&(
        agent_fingerprint(&request.agent),
        &request.budget,
        initial_events,
        &request.environment_id,
        &request.title,
        &request.metadata,
        &request.vault_ids,
        resources,
    )))
}

impl ManagedState {
    pub async fn create_session_idempotent(
        &self,
        req: SessionCreateParams,
        owner_scope: &str,
        idempotency_key: &str,
    ) -> Result<Session, StateError> {
        let owner_scope = owner_scope.to_owned();
        let session_id = managed_session_id_from_idempotency(&owner_scope, idempotency_key);
        let request_fingerprint = request_fingerprint(&req, &session_id)?;
        let idempotency = awaken_session_state_contract::IdempotencyRecord {
            key: format!("managed:session-create:{session_id}"),
            payload_hash: request_fingerprint,
        };
        if let Some(persisted) = self
            .roles
            .creation
            .replay_session_create(
                &owner_scope,
                &awaken_session_types::SessionId::from(&session_id),
                &idempotency,
            )
            .await
            .map_err(Self::map_create_replay_error)?
        {
            return self.project_replayed_session(&session_id, persisted).await;
        }
        Box::pin(self.create_session_with_identity(
            req,
            &owner_scope,
            Some(session_id),
            Some(idempotency),
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{RehydrateFake, ephemeral_session_repo};
    use std::sync::Arc;

    fn request(title: &str) -> SessionCreateParams {
        let mut request = SessionCreateParams::new(
            "coder",
            awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        );
        request.title = Some(title.into());
        request
    }

    #[tokio::test]
    async fn idempotent_create_rehydrates_durable_session_after_restart() {
        // Cause/effect graph: C1 the owner-scoped key is new or already durable;
        // C2 the canonical request fingerprint (including the compiled initial
        // Event plan) matches or differs; C3 the process cache is warm or cold.
        // Effects are E1 one new Session, E2 the exact durable Session is
        // rehydrated without another create, and E3 an idempotency mismatch
        // with no replacement. Decision rules covered: R1 new+matching -> E1;
        // R2 durable+matching+cold -> E2; R3 durable+different title+cold -> E3;
        // R4 durable+different initial Events+warm -> E3. Same-process and owner
        // isolation rules remain covered by the protocol adapter matrix.
        // Constraints/invariants: the owner-scoped key plus canonical request
        // fingerprint selects exactly one durable Session; cache rehydration may
        // project that truth but cannot create or replace another aggregate.
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let original = Arc::new(
            ManagedState::new(RehydrateFake::default())
                .await
                .with_session_repo(repo.clone()),
        );
        let created = original
            .create_session_idempotent(request("Project A"), "workspace-a", "issue-a")
            .await
            .expect("R1 creates the canonical Session");
        assert!(
            created.metadata.is_empty(),
            "R1 receipt identity is not duplicated into mutable metadata"
        );
        drop(original);

        let restarted = Arc::new(
            ManagedState::new(RehydrateFake::default())
                .await
                .with_session_repo(repo.clone()),
        );
        assert!(restarted.list_sessions().is_empty(), "R2 starts cold");
        let replayed = restarted
            .create_session_idempotent(request("Project A"), "workspace-a", "issue-a")
            .await
            .expect("R2 rehydrates durable truth");
        assert_eq!(replayed.id, created.id, "R2 preserves identity");
        assert_eq!(restarted.list_sessions().len(), 1, "R2 projects once");

        let mismatch = restarted
            .create_session_idempotent(request("Changed"), "workspace-a", "issue-a")
            .await;
        assert!(
            matches!(mismatch, Err(StateError::IdempotencyMismatch)),
            "R3 rejects changed input"
        );
        assert_eq!(restarted.list_sessions().len(), 1, "R3 does not replace");
        assert_eq!(
            repo.get(&awaken_session_types::SessionId::from(created.id))
                .await
                .expect("durable Session remains")
                .title
                .as_deref(),
            Some("Project A"),
            "R3 leaves durable truth unchanged"
        );

        let mut changed_initial_events = request("Project A");
        changed_initial_events.initial_events = vec![crate::types::InboundEvent::UserMessage {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "new command",
            )],
        }];
        assert!(
            matches!(
                restarted
                    .create_session_idempotent(changed_initial_events, "workspace-a", "issue-a",)
                    .await,
                Err(StateError::IdempotencyMismatch)
            ),
            "R4 rejects a different canonical initial Event plan"
        );
    }
}
