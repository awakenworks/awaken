//! Configuration publication preparation and values shared by the service and HTTP edge.

use awaken_config_resolver::InferenceAccessPublisher;
use awaken_config_store::{AgentConfig, AgentConfigRevision, ModelSelection};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{InferenceAccess, ResolutionManifest};

use crate::binding_resolver::ModelResolver;
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
    #[error("cannot resolve an auto model binding: {0}")]
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
    pub(crate) models: Option<ResolvedPublicationModels>,
}

#[derive(Debug)]
pub(crate) struct ResolvedPublicationModels {
    pub(crate) primary: ResolvedModelCandidate,
    pub(crate) candidates: Vec<ResolvedModelCandidate>,
}

/// Read every authored configuration input once and prepare one publication.
/// Runtime and worker paths receive only the resulting immutable snapshot.
pub(crate) fn prepare_agent_publication(
    model_resolver: Option<&dyn ModelResolver>,
    source: AgentConfigRevision,
) -> Result<AgentPublicationDraft, PublishError> {
    let source_revision = source.revision;
    let mut config = source.config;
    if crate::binding_resolver::needs_resolution(&config.model_binding) {
        let resolver = model_resolver
            .ok_or_else(|| PublishError::Unresolvable("no model resolver wired".into()))?;
        let resolved = resolver
            .resolve_auto()
            .map_err(PublishError::Unresolvable)?;
        config.model_binding = ModelSelection::Pinned(resolved.primary);
        config.model_candidates = resolved.candidates;
    }
    if let Some(resolver) = model_resolver
        && let Some(model_id) = config.model_binding.resolved().map(|b| b.model_ref.clone())
    {
        let strategy = config.compaction.clone().unwrap_or_default();
        apply_compaction(
            &mut config.plugin_config,
            &strategy,
            resolver.context_window(&model_id),
            resolver.max_output_tokens(&model_id),
        );
    }
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
        models: None,
    })
}

fn attach_access(
    binding: awaken_runtime_contract::ModelBinding,
    access: InferenceAccess,
) -> Result<ResolvedModelCandidate, PublishError> {
    if access.is_host_executor_for(&binding.model_ref) {
        return Ok(ResolvedModelCandidate::host(binding));
    }
    if access.scheme != "credential-source/v1" {
        return Err(PublishError::Unresolvable(format!(
            "unsupported published model access scheme `{}`",
            access.scheme
        )));
    }
    let provider_ref = access
        .provider_ref
        .ok_or_else(|| PublishError::Unresolvable("published model has no provider pin".into()))?;
    let route_ref = access
        .route_ref
        .ok_or_else(|| PublishError::Unresolvable("published model has no route pin".into()))?;
    let scope_id = access.scope_id.ok_or_else(|| {
        PublishError::Unresolvable("published model has no Workspace owner".into())
    })?;
    let endpoint = access.endpoint.ok_or_else(|| {
        PublishError::Unresolvable("published model has no provider endpoint".into())
    })?;
    Ok(ResolvedModelCandidate {
        binding,
        provisioning: ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            scope_id,
            credential: access.credential_access.map(Box::new),
            endpoint: Box::new(endpoint),
        },
    })
}

/// Select provider/gateway access exactly once and attach it to each complete
/// model candidate. The returned value is the single snapshot representation;
/// `InferenceAccess` is only a transitional publisher adapter value.
pub(crate) async fn resolve_publication_models(
    publisher: Option<&dyn InferenceAccessPublisher>,
    workspace: &str,
    draft: &AgentPublicationDraft,
) -> Result<ResolvedPublicationModels, PublishError> {
    let models = draft
        .config
        .model_binding
        .resolved()
        .into_iter()
        .chain(draft.config.model_candidates.iter())
        .cloned()
        .collect::<Vec<_>>();
    let access = if let Some(publisher) = publisher {
        publisher
            .resolve_access(workspace, &models)
            .await
            .map_err(PublishError::Unresolvable)?
    } else {
        InferenceAccess::candidate_set(models.iter().map(|model| {
            (
                model.model_ref.clone(),
                InferenceAccess::host_executor(&model.model_ref),
            )
        }))
        .ok_or_else(|| {
            PublishError::Unresolvable("published agent has no resolved model candidate".into())
        })?
    };
    attach_ordered_accesses(models, access)
}

