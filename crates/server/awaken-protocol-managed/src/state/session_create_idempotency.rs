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
        let session_id = match &self.private_binding {
            Some(binding) => format!(
                "sesn_{}",
                awaken_agent_contract::stable_fingerprint(&(
                    "managed-private-session-create-idempotency",
                    binding,
                    idempotency_key,
                ))
            ),
            None => managed_session_id_from_idempotency(&owner_scope, idempotency_key),
        };
        let request_fingerprint = request_fingerprint(&req, &session_id)?;
        let idempotency = awaken_session_application::SessionCreateReceipt::ClientRequest
            .record(&session_id, request_fingerprint);
        let replay = match &self.private_binding {
            Some(binding) => {
                self.roles
                    .creation
                    .replay_private_session_create(
                        binding,
                        &awaken_session_types::SessionId::from(&session_id),
                        &idempotency,
                    )
                    .await
            }
            None => {
                self.roles
                    .creation
                    .replay_session_create(
                        &owner_scope,
                        &awaken_session_types::SessionId::from(&session_id),
                        &idempotency,
                    )
                    .await
            }
        }
        .map_err(Self::map_create_replay_error)?;
        if replay.is_some() {
            return self.project_replayed_session(&session_id).await;
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

    #[test]
    fn repository_token_fingerprints_preserve_original_request_identity() {
        // F1-F4 cause/effect table: absent/A/A/B token with every other create,
        // Repository and Session-id field fixed. Exact token replay matches;
        // absent/A/B are pairwise distinct at the original Resource and create
        // request functions and the manifest's original vector composition.
        // All serialized Resource DTOs still omit the write-only token. The
        // literals below were captured by this unchanged Rust case on native
        // Windows/MSVC at 2025b27345 before the conversion-owner move; all 17
        // original producer/dependency sources were byte-exact to old886.
        // These are request-equivalence witnesses, not authorization proofs.
        let mut fingerprints = Vec::new();
        for token in [
            None,
            Some("fixture-token-a"),
            Some("fixture-token-a"),
            Some("fixture-token-b"),
        ] {
            let mut wire = serde_json::json!({
                "type": "github_repository",
                "url": "https://github.com/acme/repo.git",
                "mount_path": "/workspace/source",
                "checkout": {"type": "branch", "name": "main"}
            });
            if let Some(token) = token {
                wire["authorization_token"] = serde_json::json!(token);
            }
            let mut request = request("Token identity fixture");
            request.resources = vec![serde_json::from_value(wire).unwrap()];
            let resource = request.resources[0].idempotency_fingerprint();
            let manifest = awaken_agent_contract::stable_fingerprint(&vec![resource.clone()]);
            let create = request_fingerprint(&request, "sesn_token_identity_fixture")
                .expect("valid old request fingerprint");
            println!("baseline resource={resource} manifest={manifest} create={create}");
            let serialized = serde_json::to_string(&request.resources[0]).unwrap();
            assert!(!serialized.contains("authorization_token"));
            assert!(!serialized.contains("fixture-token"));
            fingerprints.push([resource, manifest, create]);
        }
        let original = [
            [
                "fnv1a64:05842ba748cc3c35",
                "fnv1a64:fa623d0f7b51fbda",
                "fnv1a64:34ec6737c6b8a722",
            ],
            [
                "fnv1a64:e7582c6ec9d567ff",
                "fnv1a64:fd4de969c7c0d6e2",
                "fnv1a64:cccb5c884f27071e",
            ],
            [
                "fnv1a64:e7582c6ec9d567ff",
                "fnv1a64:fd4de969c7c0d6e2",
                "fnv1a64:cccb5c884f27071e",
            ],
            [
                "fnv1a64:192aacb29c366dc6",
                "fnv1a64:76081503bf2c2418",
                "fnv1a64:84ace51ad98c405c",
            ],
        ];
        assert_eq!(
            fingerprints,
            original.map(|row| row.map(str::to_owned)).to_vec(),
            "F1-F4 exact original Rust literals"
        );
        for (((absent, first), replay), different) in fingerprints[0]
            .iter()
            .zip(&fingerprints[1])
            .zip(&fingerprints[2])
            .zip(&fingerprints[3])
        {
            assert_eq!(first, replay, "F2=F3 exact token replay");
            assert_ne!(absent, first, "F1!=F2 absent/present");
            assert_ne!(absent, different, "F1!=F4 absent/different");
            assert_ne!(first, different, "F2!=F4 token-only change");
        }
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
        let repo: Arc<dyn awaken_session_application::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let original = Arc::new({
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
        });
        let created = original
            .create_session_idempotent(request("Project A"), "workspace-a", "issue-a")
            .await
            .expect("R1 creates the canonical Session");
        assert!(
            created.metadata.is_empty(),
            "R1 receipt identity is not duplicated into mutable metadata"
        );
        drop(original);

        let restarted = Arc::new({
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
        });
        assert!(
            restarted.list_sessions().unwrap().is_empty(),
            "R2 starts cold"
        );
        let replayed = restarted
            .create_session_idempotent(request("Project A"), "workspace-a", "issue-a")
            .await
            .expect("R2 rehydrates durable truth");
        assert_eq!(replayed.id, created.id, "R2 preserves identity");
        assert_eq!(
            restarted.list_sessions().unwrap().len(),
            1,
            "R2 projects once"
        );

        let mismatch = restarted
            .create_session_idempotent(request("Changed"), "workspace-a", "issue-a")
            .await;
        assert!(
            matches!(mismatch, Err(StateError::IdempotencyMismatch)),
            "R3 rejects changed input"
        );
        assert_eq!(
            restarted.list_sessions().unwrap().len(),
            1,
            "R3 does not replace"
        );
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
