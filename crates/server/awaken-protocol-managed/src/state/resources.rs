//! Live-session typed input CRUD and durable resource activation coordination.
//! The persisted [`SessionResourceState`](awaken_session_contract::SessionResourceState)
//! is authoritative; runtime mounts and wire DTOs are projections of its active
//! manifest.

use super::*;

impl ManagedState {
    /// ResourceReclaimer entry point. Composition roots call this after durable
    /// stores and the Runtime Host are wired. It scans only Session application
    /// state; authorization principals and policy objects never cross this seam.
    pub async fn reconcile_resource_activations(&self) -> usize {
        let report = self.application.reconcile_resource_activations().await;
        for session in &report.settled {
            if let Err(error) = self.refresh_cached_projection(session) {
                tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session Resource wire projection refresh remains pending"
                );
            }
        }
        for failure in report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session resource reconciliation remains pending"
            );
        }
        report.settled.len()
    }

    pub(super) fn map_preparation_error(
        error: awaken_session_application::SessionPreparationError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionPreparationError::NotFound => StateError::NotFound,
            awaken_session_application::SessionPreparationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionPreparationError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionPreparationError::Unavailable(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    fn resolve_live_file_input(
        &self,
        owner_scope: &str,
        binding_id: String,
        parsed: &ParsedSessionInput,
    ) -> Result<awaken_session_contract::ResolvedInput, StateError> {
        debug_assert!(matches!(parsed.target, ParsedInputTarget::File(_)));
        let binding = input_binding(binding_id, parsed, None);
        let attachment = awaken_session_contract::SessionInputAttachment {
            binding,
            replaces: None,
        };
        awaken_session_contract::SessionInputResolver::resolve_inputs(
            owner_scope,
            None,
            &[],
            &[attachment],
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?
        .inputs
        .pop()
        .ok_or_else(|| StateError::Run(RunError::internal("resolved input is empty")))
    }

    pub fn list_resources(
        &self,
        id: &str,
    ) -> Result<Vec<crate::types::resource::SessionResource>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record
            .resource_state
            .active
            .inputs
            .iter()
            .map(|input| resolved_resource_dto(id, input))
            .collect())
    }

    pub async fn create_resource(
        &self,
        id: &str,
        body: crate::types::resource::ResourceAddParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let parsed = body.into_resource_input().to_parsed_input();
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .session(id)
            .await
            .map_err(StateError::from)?;
        let current = persisted.resources.desired().clone();
        if matches!(parsed.target, ParsedInputTarget::File(_))
            && current
                .inputs
                .iter()
                .filter(|input| {
                    matches!(
                        input.source,
                        awaken_session_contract::ResolvedInputSource::File { .. }
                    )
                })
                .count()
                >= super::resource::MAX_SESSION_FILE_RESOURCES
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {} files",
                super::resource::MAX_SESSION_FILE_RESOURCES
            ))));
        }
        let mut suffix = current.inputs.len();
        let binding_id = loop {
            let candidate = format!("session:{id}:live:{suffix}");
            if current
                .inputs
                .iter()
                .all(|input| input.binding_id.as_str() != candidate)
            {
                break candidate;
            }
            suffix += 1;
        };
        current
            .validate_new_binding(
                &awaken_resource_contract::BindingId::from(binding_id.clone()),
                &parsed.mount_path,
            )
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        let input = self.resolve_live_file_input(&owner_scope, binding_id, &parsed)?;
        let committed = self
            .application
            .attach_session_input(id, &owner_scope, input.clone())
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&committed)?;
        Ok(resolved_resource_dto(id, &input))
    }

    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        record
            .resource_state
            .active
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .map(|input| resolved_resource_dto(id, input))
            .ok_or(StateError::NotFound)
    }

    pub async fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: crate::types::resource::ResourceUpdateParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .rotate_repository_credential(
                id,
                &owner_scope,
                &binding_id,
                patch.authorization_token.into_redacted(),
            )
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&persisted)?;
        let input = persisted
            .resources
            .active
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .ok_or(StateError::NotFound)?;
        Ok(resolved_resource_dto(id, input))
    }

    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .application
            .session(id)
            .await
            .map_err(StateError::from)?;
        let input = persisted
            .resources
            .active
            .inputs
            .iter()
            .find(|input| input.binding_id == binding_id)
            .cloned()
            .ok_or(StateError::NotFound)?;
        if matches!(
            input.source,
            awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let committed = self
            .application
            .detach_session_input(id, &owner_scope, &binding_id)
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&committed)?;
        Ok(())
    }
}