fn attach_ordered_accesses(
    models: Vec<awaken_runtime_contract::ModelBinding>,
    access: InferenceAccess,
) -> Result<ResolvedPublicationModels, PublishError> {
    let accesses = if access.candidates.is_empty() {
        if models.len() != 1 {
            return Err(PublishError::Unresolvable(format!(
                "publisher returned one flat access for {} authored model candidates",
                models.len()
            )));
        }
        vec![access]
    } else {
        if access.candidates.len() != models.len() {
            return Err(PublishError::Unresolvable(format!(
                "publisher returned {} accesses for {} authored model candidates",
                access.candidates.len(),
                models.len()
            )));
        }
        access
            .candidates
            .into_iter()
            .zip(models.iter())
            .map(|(candidate, binding)| {
                if candidate.model_ref != binding.model_ref {
                    return Err(PublishError::Unresolvable(format!(
                        "publisher candidate `{}` does not match authored model `{}` in the same position",
                        candidate.model_ref, binding.model_ref
                    )));
                }
                if !candidate.access.candidates.is_empty() {
                    return Err(PublishError::Unresolvable(format!(
                        "publisher candidate `{}` contains a nested candidate set",
                        candidate.model_ref
                    )));
                }
                Ok(candidate.access)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut resolved = Vec::with_capacity(models.len());
    for (binding, exact) in models.into_iter().zip(accesses) {
        resolved.push(attach_access(binding, exact)?);
    }
    let mut resolved = resolved.into_iter();
    let primary = resolved.next().ok_or_else(|| {
        PublishError::Unresolvable("published agent has no resolved model candidate".into())
    })?;
    Ok(ResolvedPublicationModels {
        primary,
        candidates: resolved.collect(),
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
        inference_access: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{InferenceEndpoint, ModelBinding};

    fn binding(model_ref: &str) -> ModelBinding {
        ModelBinding::new(format!("identity-{model_ref}"), model_ref, "genai")
    }

    fn access(model_ref: &str) -> InferenceAccess {
        InferenceAccess::resolved_credential(
            format!("credential-{model_ref}"),
            1,
            "workspace-a",
            "provider@1",
            format!("route-{model_ref}@1"),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                base_url: "https://example.invalid/v1".into(),
                upstream_model: model_ref.into(),
            },
        )
    }

    #[test]
    fn a_flat_access_cannot_cover_multiple_authored_candidates() {
        let error = attach_ordered_accesses(
            vec![binding("primary"), binding("fallback")],
            access("primary"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("one flat access for 2 authored model candidates")
        );
    }

    #[test]
    fn a_reordered_access_pool_is_rejected() {
        let access = InferenceAccess::candidate_set([
            ("fallback".to_string(), access("fallback")),
            ("primary".to_string(), access("primary")),
        ])
        .unwrap();

        let error = attach_ordered_accesses(vec![binding("primary"), binding("fallback")], access)
            .unwrap_err();

        assert!(error.to_string().contains("does not match authored model"));
    }

    #[test]
    fn complete_accesses_preserve_authored_order_and_coordinates() {
        let access = InferenceAccess::candidate_set([
            ("primary".to_string(), access("primary")),
            ("fallback".to_string(), access("fallback")),
        ])
        .unwrap();

        let resolved =
            attach_ordered_accesses(vec![binding("primary"), binding("fallback")], access).unwrap();
        let candidates = std::iter::once(&resolved.primary)
            .chain(resolved.candidates.iter())
            .collect::<Vec<_>>();

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.binding.model_ref.as_str())
                .collect::<Vec<_>>(),
            ["primary", "fallback"]
        );
        for candidate in candidates {
            let ModelProvisioning::Provider { endpoint, .. } = &candidate.provisioning else {
                panic!("provider provisioning")
            };
            assert_eq!(endpoint.upstream_model, candidate.binding.model_ref);
        }
    }
}
