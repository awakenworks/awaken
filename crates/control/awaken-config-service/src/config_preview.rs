//! Draft validation and ephemeral preview publication.
//!
//! This module owns the non-persistent half of [`ConfigService`]: both operations
//! compile an authored draft, but only preview registers an immutable, disposable
//! Coordinator projection. Durable authoring and publication remain in
//! `config_service`.

use awaken_config_resolver::AgentInputConfig;
use awaken_config_store::{AgentConfig, AgentConfigRevision};
use awaken_executable_agent_contract::ExecutableAgentRegistration;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use crate::agent_projection::registered_session_profile;
use crate::config_service::ConfigService;
use crate::credential_reference::validate_credential_references;
use crate::plugin_validation::resolve_plugin_configuration;
use crate::publication::{
    PublishError, ValidationIssue, prepare_agent_publication, snapshot_metadata,
};

impl ConfigService {
    /// Validate a config by compiling it against the caller-supplied tool
    /// `catalog`; this is the write-free form of publication.
    pub async fn validate(
        &self,
        workspace: &ScopeId,
        config: &AgentConfig,
        catalog: &[ToolDescriptor],
    ) -> Result<(), ValidationIssue> {
        let mut resolved = prepare_agent_publication(
            self.model_publication_resolver.as_ref(),
            workspace,
            AgentConfigRevision {
                config: config.clone(),
                revision: 0,
            },
        )
        .await
        .map_err(|error| ValidationIssue {
            path: "model".to_string(),
            message: error.to_string(),
        })?;
        resolve_plugin_configuration(
            &self.plugin_publication_resolvers,
            workspace,
            &mut resolved.config,
        )
        .await?;
        validate_credential_references(
            self.credential_reference_validator.as_ref(),
            workspace,
            &resolved.config,
        )
        .await
        .map_err(|error| ValidationIssue {
            path: error.path,
            message: error.message,
        })?;
        awaken_config_store::compile_published(
            &resolved.config,
            catalog,
            snapshot_metadata(&resolved),
            resolved.models.primary,
            resolved.models.candidates,
        )
        .map(|_| ())
        .map_err(|error| ValidationIssue {
            path: error.field_path().to_string(),
            message: error.to_string(),
        })
    }

    /// Compile an unsaved draft and register it through the canonical
    /// Control-to-Coordinator boundary without creating authoring or publication
    /// records. Preview ids are immutable: a refreshed draft receives a new id.
    pub async fn preview(
        &self,
        workspace: &ScopeId,
        preview_id: &str,
        config: &AgentConfig,
        inputs: AgentInputConfig,
        catalog: &[ToolDescriptor],
    ) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, PublishError> {
        if config.id != preview_id || inputs.agent_id != preview_id {
            return Err(PublishError::Unresolvable(
                "preview config and resources must use the requested preview id".into(),
            ));
        }
        let source_revision = 1;
        let mut resolved = prepare_agent_publication(
            self.model_publication_resolver.as_ref(),
            workspace,
            AgentConfigRevision {
                config: config.clone(),
                revision: source_revision,
            },
        )
        .await?;
        resolve_plugin_configuration(
            &self.plugin_publication_resolvers,
            workspace,
            &mut resolved.config,
        )
        .await
        .map_err(|error| {
            PublishError::Unresolvable(format!("{}: {}", error.path, error.message))
        })?;
        validate_credential_references(
            self.credential_reference_validator.as_ref(),
            workspace,
            &resolved.config,
        )
        .await
        .map_err(|error| {
            PublishError::Unresolvable(format!("{}: {}", error.path, error.message))
        })?;
        let mut metadata = snapshot_metadata(&resolved);
        let mut resolved_inputs = std::mem::take(&mut metadata.resolution.inputs);
        resolved_inputs.push(awaken_runtime_contract::ResolvedInputRef {
            kind: "agent_session_defaults".into(),
            id: preview_id.to_owned(),
            version: awaken_runtime_contract::ResolvedInputVersion::Revision(
                inputs.revision as u64,
            ),
        });
        metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(resolved_inputs)
            .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
        let snapshot = awaken_config_store::compile_published(
            &resolved.config,
            catalog,
            metadata,
            resolved.models.primary,
            resolved.models.candidates,
        )
        .map_err(|error| PublishError::Compile(error.to_string()))?;
        let session_profile =
            registered_session_profile(&snapshot, &resolved.authored_model_selection, Some(inputs))
                .ok_or_else(|| {
                    PublishError::Registration(
                        awaken_executable_agent_contract::ExecutableAgentRegistrationError::Invalid(
                            "preview Session defaults changed while the snapshot was compiled"
                                .into(),
                        ),
                    )
                })?;
        self.registrar
            .register(ExecutableAgentRegistration {
                workspace_id: workspace.as_str().to_owned(),
                agent_id: preview_id.to_owned(),
                source_revision,
                snapshot: snapshot.clone(),
                session_profile,
            })
            .await
            .map_err(PublishError::Registration)?;
        Ok(snapshot)
    }
}
