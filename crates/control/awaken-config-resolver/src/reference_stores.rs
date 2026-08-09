//! In-memory reference adapters for tests and local development.
//!
//! Production composition injects durable adapters for the ports in
//! [`crate::stores`]. Keeping these process-local implementations separate makes
//! the application contracts visible without presenting a second persistence
//! model as part of the production read side.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::stores::{
    AgentInputBindingRepository, AgentInputRepositoryError, ConfigRepositoryError,
    InferenceProfileStore, WebhookAuthoringPatch, WebhookAuthoringState, WebhookDeliveryOutcome,
    WebhookDeliveryState, WebhookMutationIntent, WebhookStore, validate_agent_input_revision,
};
use crate::{AgentInputConfig, InferenceProfile, WebhookEndpointDef};

/// Process-local reference implementation of [`InferenceProfileStore`].
#[derive(Default)]
pub struct InMemoryProfileStore(Mutex<HashMap<String, InferenceProfile>>);

impl InMemoryProfileStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InferenceProfileStore for InMemoryProfileStore {
    fn put(&self, id: String, profile: InferenceProfile) -> Result<(), ConfigRepositoryError> {
        self.0
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("profile store mutex poisoned".into()))?
            .insert(id, profile);
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("profile store mutex poisoned".into()))?
            .get(id)
            .cloned())
    }
}

/// Process-local reference implementation of [`WebhookStore`].
#[derive(Default)]
pub struct InMemoryWebhookStore {
    state: Mutex<InMemoryWebhookState>,
}

#[derive(Default)]
struct InMemoryWebhookState {
    endpoints: HashMap<String, WebhookEndpointDef>,
    mutations: HashMap<String, WebhookMutationIntent>,
}

impl InMemoryWebhookStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WebhookStore for InMemoryWebhookStore {
    fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?
            .endpoints
            .get(id)
            .cloned())
    }

    fn list(&self, workspace_id: &str) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError> {
        let mut rows: Vec<WebhookEndpointDef> = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?
            .endpoints
            .values()
            .filter(|definition| definition.workspace_id == workspace_id)
            .cloned()
            .collect();
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(rows)
    }

    fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?;
        if state.mutations.contains_key(&patch.id) {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {} has a pending material mutation",
                patch.id
            )));
        }
        let Some(definition) = state.endpoints.get_mut(&patch.id) else {
            return Ok(WebhookAuthoringState::Missing);
        };
        if definition.workspace_id != patch.workspace_id {
            return Ok(WebhookAuthoringState::OwnerMismatch);
        }
        definition.url = patch.url;
        definition.event_types = patch.event_types;
        if let Some(disabled) = patch.disabled {
            definition.disabled = disabled;
            if !disabled {
                definition.consecutive_failures = 0;
            }
        }
        Ok(WebhookAuthoringState::Updated(definition.clone()))
    }

    fn begin_mutation(&self, intent: WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?;
        if let Some(pending) = state.mutations.get(&id) {
            return if pending == &intent {
                Ok(())
            } else {
                Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} already has a pending mutation"
                )))
            };
        }
        if state.endpoints.get(&id) != intent.before.as_ref() {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} changed before mutation admission"
            )));
        }
        state.mutations.insert(id, intent);
        Ok(())
    }

    fn apply_mutation(&self, intent: &WebhookMutationIntent) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?.to_string();
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?;
        if state.mutations.get(&id) != Some(intent)
            || state.endpoints.get(&id) != intent.before.as_ref()
        {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} no longer matches its pending mutation"
            )));
        }
        match &intent.after {
            Some(after) => {
                state.endpoints.insert(id, after.clone());
            }
            None => {
                state.endpoints.remove(&id);
            }
        }
        Ok(())
    }

    fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError> {
        let mut intents: Vec<_> = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?
            .mutations
            .values()
            .cloned()
            .collect();
        intents.sort_by(|left, right| left.id().ok().cmp(&right.id().ok()));
        Ok(intents)
    }

    fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError> {
        let id = intent.id()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?;
        match state.mutations.get(id) {
            None => return Ok(()),
            Some(pending) if pending == intent => {}
            Some(_) => {
                return Err(ConfigRepositoryError::MutationConflict(format!(
                    "webhook {id} has a different pending mutation"
                )));
            }
        }
        state.mutations.remove(id);
        Ok(())
    }

    fn material_refs(
        &self,
    ) -> Result<Vec<awaken_credential_vault::SecretRef>, ConfigRepositoryError> {
        let mut refs: Vec<_> = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?
            .endpoints
            .values()
            .map(|definition| definition.secret_ref.clone())
            .collect();
        refs.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(refs)
    }

    fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConfigRepositoryError::Storage("webhook store mutex poisoned".into()))?;
        if state.mutations.contains_key(id) {
            return Err(ConfigRepositoryError::MutationConflict(format!(
                "webhook {id} has a pending material mutation"
            )));
        }
        Ok(state
            .endpoints
            .get_mut(id)
            .map_or(WebhookDeliveryState::Missing, |definition| {
                definition.record_delivery(outcome, failure_threshold)
            }))
    }
}

