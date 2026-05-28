//! Pinned runtime registries used by run-scoped frozen registry sets.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_runtime_contract::contract::storage::PinnedRegistryEntry;
use awaken_runtime_contract::registry_spec::{AgentSpec, ModelPoolSpec, ModelSpec};
use thiserror::Error;

use super::memory::MapAgentSpecRegistry;
use super::traits::{AgentSpecRegistry, ModelRegistry};

/// Errors returned while building a pinned agent registry.
///
/// Marked `#[non_exhaustive]` so future minor releases can add variants
/// without breaking downstream `match` arms.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PinnedRegistryError {
    #[error("pinned entry has kind {kind}, expected {expected}")]
    WrongKind { kind: String, expected: String },
    #[error("pinned entry id {entry_id} does not match spec id {spec_id}")]
    IdMismatch { entry_id: String, spec_id: String },
    #[error("pinned entry {id} has invalid version {version}")]
    InvalidVersion { id: String, version: u64 },
    #[error("pinned entry {id} has invalid content hash {content_hash}")]
    InvalidContentHash { id: String, content_hash: String },
    #[error("duplicate pinned entry {id}")]
    DuplicateAgent { id: String },
}

/// Synchronous agent registry materialized from a run's pinned registry manifest.
#[derive(Default)]
pub struct PinnedAgentSpecRegistry {
    agents: MapAgentSpecRegistry,
    pins: HashMap<String, PinnedRegistryEntry>,
}

impl PinnedAgentSpecRegistry {
    /// Create an empty pinned agent registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a pinned registry from `(AgentSpec, PinnedRegistryEntry)` pairs.
    pub fn from_pinned_agents(
        agents: impl IntoIterator<Item = (AgentSpec, PinnedRegistryEntry)>,
    ) -> Result<Self, PinnedRegistryError> {
        let mut registry = Self::new();
        for (spec, pin) in agents {
            registry.insert(spec, pin)?;
        }
        Ok(registry)
    }

    /// Insert one pinned agent and its immutable version metadata.
    pub fn insert(
        &mut self,
        spec: AgentSpec,
        pin: PinnedRegistryEntry,
    ) -> Result<(), PinnedRegistryError> {
        validate_agent_pin(&spec, &pin)?;
        if self.pins.contains_key(&spec.id) {
            return Err(PinnedRegistryError::DuplicateAgent { id: spec.id });
        }
        let id = spec.id.clone();
        self.agents.replace(id.clone(), spec);
        self.pins.insert(id, pin);
        Ok(())
    }

    /// Return the pinned version metadata for an agent id.
    #[must_use]
    pub fn pin_for_agent(&self, id: &str) -> Option<&PinnedRegistryEntry> {
        self.pins.get(id)
    }

    /// Return the pinned version metadata for every agent in this registry.
    #[must_use]
    pub fn pinned_agents(&self) -> Vec<PinnedRegistryEntry> {
        self.pins.values().cloned().collect()
    }
}

impl AgentSpecRegistry for PinnedAgentSpecRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        self.agents.get_agent(id)
    }

    fn agent_ids(&self) -> Vec<String> {
        self.agents.agent_ids()
    }
}

fn validate_agent_pin(
    spec: &AgentSpec,
    pin: &PinnedRegistryEntry,
) -> Result<(), PinnedRegistryError> {
    validate_pin_envelope(pin, "agent")?;
    if pin.id != spec.id {
        return Err(PinnedRegistryError::IdMismatch {
            entry_id: pin.id.clone(),
            spec_id: spec.id.clone(),
        });
    }
    Ok(())
}

fn validate_pin_envelope(
    pin: &PinnedRegistryEntry,
    expected_kind: &str,
) -> Result<(), PinnedRegistryError> {
    if pin.kind != expected_kind {
        return Err(PinnedRegistryError::WrongKind {
            kind: pin.kind.clone(),
            expected: expected_kind.to_string(),
        });
    }
    if pin.version == 0 {
        return Err(PinnedRegistryError::InvalidVersion {
            id: pin.id.clone(),
            version: pin.version,
        });
    }
    if !is_valid_content_hash(&pin.content_hash) {
        return Err(PinnedRegistryError::InvalidContentHash {
            id: pin.id.clone(),
            content_hash: pin.content_hash.clone(),
        });
    }
    Ok(())
}

/// Generic synchronous map of pinned runtime-config specs of one kind.
/// Used to materialize the non-agent kinds (model, provider, skill, tool,
/// plugin_config) into a frozen run-scoped registry as ADR-0035 D8
/// requires.
#[derive(Debug)]
pub struct PinnedSpecMap<T> {
    expected_kind: &'static str,
    specs: HashMap<String, T>,
    pins: HashMap<String, PinnedRegistryEntry>,
}

