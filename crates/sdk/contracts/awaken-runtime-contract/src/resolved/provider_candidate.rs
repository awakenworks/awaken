// @formal surface obligation=config.inference_access_resolved_once_at_publication
// @formal surface obligation=credential.provider_scope_widening_requires_exact_revision_provider_and_endpoint
// @formal surface obligation=model_readiness.brokered_and_direct_routes_require_exact_access_evidence
// @formal surface obligation=model_route.fallback_preserves_the_complete_publication_pin
// @formal reason: Provider candidate constructors seal the exact direct or brokered scope, endpoint,
// @formal reason: access posture, and credential boundary before the candidate enters a frozen route,
// @formal reason: delegating every invariant to the canonical validated candidate boundary.
use super::{
    AcpExecutionProfile, ImageInput, InvalidResolvedModelCandidate, ModelBinding,
    ModelProvisioning, ProviderAccessKind, ProviderExecutionProfile, ResolvedModelCandidate,
    UnspecifiedReasoning,
};

impl ResolvedModelCandidate {
    pub fn try_provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        credential: Option<awaken_agent_contract::credential::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_reasoning(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            UnspecifiedReasoning::ProviderDefault,
        )
    }

    pub fn try_provider_with_reasoning(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        credential: Option<awaken_agent_contract::credential::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        unspecified_reasoning: UnspecifiedReasoning,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning,
                acp: None,
            },
        )
    }

    /// Build one broker-authorized Provider candidate. Brokered publications
    /// never carry a Workspace credential; the installed broker freezes the
    /// exact downstream credential only while authorizing an attempt.
    pub fn try_brokered_provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        endpoint: crate::InferenceEndpoint,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_reasoning(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            UnspecifiedReasoning::ProviderDefault,
        )
    }

    pub fn try_brokered_provider_with_reasoning(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        unspecified_reasoning: UnspecifiedReasoning,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning,
                acp: None,
            },
        )
    }

    pub fn try_brokered_provider_with_acp(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        acp: AcpExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_brokered_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning: UnspecifiedReasoning::ProviderDefault,
                acp: Some(acp),
            },
        )
    }

    pub fn try_brokered_provider_with_profile(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        endpoint: crate::InferenceEndpoint,
        profile: ProviderExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::Provider {
                provider_ref: provider_ref.into(),
                route_ref: route_ref.into(),
                access_kind: ProviderAccessKind::Brokered,
                scope_id: scope_id.into(),
                credential: None,
                endpoint: Box::new(endpoint),
                unspecified_reasoning: profile.unspecified_reasoning,
                image_input: ImageInput::Accepted,
                acp: profile.acp.map(Box::new),
            },
        )
    }

    pub fn try_provider_with_acp(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        credential: Option<awaken_agent_contract::credential::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        acp: AcpExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_provider_with_profile(
            binding,
            provider_ref,
            route_ref,
            scope_id,
            credential,
            endpoint,
            ProviderExecutionProfile {
                unspecified_reasoning: UnspecifiedReasoning::ProviderDefault,
                acp: Some(acp),
            },
        )
    }

    pub fn try_provider_with_profile(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_agent_contract::ScopeId>,
        credential: Option<awaken_agent_contract::credential::CredentialAccess>,
        endpoint: crate::InferenceEndpoint,
        profile: ProviderExecutionProfile,
    ) -> Result<Self, InvalidResolvedModelCandidate> {
        Self::try_from_parts(
            binding,
            ModelProvisioning::Provider {
                provider_ref: provider_ref.into(),
                route_ref: route_ref.into(),
                access_kind: ProviderAccessKind::Direct,
                scope_id: scope_id.into(),
                credential: credential.map(Box::new),
                endpoint: Box::new(endpoint),
                unspecified_reasoning: profile.unspecified_reasoning,
                image_input: ImageInput::Accepted,
                acp: profile.acp.map(Box::new),
            },
        )
    }

    /// Freeze whether this route's model accepts images. Only a Provider route
    /// reaches a provider request; any other provisioning is returned unchanged.
    #[must_use]
    pub fn with_image_input(mut self, policy: ImageInput) -> Self {
        if let ModelProvisioning::Provider { image_input, .. } = &mut self.provisioning {
            *image_input = policy;
        }
        self
    }
}
