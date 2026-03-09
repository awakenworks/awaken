use super::sorted_registry_ids;
use super::traits::{ModelDefinition, ModelRegistry, ModelRegistryError};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct InMemoryModelRegistry {
    models: HashMap<String, ModelDefinition>,
}

impl InMemoryModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.models.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.models.keys()
    }

    pub fn register(
        &mut self,
        model_id: impl Into<String>,
        mut def: ModelDefinition,
    ) -> Result<(), ModelRegistryError> {
        let model_id = model_id.into();
        if self.models.contains_key(&model_id) {
            return Err(ModelRegistryError::ModelIdConflict(model_id));
        }
        def.provider = def.provider.trim().to_string();
        def.model = def.model.trim().to_string();
        if def.provider.is_empty() {
            return Err(ModelRegistryError::EmptyProviderId);
        }
        if def.model.is_empty() {
            return Err(ModelRegistryError::EmptyModelName);
        }
        self.models.insert(model_id, def);
        Ok(())
    }

    pub fn extend(
        &mut self,
        defs: HashMap<String, ModelDefinition>,
    ) -> Result<(), ModelRegistryError> {
        for (id, def) in defs {
            self.register(id, def)?;
        }
        Ok(())
    }

    pub fn extend_registry(&mut self, other: &dyn ModelRegistry) -> Result<(), ModelRegistryError> {
        self.extend(other.snapshot())
    }
}

impl ModelRegistry for InMemoryModelRegistry {
    fn len(&self) -> usize {
        self.len()
    }

    fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.models)
    }

    fn snapshot(&self) -> HashMap<String, ModelDefinition> {
        self.models.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeModelRegistry {
    merged: InMemoryModelRegistry,
}

impl std::fmt::Debug for CompositeModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeModelRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeModelRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ModelRegistry>>,
    ) -> Result<Self, ModelRegistryError> {
        let mut merged = InMemoryModelRegistry::new();
        for r in regs {
            merged.extend_registry(r.as_ref())?;
        }
        Ok(Self { merged })
    }
}

impl ModelRegistry for CompositeModelRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.merged.models)
    }

    fn snapshot(&self) -> HashMap<String, ModelDefinition> {
        self.merged.models.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_registry_trims_provider_and_model_names() {
        let mut registry = InMemoryModelRegistry::new();
        registry
            .register("m1", ModelDefinition::new(" openai ", " gemini-2.5-flash "))
            .expect("register model");

        let model = registry.get("m1").expect("stored model");
        assert_eq!(model.provider, "openai");
        assert_eq!(model.model, "gemini-2.5-flash");
    }

    #[test]
    fn model_registry_rejects_whitespace_only_provider_or_model() {
        let mut registry = InMemoryModelRegistry::new();
        assert!(matches!(
            registry.register("m1", ModelDefinition::new("   ", "gpt-4o-mini")),
            Err(ModelRegistryError::EmptyProviderId)
        ));
        assert!(matches!(
            registry.register("m2", ModelDefinition::new("openai", "   ")),
            Err(ModelRegistryError::EmptyModelName)
        ));
    }
}