/// Process-local reference implementation of [`AgentInputBindingRepository`].
#[derive(Default)]
pub struct InMemoryAgentInputBindingRepository(Mutex<HashMap<(String, String), AgentInputConfig>>);

impl InMemoryAgentInputBindingRepository {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl AgentInputBindingRepository for InMemoryAgentInputBindingRepository {
    fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError> {
        let key = (workspace_id.to_string(), config.agent_id.clone());
        let mut rows = self.0.lock().map_err(|_| {
            AgentInputRepositoryError::Storage("agent input repository mutex poisoned".into())
        })?;
        if !validate_agent_input_revision(rows.get(&key), &config)? {
            return Ok(());
        }
        rows.insert(key, config);
        Ok(())
    }

    fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| {
                AgentInputRepositoryError::Storage("agent input repository mutex poisoned".into())
            })?
            .get(&(workspace_id.to_string(), agent_id.to_string()))
            .cloned())
    }

    fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError> {
        let mut configs: Vec<_> = self
            .0
            .lock()
            .map_err(|_| {
                AgentInputRepositoryError::Storage("agent input repository mutex poisoned".into())
            })?
            .iter()
            .filter(|((workspace, _), _)| workspace == workspace_id)
            .map(|(_, config)| config.clone())
            .collect();
        configs.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        Ok(configs)
    }
}

#[cfg(test)]
mod profile_tenancy_tests {
    //! Cause graph: C1 same public profile id; C2 different trusted Workspaces;
    //! C3 rows are written through the Workspace-key helper. E1 both values remain
    //! independently readable; E2 a Workspace cannot observe the other's value.
    //!
    //! Decision table:
    //! | Rule | C1 | C2 | C3 | Effect |
    //! | T1   | Y  | Y  | Y  | E1+E2 |
    //! | T2   | Y  | N  | Y  | ordinary overwrite |

    use awaken_credential_vault::CredentialBinding;

    use super::*;
    use crate::{ModelTarget, ProfileCandidate, get_workspace_profile, put_workspace_profile};

    fn profile(workspace: &str, model: &str) -> InferenceProfile {
        InferenceProfile {
            workspace_id: workspace.into(),
            primary: ProfileCandidate {
                target: ModelTarget::unqualified(model),
                credential_binding: CredentialBinding::None,
            },
            fallbacks: Vec::new(),
            disabled_endpoint_ids: Vec::new(),
        }
    }

    #[test]
    fn t1_common_default_id_is_isolated_by_workspace() {
        let store = InMemoryProfileStore::new();
        put_workspace_profile(
            &store,
            "workspace-a",
            "shared-profile",
            profile("workspace-a", "a"),
        )
        .unwrap();
        put_workspace_profile(
            &store,
            "workspace-b",
            "shared-profile",
            profile("workspace-b", "b"),
        )
        .unwrap();

        assert_eq!(
            get_workspace_profile(&store, "workspace-a", "shared-profile")
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "a"
        );
        assert_eq!(
            get_workspace_profile(&store, "workspace-b", "shared-profile")
                .unwrap()
                .unwrap()
                .primary
                .target
                .model_id,
            "b"
        );
    }
}