impl<T> PinnedSpecMap<T> {
    #[must_use]
    pub fn new(expected_kind: &'static str) -> Self {
        Self {
            expected_kind,
            specs: HashMap::new(),
            pins: HashMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        id: String,
        spec: T,
        pin: PinnedRegistryEntry,
    ) -> Result<(), PinnedRegistryError> {
        validate_pin_envelope(&pin, self.expected_kind)?;
        if pin.id != id {
            return Err(PinnedRegistryError::IdMismatch {
                entry_id: pin.id.clone(),
                spec_id: id,
            });
        }
        if self.pins.contains_key(&id) {
            return Err(PinnedRegistryError::DuplicateAgent { id });
        }
        self.pins.insert(id.clone(), pin);
        self.specs.insert(id, spec);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&T> {
        self.specs.get(id)
    }

    #[must_use]
    pub fn pin_for(&self, id: &str) -> Option<&PinnedRegistryEntry> {
        self.pins.get(id)
    }

    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.specs.keys().cloned().collect()
    }

    #[must_use]
    pub fn pinned_entries(&self) -> Vec<PinnedRegistryEntry> {
        self.pins.values().cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }
}

impl<T> Default for PinnedSpecMap<T> {
    fn default() -> Self {
        Self::new("")
    }
}

impl ModelRegistry for PinnedSpecMap<ModelSpec> {
    fn get_model(&self, id: &str) -> Option<ModelSpec> {
        self.get(id).cloned()
    }
    fn model_ids(&self) -> Vec<String> {
        self.ids()
    }
}

/// Pinned model registry holding both pinned models and pinned pools in one id
/// namespace, mirroring the live [`MapModelRegistry`](super::memory::MapModelRegistry)
/// so durable/pinned runs resolve a pool exactly as live runs do.
///
/// Holds the maps behind `Arc` so a frozen registry can share its existing
/// pinned spec maps without cloning them.
#[derive(Debug)]
pub struct PinnedModelRegistry {
    models: Arc<PinnedSpecMap<ModelSpec>>,
    pools: Arc<PinnedSpecMap<ModelPoolSpec>>,
}

impl PinnedModelRegistry {
    #[must_use]
    pub fn new(
        models: Arc<PinnedSpecMap<ModelSpec>>,
        pools: Arc<PinnedSpecMap<ModelPoolSpec>>,
    ) -> Self {
        Self { models, pools }
    }
}

impl ModelRegistry for PinnedModelRegistry {
    fn get_model(&self, id: &str) -> Option<ModelSpec> {
        self.models.get(id).cloned()
    }

    fn model_ids(&self) -> Vec<String> {
        self.models.ids()
    }

    fn get_pool(&self, id: &str) -> Option<ModelPoolSpec> {
        self.pools.get(id).cloned()
    }

