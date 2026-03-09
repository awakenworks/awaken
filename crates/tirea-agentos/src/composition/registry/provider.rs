use super::sorted_registry_ids;
use super::traits::{ProviderRegistry, ProviderRegistryError};
use genai::Client;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct InMemoryProviderRegistry {
    providers: HashMap<String, Client>,
}

impl std::fmt::Debug for InMemoryProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryProviderRegistry")
            .field("len", &self.providers.len())
            .finish()
    }
}

impl InMemoryProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        provider_id: impl Into<String>,
        client: Client,
    ) -> Result<(), ProviderRegistryError> {
        let provider_id = provider_id.into();
        if provider_id.trim().is_empty() {
            return Err(ProviderRegistryError::EmptyProviderId);
        }
        if self.providers.contains_key(&provider_id) {
            return Err(ProviderRegistryError::ProviderIdConflict(provider_id));
        }
        self.providers.insert(provider_id, client);
        Ok(())
    }

    pub fn extend(
        &mut self,
        providers: HashMap<String, Client>,
    ) -> Result<(), ProviderRegistryError> {
        for (id, client) in providers {
            self.register(id, client)?;
        }
        Ok(())
    }
}

impl ProviderRegistry for InMemoryProviderRegistry {
    fn len(&self) -> usize {
        self.providers.len()
    }

    fn get(&self, id: &str) -> Option<Client> {
        self.providers.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.providers)
    }

    fn snapshot(&self) -> HashMap<String, Client> {
        self.providers.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeProviderRegistry {
    merged: InMemoryProviderRegistry,
}

impl std::fmt::Debug for CompositeProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeProviderRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeProviderRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ProviderRegistry>>,
    ) -> Result<Self, ProviderRegistryError> {
        let mut merged = InMemoryProviderRegistry::new();
        for r in regs {
            merged.extend(r.snapshot())?;
        }
        Ok(Self { merged })
    }
}

impl ProviderRegistry for CompositeProviderRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<Client> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.merged.providers)
    }

    fn snapshot(&self) -> HashMap<String, Client> {
        self.merged.snapshot()
    }
}
