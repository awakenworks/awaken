// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The extension owns one validated provider registry for discovery and dispatch;
// @formal reason: descriptors carry no authorization and publication uses the same exact routes.
//! Provider registration and its descriptor-derived configuration schema.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// One authoritative source for provider discovery and dispatch.
#[derive(Clone, Default)]
pub struct WebSearchProviderRegistry {
    pub(super) providers: BTreeMap<String, RegisteredWebSearchProvider>,
    pub(super) fetch_providers: BTreeMap<String, RegisteredWebFetchProvider>,
    pub(super) server_search_providers: BTreeMap<String, WebServerToolProviderDescriptor>,
    pub(super) server_fetch_providers: BTreeMap<String, WebServerToolProviderDescriptor>,
}

#[derive(Clone)]
pub(super) struct RegisteredWebSearchProvider {
    pub(super) descriptor: WebSearchProviderDescriptor,
    pub(super) provider: Arc<dyn WebSearchProvider>,
}

#[derive(Clone)]
pub(super) struct RegisteredWebFetchProvider {
    pub(super) descriptor: WebFetchProviderDescriptor,
    pub(super) provider: Arc<dyn WebFetchProvider>,
}

pub(super) fn validate_provider_descriptor(
    id: &str,
    label: &str,
    options_schema: &Value,
) -> Result<(), WebSearchRegistryError> {
    if id.trim().is_empty()
        || label.trim().is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        || !options_schema.is_object()
    {
        return Err(WebSearchRegistryError::InvalidDescriptor);
    }
    Ok(())
}

pub(super) fn validate_web_credential_requirement(
    requirement: &WebSearchCredentialRequirement,
) -> Result<(), WebSearchRegistryError> {
    match requirement {
        WebSearchCredentialRequirement::None => Ok(()),
        WebSearchCredentialRequirement::Exact(CredentialUsage::HttpHeader { name, .. })
            if !name.trim().is_empty() =>
        {
            Ok(())
        }
        WebSearchCredentialRequirement::Exact(_) => Err(WebSearchRegistryError::InvalidDescriptor),
    }
}

impl WebSearchProviderRegistry {
    pub fn try_new(
        providers: impl IntoIterator<Item = Arc<dyn WebSearchProvider>>,
    ) -> Result<Self, WebSearchRegistryError> {
        let mut registry = Self::default();
        for provider in providers {
            registry.register(provider)?;
        }
        Ok(registry)
    }

    pub fn builtins() -> Self {
        let mut catalog = Self::try_new([
            Arc::new(DuckDuckGoProvider) as Arc<dyn WebSearchProvider>,
            Arc::new(BraveSearchProvider) as Arc<dyn WebSearchProvider>,
        ])
        .expect("built-in web-search provider descriptors are unique and valid");
        catalog
            .register_fetch(Arc::new(AwakenDirectFetchProvider))
            .expect("built-in web-fetch provider descriptor is valid");
        catalog
            .register_server_search(openrouter_server_search_descriptor())
            .expect("OpenRouter search descriptor is valid");
        for descriptor in native_server_search_descriptors() {
            catalog
                .register_server_search(descriptor)
                .expect("native search descriptor is valid");
        }
        catalog
            .register_server_fetch(openrouter_server_fetch_descriptor())
            .expect("OpenRouter fetch descriptor is valid");
        catalog
    }

    /// Provider-server capabilities usable by hosted compositions without
    /// installing the open direct/BYOK HTTP providers in that process.
    pub fn server_builtins() -> Self {
        let mut catalog = Self::default();
        catalog
            .register_server_search(openrouter_server_search_descriptor())
            .expect("OpenRouter search descriptor is valid");
        for descriptor in native_server_search_descriptors() {
            catalog
                .register_server_search(descriptor)
                .expect("native search descriptor is valid");
        }
        catalog
            .register_server_fetch(openrouter_server_fetch_descriptor())
            .expect("OpenRouter fetch descriptor is valid");
        catalog
    }

