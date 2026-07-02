//! The skill registry: the lookup the single `Skill` tool resolves against.
//!
//! A registry answers two questions the tool needs: "list the activatable skills"
//! (for the catalog in the tool descriptor) and "get one skill by id" (for
//! activation). Keeping this a trait lets a later slice add filesystem or
//! MCP-backed registries without changing the tool.

use std::collections::BTreeMap;

use crate::spec::SkillSpec;

/// Resolves skills for the `Skill` tool. Implementations are the source of the
/// catalog and of the body returned on activation.
pub trait SkillRegistry: Send + Sync {
    /// Look up one skill by its id.
    fn get(&self, id: &str) -> Option<SkillSpec>;

    /// Every skill, in a stable order. The tool filters to model-invocable ones
    /// when it renders the catalog.
    fn list(&self) -> Vec<SkillSpec>;
}

/// An in-memory registry built from a fixed set of specs. The composition root
/// (e.g. the local server) constructs this from configured skills.
#[derive(Debug, Clone, Default)]
pub struct InMemorySkillRegistry {
    skills: BTreeMap<String, SkillSpec>,
}

impl InMemorySkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from a set of specs. A later spec with a duplicate id
    /// replaces an earlier one.
    pub fn from_specs(specs: impl IntoIterator<Item = SkillSpec>) -> Self {
        let mut registry = Self::new();
        for spec in specs {
            registry.insert(spec);
        }
        registry
    }

    /// Insert or replace one skill.
    pub fn insert(&mut self, spec: SkillSpec) {
        self.skills.insert(spec.id.clone(), spec);
    }

    /// Whether the registry holds no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

impl SkillRegistry for InMemorySkillRegistry {
    fn get(&self, id: &str) -> Option<SkillSpec> {
        self.skills.get(id).cloned()
    }

    fn list(&self) -> Vec<SkillSpec> {
        self.skills.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_specs_lists_in_id_order_and_gets_by_id() {
        let registry = InMemorySkillRegistry::from_specs([
            SkillSpec::new("b", "B", "second", "body-b"),
            SkillSpec::new("a", "A", "first", "body-a"),
        ]);
        let ids: Vec<_> = registry.list().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(registry.get("a").unwrap().body, "body-a");
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn insert_replaces_a_duplicate_id() {
        let registry = InMemorySkillRegistry::from_specs([
            SkillSpec::new("a", "A", "old", "old-body"),
            SkillSpec::new("a", "A", "new", "new-body"),
        ]);
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("a").unwrap().body, "new-body");
    }
}