    fn pool_ids(&self) -> Vec<String> {
        self.pools.ids()
    }
}

fn is_valid_content_hash(hash: &str) -> bool {
    const SHA256_PREFIX: &str = "sha256:";
    let Some(hex) = hash.strip_prefix(SHA256_PREFIX) else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_string(),
            model_id: "model".to_string(),
            system_prompt: "system".to_string(),
            ..Default::default()
        }
    }

    fn pin(id: &str, version: u64) -> PinnedRegistryEntry {
        PinnedRegistryEntry {
            kind: "agent".to_string(),
            id: id.to_string(),
            version,
            content_hash: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
        }
    }

    fn registry_error(
        result: Result<PinnedAgentSpecRegistry, PinnedRegistryError>,
    ) -> PinnedRegistryError {
        match result {
            Ok(_) => panic!("expected pinned registry error"),
            Err(error) => error,
        }
    }

    #[test]
    fn pinned_registry_implements_agent_lookup_with_version_metadata() {
        let registry = PinnedAgentSpecRegistry::from_pinned_agents(vec![
            (agent("root"), pin("root", 7)),
            (agent("delegate"), pin("delegate", 2)),
        ])
        .unwrap();

        assert_eq!(registry.get_agent("root").unwrap().id, "root");
        assert_eq!(registry.pin_for_agent("root").unwrap().version, 7);
        assert_eq!(registry.pin_for_agent("delegate").unwrap().version, 2);
        assert!(registry.get_agent("missing").is_none());

        let mut ids = registry.agent_ids();
        ids.sort();
        assert_eq!(ids, vec!["delegate".to_string(), "root".to_string()]);
    }

    #[test]
    fn pinned_registry_rejects_non_agent_entries() {
        let mut wrong = pin("root", 1);
        wrong.kind = "model".to_string();

        let error = registry_error(PinnedAgentSpecRegistry::from_pinned_agents(vec![(
            agent("root"),
            wrong,
        )]));

        assert!(
            matches!(error, PinnedRegistryError::WrongKind { kind, expected } if kind == "model" && expected == "agent")
        );
    }

    #[test]
    fn pinned_registry_rejects_id_mismatch() {
        let error = registry_error(PinnedAgentSpecRegistry::from_pinned_agents(vec![(
            agent("root"),
            pin("other", 1),
        )]));

        assert!(matches!(
            error,
            PinnedRegistryError::IdMismatch { entry_id, spec_id }
                if entry_id == "other" && spec_id == "root"
        ));
    }

    #[test]
    fn pinned_registry_rejects_invalid_version_or_hash() {
        let version_error = registry_error(PinnedAgentSpecRegistry::from_pinned_agents(vec![(
            agent("root"),
            pin("root", 0),
        )]));
        assert!(matches!(
            version_error,
            PinnedRegistryError::InvalidVersion { id, version }
                if id == "root" && version == 0
        ));

        let mut bad_hash = pin("root", 1);
        bad_hash.content_hash = "sha256:not-hex".to_string();
        let hash_error = registry_error(PinnedAgentSpecRegistry::from_pinned_agents(vec![(
            agent("root"),
            bad_hash,
        )]));
        assert!(matches!(
            hash_error,
            PinnedRegistryError::InvalidContentHash { id, .. } if id == "root"
        ));
    }

    #[test]
    fn pinned_spec_map_stores_typed_specs_and_validates_kind() {
        // ADR-0035 D8: frozen RegistrySet backed by synchronous typed maps
        // for every runtime-config kind, not only agents.
        let mut map: PinnedSpecMap<String> = PinnedSpecMap::new("model");
        let mut model_pin = pin("model-a", 3);
        model_pin.kind = "model".to_string();
        map.insert(
            "model-a".to_string(),
            "model-spec".to_string(),
            model_pin.clone(),
        )
        .unwrap();
        assert_eq!(map.get("model-a"), Some(&"model-spec".to_string()));
        assert_eq!(map.pin_for("model-a").unwrap().version, 3);
        assert_eq!(map.len(), 1);

        // Wrong kind rejected.
        let mut wrong_kind = pin("model-b", 1);
        wrong_kind.kind = "agent".to_string();
        let err = map
            .insert("model-b".to_string(), "x".to_string(), wrong_kind)
            .unwrap_err();
        assert!(matches!(
            err,
            PinnedRegistryError::WrongKind { kind, expected }
                if kind == "agent" && expected == "model"
        ));

        // Duplicate id rejected.
        let dup = map
            .insert("model-a".to_string(), "x".to_string(), model_pin)
            .unwrap_err();
        assert!(matches!(dup, PinnedRegistryError::DuplicateAgent { id } if id == "model-a"));
    }

    #[test]
    fn pinned_spec_map_implements_model_registry_when_holding_model_specs() {
        let mut map: PinnedSpecMap<ModelSpec> = PinnedSpecMap::new("model");
        let mut model_pin = pin("model-1", 4);
        model_pin.kind = "model".to_string();
        map.insert(
            "model-1".to_string(),
            ModelSpec::new("model-1", "openai", "gpt-4o"),
            model_pin,
        )
        .unwrap();
        let spec = ModelRegistry::get_model(&map, "model-1").expect("model spec");
        assert_eq!(spec.provider_id, "openai");
        assert_eq!(spec.upstream_model, "gpt-4o");
        assert_eq!(ModelRegistry::model_ids(&map), vec!["model-1".to_string()]);
    }

    #[test]
    fn pinned_model_registry_resolves_models_and_pools() {
        let mut models: PinnedSpecMap<ModelSpec> = PinnedSpecMap::new("model");
        let mut model_pin = pin("m0", 2);
        model_pin.kind = "model".to_string();
        models
            .insert("m0".to_string(), ModelSpec::new("m0", "p", "up"), model_pin)
            .unwrap();

        let mut pools: PinnedSpecMap<ModelPoolSpec> = PinnedSpecMap::new("model_pool");
        let mut pool_pin = pin("pool-1", 5);
        pool_pin.kind = "model_pool".to_string();
        pools
            .insert(
                "pool-1".to_string(),
                ModelPoolSpec::new("pool-1", ["m0"]),
                pool_pin,
            )
            .unwrap();

        let registry = PinnedModelRegistry::new(Arc::new(models), Arc::new(pools));
        // Models resolve as before.
        assert_eq!(registry.get_model("m0").unwrap().provider_id, "p");
        assert_eq!(registry.model_ids(), vec!["m0".to_string()]);
        // Pools resolve from the parallel pinned map.
        assert_eq!(registry.get_pool("pool-1").unwrap().members.len(), 1);
        assert_eq!(registry.pool_ids(), vec!["pool-1".to_string()]);
        // A pool id is not a model and vice versa.
        assert!(registry.get_model("pool-1").is_none());
        assert!(registry.get_pool("m0").is_none());
    }

    #[test]
    fn pinned_registry_rejects_duplicate_agents() {
        let error = registry_error(PinnedAgentSpecRegistry::from_pinned_agents(vec![
            (agent("root"), pin("root", 1)),
            (agent("root"), pin("root", 2)),
        ]));

        assert!(matches!(error, PinnedRegistryError::DuplicateAgent { id } if id == "root"));
    }
}