    pub fn register(
        &mut self,
        provider: Arc<dyn WebSearchProvider>,
    ) -> Result<(), WebSearchRegistryError> {
        let descriptor = provider.descriptor();
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        validate_web_credential_requirement(&descriptor.credential)?;
        if self.providers.contains_key(&descriptor.id)
            || self.server_search_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.providers.insert(
            descriptor.id.clone(),
            RegisteredWebSearchProvider {
                descriptor,
                provider,
            },
        );
        Ok(())
    }

    pub fn register_fetch(
        &mut self,
        provider: Arc<dyn WebFetchProvider>,
    ) -> Result<(), WebSearchRegistryError> {
        let descriptor = provider.descriptor();
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        validate_web_credential_requirement(&descriptor.credential)?;
        if self.fetch_providers.contains_key(&descriptor.id)
            || self.server_fetch_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.fetch_providers.insert(
            descriptor.id.clone(),
            RegisteredWebFetchProvider {
                descriptor,
                provider,
            },
        );
        Ok(())
    }

    pub(super) fn register_server_search(
        &mut self,
        descriptor: WebServerToolProviderDescriptor,
    ) -> Result<(), WebSearchRegistryError> {
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.providers.contains_key(&descriptor.id)
            || self.server_search_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.server_search_providers
            .insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub(super) fn register_server_fetch(
        &mut self,
        descriptor: WebServerToolProviderDescriptor,
    ) -> Result<(), WebSearchRegistryError> {
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.fetch_providers.contains_key(&descriptor.id)
            || self.server_fetch_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.server_fetch_providers
            .insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub fn descriptors(&self) -> Vec<WebSearchProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor.clone())
            .collect()
    }

    pub(super) fn provider(&self, id: &str) -> Option<RegisteredWebSearchProvider> {
        self.providers.get(id).cloned()
    }

    pub(super) fn fetch_provider(&self, id: &str) -> Option<RegisteredWebFetchProvider> {
        self.fetch_providers.get(id).cloned()
    }

    /// Secret-free authentication contract for one host-executed search
    /// provider. Publication uses this same descriptor that dispatch uses.
    #[must_use]
    pub fn search_credential_requirement(
        &self,
        id: &str,
    ) -> Option<WebSearchCredentialRequirement> {
        self.providers
            .get(id)
            .map(|provider| provider.descriptor.credential.clone())
    }

    /// Secret-free authentication contract for one host-executed fetch
    /// provider. Publication uses this same descriptor that dispatch uses.
    #[must_use]
    pub fn fetch_credential_requirement(&self, id: &str) -> Option<WebSearchCredentialRequirement> {
        self.fetch_providers
            .get(id)
            .map(|provider| provider.descriptor.credential.clone())
    }

    /// JSON Schema is derived from the same descriptors dispatch uses. Each
    /// provider becomes one branch, so paid providers require an exact pin while
    /// free providers do not expose a meaningless credential field.
    pub fn config_schema(&self) -> Value {
        let mut descriptors = self.descriptors();
        descriptors.sort_by_key(|provider| {
            (
                !matches!(provider.credential, WebSearchCredentialRequirement::None),
                provider.id.clone(),
            )
        });
        let host_variants = descriptors
            .into_iter()
            .map(|provider| {
                host_target_schema(
                    &provider.id,
                    &provider.label,
                    &provider.credential,
                    provider.options_schema,
                )
            })
            .collect::<Vec<_>>();
        route_plan_schema(
            "Web search",
            host_variants,
            self.server_search_providers.values().cloned().collect(),
        )
    }

    pub fn fetch_config_schema(&self) -> Value {
        let mut descriptors = self
            .fetch_providers
            .values()
            .map(|provider| provider.descriptor.clone())
            .collect::<Vec<_>>();
        descriptors.sort_by_key(|provider| provider.id.clone());
        let host_variants = descriptors
            .into_iter()
            .map(|provider| {
                host_target_schema(
                    &provider.id,
                    &provider.label,
                    &provider.credential,
                    provider.options_schema,
                )
            })
            .collect();
        route_plan_schema(
            "Web fetch",
            host_variants,
            self.server_fetch_providers.values().cloned().collect(),
        )
    }
}
