//! Configuration publication preparation and values shared by the service and HTTP edge.

use awaken_config_store::{AgentConfig, AgentConfigRevision, ModelSelection};
use awaken_runtime_contract::ResolutionManifest;

use crate::binding_resolver::{
    ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
};
use crate::compaction::apply_compaction;

/// A publish failure, split so the edge can map it to an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("reserved configuration publication requires an explicit execution Workspace")]
    ExecutionWorkspaceRequired,
    #[error("no config stored for agent `{0}`")]
    NotStored(String),
    #[error("agent `{0}` is archived")]
    Archived(String),
    #[error("cannot resolve model publication: {0}")]
    Unresolvable(String),
    #[error("config changed while it was being published (current revision: {0:?})")]
    StaleRevision(Option<u64>),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Store(String),
}

/// A compile problem projected onto the authored config field that caused it.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

/// Transient, secret-free result of reading every configuration source once.
#[derive(Debug)]
pub(crate) struct AgentPublicationDraft {
    pub(crate) source: awaken_runtime_contract::AgentConfigRevisionRef,
    pub(crate) config: AgentConfig,
    pub(crate) manifest: ResolutionManifest,
    pub(crate) models: ResolvedPublicationModels,
}

/// Read every authored configuration input once and prepare one publication.
/// Runtime and worker paths receive only the resulting immutable snapshot.
pub(crate) async fn prepare_agent_publication(
    model_resolver: &dyn ModelPublicationResolver,
    workspace: &awaken_tenancy::ScopeId,
    source: AgentConfigRevision,
) -> Result<AgentPublicationDraft, PublishError> {
    let source_revision = source.revision;
    let mut config = source.config;
    let models = model_resolver
        .resolve_models(workspace, &config.model_binding, &config.model_candidates)
        .await
        .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    let mut bindings = std::collections::BTreeSet::new();
    for candidate in std::iter::once(&models.primary).chain(models.candidates.iter()) {
        if !bindings.insert(candidate.binding.clone()) {
            return Err(PublishError::Unresolvable(
                PublicationResolutionError::DuplicateBinding(candidate.binding.clone()).to_string(),
            ));
        }
    }
    if let Some(authored) = config.model_binding.resolved()
        && models.primary.binding != *authored
    {
        return Err(PublishError::Unresolvable(
            "resolved primary candidate does not match the pinned authoring binding".into(),
        ));
    }
    if config.model_binding.resolved().is_some()
        && (models.candidates.len() != config.model_candidates.len()
            || models
                .candidates
                .iter()
                .zip(&config.model_candidates)
                .any(|(resolved, authored)| &resolved.binding != authored))
    {
        return Err(PublishError::Unresolvable(
            "resolved fallback candidates do not match the pinned authoring order".into(),
        ));
    }
    config.model_binding = ModelSelection::Pinned(models.primary.binding.clone());
    config.model_candidates = models
        .candidates
        .iter()
        .map(|candidate| candidate.binding.clone())
        .collect();
    let strategy = config.compaction.clone().unwrap_or_default();
    apply_compaction(
        &mut config.plugin_config,
        &strategy,
        models.context_window,
        models.max_output_tokens,
    );
    let model_bytes = serde_json::to_vec(&(
        config.model_binding.resolved(),
        &config.model_candidates,
        &config.compaction,
    ))
    .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    let manifest = ResolutionManifest::new([
        awaken_runtime_contract::ResolvedInputRef {
            kind: "agent_config".into(),
            id: config.id.clone(),
            version: awaken_runtime_contract::ResolvedInputVersion::Revision(source_revision),
        },
        awaken_runtime_contract::ResolvedInputRef {
            kind: "model_binding".into(),
            id: config
                .model_binding
                .resolved()
                .map(|binding| binding.model_ref.clone())
                .unwrap_or_default(),
            version: awaken_runtime_contract::ResolvedInputVersion::ContentHash(
                awaken_runtime_contract::content_fingerprint(&model_bytes)
                    .map_err(|error| PublishError::Unresolvable(error.to_string()))?,
            ),
        },
    ])
    .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    Ok(AgentPublicationDraft {
        source: awaken_runtime_contract::AgentConfigRevisionRef {
            agent_id: awaken_runtime_contract::snapshot::AgentId(config.id.clone()),
            revision: source_revision,
        },
        config,
        manifest,
        models,
    })
}

pub(crate) fn snapshot_metadata(
    resolved: &AgentPublicationDraft,
) -> awaken_runtime_contract::AgentSnapshotMetadata {
    awaken_runtime_contract::AgentSnapshotMetadata {
        source: resolved.source.clone(),
        publication_version: awaken_runtime_contract::AgentPublicationVersion(String::new()),
        resolution: resolved.manifest.clone(),
        fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;

    fn revision(selection: ModelSelection, fallbacks: Vec<ModelBinding>) -> AgentConfigRevision {
        AgentConfigRevision {
            config: AgentConfig {
                id: "agent-a".into(),
                model_binding: selection,
                model_candidates: fallbacks,
                ..Default::default()
            },
            revision: 7,
        }
    }

    struct FixedResolver {
        expected_workspace: &'static str,
        output: ResolvedPublicationModels,
    }

    #[async_trait::async_trait]
    impl ModelPublicationResolver for FixedResolver {
        async fn resolve_models(
            &self,
            workspace: &awaken_tenancy::ScopeId,
            _selection: &ModelSelection,
            _candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            if workspace.as_str() != self.expected_workspace {
                return Err(format!("unexpected Workspace {workspace}").into());
            }
            Ok(self.output.clone())
        }
    }

    #[tokio::test]
    async fn resolver_receives_execution_workspace_and_auto_output_becomes_authoring_projection() {
        let primary = ModelBinding::new("provider", "primary", "genai");
        let fallback = ModelBinding::new("provider", "fallback", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-real",
            output: ResolvedPublicationModels::host(
                primary.clone(),
                vec![fallback.clone()],
                None,
                None,
            ),
        };
        let draft = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-real"),
            revision(ModelSelection::Auto, Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(draft.config.model_binding.resolved(), Some(&primary));
        assert_eq!(draft.config.model_candidates, vec![fallback]);
    }

    #[tokio::test]
    async fn resolver_cannot_rewrite_pinned_primary_or_fallback_order() {
        let primary = ModelBinding::new("provider", "primary", "genai");
        let fallback = ModelBinding::new("provider", "fallback", "genai");
        let rewritten = ModelBinding::new("provider", "other", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels::host(primary.clone(), vec![rewritten], None, None),
        };
        let error = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(ModelSelection::Pinned(primary), vec![fallback]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("fallback candidates"));
    }

    #[tokio::test]
    async fn duplicate_complete_bindings_reject_the_entire_publication() {
        let binding = ModelBinding::new("provider", "model", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels::host(binding.clone(), vec![binding], None, None),
        };

        let error = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(ModelSelection::Auto, Vec::new()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("duplicate model candidate"));
    }
}
