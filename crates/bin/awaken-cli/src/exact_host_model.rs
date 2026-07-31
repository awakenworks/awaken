//! Exact host-executor publication resolution for deterministic compositions.
//!
//! Provider-backed and hosted compositions use their own injected resolvers.
//! This adapter exists only for the one-host-executor scenario path.

pub(super) struct ExactHostModelPublicationResolver {
    pub(super) binding: awaken_runtime_contract::resolved::ModelBinding,
}

#[async_trait::async_trait]
impl awaken_config_service::ModelPublicationResolver for ExactHostModelPublicationResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_config_store::ModelSelection,
        candidates: &[awaken_runtime_contract::resolved::ModelBinding],
    ) -> Result<
        awaken_config_service::ResolvedPublicationModels,
        awaken_config_service::PublicationResolutionError,
    > {
        if let Some(authored) = selection.resolved() {
            let matches_host = authored.model_ref == self.binding.model_ref
                && (authored.provider_identity_ref.is_empty()
                    || authored.provider_identity_ref == self.binding.provider_identity_ref)
                && (authored.backend_ref.is_empty()
                    || authored.backend_ref == self.binding.backend_ref);
            if !matches_host {
                return Err(format!(
                    "scenario host executor `{}` cannot publish model `{}`",
                    self.binding.model_ref, authored.model_ref
                )
                .into());
            }
        }
        if !candidates.is_empty() {
            return Err("a single host executor cannot publish fallback candidates".into());
        }
        Ok(awaken_config_service::ResolvedPublicationModels::host(
            self.binding.clone(),
            Vec::new(),
            None,
            None,
        ))
    }
}
