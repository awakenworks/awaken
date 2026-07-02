//! The skill registry: the lookup the single `Skill` tool resolves against.
//!
//! A registry answers two questions the tool needs: "list the activatable skills"
//! (for the catalog in the tool descriptor) and "get one skill by id" (for
//! activation). Keeping this a trait lets a later slice add filesystem or
//! MCP-backed registries without changing the tool.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::spec::{SkillProvenance, SkillSpec, parse_skill_md};

/// Resolves skills for the `Skill` tool. Implementations are the source of the
/// catalog and of the body returned on activation.
pub trait SkillRegistry: Send + Sync {
    /// Look up one skill by its id.
    fn get(&self, id: &str) -> Option<SkillSpec>;

    /// Every skill, in a stable order. The tool filters to model-invocable ones
    /// when it renders the catalog.
    fn list(&self) -> Vec<SkillSpec>;
}

/// One `SKILL.md`-bearing directory as neutral file data. The port a registry
/// scans; the host implements it (e.g. over the sandbox environment) so this
/// crate stays unaware of where the files live (local dir, container, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    pub id: String,
    pub content: String,
    /// Logical directory (`${SKILL_DIR}`), when known.
    pub dir: Option<String>,
}

/// Where skill files come from. Scanned **live** on each registry query, so a
/// skill the agent authored this run is discovered (ADR-0036 D6/D8).
pub trait SkillSource: Send + Sync {
    fn scan(&self) -> Vec<SkillFile>;
}

/// A registry backed by a [`SkillSource`]: parses each scanned `SKILL.md` into a
/// [`SkillSpec`], stamping the given provenance and the file's `dir`. Re-scans on
/// every call, so authored-this-run skills appear without a rebuild.
pub struct SourceSkillRegistry {
    source: Arc<dyn SkillSource>,
    provenance: SkillProvenance,
}

impl SourceSkillRegistry {
    pub fn new(source: Arc<dyn SkillSource>, provenance: SkillProvenance) -> Self {
        Self { source, provenance }
    }
}

impl SkillRegistry for SourceSkillRegistry {
    fn get(&self, id: &str) -> Option<SkillSpec> {
        self.list().into_iter().find(|s| s.id == id)
    }

    fn list(&self) -> Vec<SkillSpec> {
        self.source
            .scan()
            .into_iter()
            .map(|file| {
                let mut spec = parse_skill_md(file.id, &file.content);
                spec.dir = file.dir;
                spec.provenance = self.provenance;
                spec
            })
            .collect()
    }
}

/// Aggregates several registries. `get` returns the first hit; `list` concatenates
/// and de-duplicates by id (an earlier registry wins), so a delivered skill shadows
/// an agent-created one with the same id.
pub struct CompositeSkillRegistry {
    registries: Vec<Arc<dyn SkillRegistry>>,
}

impl CompositeSkillRegistry {
    pub fn new(registries: Vec<Arc<dyn SkillRegistry>>) -> Self {
        Self { registries }
    }
}

impl SkillRegistry for CompositeSkillRegistry {
    fn get(&self, id: &str) -> Option<SkillSpec> {
        self.registries.iter().find_map(|r| r.get(id))
    }

    fn list(&self) -> Vec<SkillSpec> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for registry in &self.registries {
            for spec in registry.list() {
                if seen.insert(spec.id.clone()) {
                    out.push(spec);
                }
            }
        }
        out
    }
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

    struct FakeSource(Vec<SkillFile>);
    impl SkillSource for FakeSource {
        fn scan(&self) -> Vec<SkillFile> {
            self.0.clone()
        }
    }

    #[test]
    fn source_registry_parses_files_and_stamps_provenance_and_dir() {
        let source = Arc::new(FakeSource(vec![SkillFile {
            id: "deploy".into(),
            content: "---\ndescription: ship it\n---\nbody".into(),
            dir: Some("skills/deploy".into()),
        }]));
        let reg = SourceSkillRegistry::new(source, SkillProvenance::AgentCreated);
        let s = reg.get("deploy").unwrap();
        assert_eq!(s.description, "ship it");
        assert_eq!(s.provenance, SkillProvenance::AgentCreated);
        assert_eq!(s.dir.as_deref(), Some("skills/deploy"));
    }

    #[test]
    fn composite_first_registry_wins_on_duplicate_id() {
        let delivered = Arc::new(InMemorySkillRegistry::from_specs([SkillSpec::new(
            "dup",
            "Delivered",
            "trusted",
            "d",
        )]));
        let authored = Arc::new(SourceSkillRegistry::new(
            Arc::new(FakeSource(vec![
                SkillFile {
                    id: "dup".into(),
                    content: "authored".into(),
                    dir: None,
                },
                SkillFile {
                    id: "extra".into(),
                    content: "x".into(),
                    dir: None,
                },
            ])),
            SkillProvenance::AgentCreated,
        ));
        let composite = CompositeSkillRegistry::new(vec![delivered, authored]);
        // delivered shadows the agent-created dup.
        assert_eq!(composite.get("dup").unwrap().name, "Delivered");
        let ids: Vec<_> = composite.list().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["dup", "extra"]);
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
