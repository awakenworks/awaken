use super::{
    AcpExecutionProfile, InvalidResolvedModelCandidate, ModelBinding, ModelProvisioning,
    ProviderAccessKind, ProviderExecutionProfile, ResolvedModelCandidate, UnspecifiedReasoning,
};

impl ResolvedModelCandidate {
    pub fn try_provider(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
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
                acp: profile.acp.map(Box::new),
            },
        )
    }

    pub fn try_provider_with_acp(
        binding: ModelBinding,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
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
        scope_id: impl Into<awaken_tenancy::ScopeId>,
        credential: Option<crate::CredentialAccess>,
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
                acp: profile.acp.map(Box::new),
            },
        )
    }
}
