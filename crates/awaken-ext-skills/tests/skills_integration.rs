//! Integration tests for the awaken-ext-skills crate.
//!
//! Tests skill lifecycle: registration, discovery via registry,
//! embedded skill construction, activation, resource loading,
//! state management, and the subsystem facade.

use std::fs;
use std::sync::Arc;

use awaken_contract::state::StateKey;
use awaken_ext_skills::registry::{InMemorySkillRegistry, SkillRegistry};
use awaken_ext_skills::skill::{Skill, SkillResourceKind};
use awaken_ext_skills::state::{SkillState, SkillStateUpdate, SkillStateValue};
use awaken_ext_skills::{EmbeddedSkill, EmbeddedSkillData, FsSkill, SkillSubsystem};
use tempfile::TempDir;

const SKILL_A_MD: &str = "\
---
name: skill-a
description: First test skill
---
# Skill A

Use skill-a for task A.
";

const SKILL_B_MD: &str = "\
---
name: skill-b
description: Second test skill
allowed-tools: read_file search
---
# Skill B

Instructions for skill B with allowed tools.
";

fn make_fs_skills() -> (TempDir, Vec<Arc<dyn Skill>>) {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("skill-a")).unwrap();
    fs::create_dir_all(root.join("skill-b")).unwrap();
    fs::write(root.join("skill-a").join("SKILL.md"), SKILL_A_MD).unwrap();
    fs::write(root.join("skill-b").join("SKILL.md"), SKILL_B_MD).unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    (td, skills)
}

fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
    Arc::new(InMemorySkillRegistry::from_skills(skills))
}

// ── Skill registration and discovery ───────────────────────────────

#[test]
fn registry_from_fs_skills_contains_all_discovered() {
    let (_td, skills) = make_fs_skills();
    assert_eq!(skills.len(), 2);

    let registry = InMemorySkillRegistry::from_skills(skills);
    assert_eq!(registry.len(), 2);
    assert!(!registry.is_empty());
    assert!(registry.get("skill-a").is_some());
    assert!(registry.get("skill-b").is_some());
    assert!(registry.get("nonexistent").is_none());
}

#[test]
fn registry_ids_returns_all_skill_ids() {
    let (_td, skills) = make_fs_skills();
    let registry = InMemorySkillRegistry::from_skills(skills);
    let mut ids = registry.ids();
    ids.sort();
    assert_eq!(ids, vec!["skill-a", "skill-b"]);
}

#[test]
fn registry_snapshot_returns_full_map() {
    let (_td, skills) = make_fs_skills();
    let registry = InMemorySkillRegistry::from_skills(skills);
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.contains_key("skill-a"));
    assert!(snapshot.contains_key("skill-b"));
}

#[test]
fn registry_register_rejects_duplicate() {
    let (_td, skills) = make_fs_skills();
    let mut registry = InMemorySkillRegistry::new();
    for s in &skills {
        registry.register(s.clone()).unwrap();
    }
    let err = registry.register(skills[0].clone()).unwrap_err();
    // Error should reference the duplicate skill id
    let msg = err.to_string();
    assert!(msg.contains("skill-a") || msg.contains("skill-b"));
}

#[test]
fn registry_extend_upsert_overwrites_existing() {
    let (_td, skills) = make_fs_skills();
    let mut registry = InMemorySkillRegistry::from_skills(skills.clone());
    assert_eq!(registry.len(), 2);
    // Upsert same skills - should not increase count
    registry.extend_upsert(skills);
    assert_eq!(registry.len(), 2);
}

#[test]
fn empty_registry_is_empty() {
    let registry = InMemorySkillRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.ids().is_empty());
    assert!(registry.snapshot().is_empty());
    assert!(registry.get("any").is_none());
}

// ── Embedded skill lifecycle ───────────────────────────────────────

#[test]
fn embedded_skill_creation_and_registry() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[("references/guide.md", "# Guide\n\nSome content.\n")],
        assets: &[],
    };

    let skill = EmbeddedSkill::new(&data).unwrap();
    assert_eq!(skill.meta().id, "skill-a");
    assert_eq!(skill.meta().description, "First test skill");

    let mut registry = InMemorySkillRegistry::new();
    registry.register(Arc::new(skill)).unwrap();
    assert_eq!(registry.len(), 1);
    assert!(registry.get("skill-a").is_some());
}

#[tokio::test]
async fn embedded_skill_activate_returns_body() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();
    let activation = skill.activate(None).await.unwrap();
    assert!(activation.instructions.contains("Use skill-a for task A."));
}

#[tokio::test]
async fn embedded_skill_load_reference() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[("references/guide.md", "Guide content here")],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();

    let resource = skill
        .load_resource(SkillResourceKind::Reference, "references/guide.md")
        .await
        .unwrap();

    if let awaken_ext_skills::skill::SkillResource::Reference(r) = resource {
        assert_eq!(r.content, "Guide content here");
        assert!(!r.sha256.is_empty());
        assert_eq!(r.bytes, "Guide content here".len() as u64);
        assert!(!r.truncated);
    } else {
        panic!("expected reference resource");
    }
}

#[tokio::test]
async fn embedded_skill_load_multiple_references() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[
            ("references/a.md", "Content A"),
            ("references/b.md", "Content B"),
        ],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();

    let a = skill
        .load_resource(SkillResourceKind::Reference, "references/a.md")
        .await
        .unwrap();
    if let awaken_ext_skills::skill::SkillResource::Reference(r) = a {
        assert_eq!(r.content, "Content A");
    } else {
        panic!("expected reference");
    }

    let b = skill
        .load_resource(SkillResourceKind::Reference, "references/b.md")
        .await
        .unwrap();
    if let awaken_ext_skills::skill::SkillResource::Reference(r) = b {
        assert_eq!(r.content, "Content B");
    } else {
        panic!("expected reference");
    }
}

#[tokio::test]
async fn embedded_skill_load_missing_reference_errors() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();

    let err = skill
        .load_resource(SkillResourceKind::Reference, "references/missing.md")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not available"));
}

#[tokio::test]
async fn embedded_skill_script_execution_unsupported() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();

    let err = skill.run_script("scripts/run.sh", &[]).await.unwrap_err();
    assert!(err.to_string().contains("embedded skills do not support"));
}

#[test]
fn embedded_skill_invalid_md_errors() {
    let data = EmbeddedSkillData {
        skill_md: "not valid frontmatter at all",
        references: &[],
        assets: &[],
    };
    let err = EmbeddedSkill::new(&data).unwrap_err();
    assert!(err.to_string().contains("SKILL.md") || !err.to_string().is_empty());
}

// ── Embedded skill with allowed tools ──────────────────────────────

#[test]
fn embedded_skill_parses_allowed_tools() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_B_MD,
        references: &[],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();
    assert_eq!(skill.meta().allowed_tools, vec!["read_file", "search"]);
}

#[test]
fn embedded_skill_without_allowed_tools_has_empty_vec() {
    let data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[],
        assets: &[],
    };
    let skill = EmbeddedSkill::new(&data).unwrap();
    assert!(skill.meta().allowed_tools.is_empty());
}

// ── Batch creation ─────────────────────────────────────────────────

#[test]
fn embedded_skill_batch_from_static_slice() {
    let data = &[
        EmbeddedSkillData {
            skill_md: SKILL_A_MD,
            references: &[],
            assets: &[],
        },
        EmbeddedSkillData {
            skill_md: SKILL_B_MD,
            references: &[],
            assets: &[],
        },
    ];

    let skills = EmbeddedSkill::from_static_slice(data).unwrap();
    assert_eq!(skills.len(), 2);

    let registry = InMemorySkillRegistry::from_skills(skills);
    assert!(registry.get("skill-a").is_some());
    assert!(registry.get("skill-b").is_some());
}

#[test]
fn embedded_skill_batch_rejects_duplicates() {
    let data = &[
        EmbeddedSkillData {
            skill_md: SKILL_A_MD,
            references: &[],
            assets: &[],
        },
        EmbeddedSkillData {
            skill_md: SKILL_A_MD,
            references: &[],
            assets: &[],
        },
    ];

    let err = EmbeddedSkill::from_static_slice(data).unwrap_err();
    assert!(err.to_string().contains("skill-a"));
}

// ── Filesystem skill discovery ─────────────────────────────────────

#[test]
fn fs_skill_discovery_reports_warnings_for_invalid_dirs() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");

    // Valid skill
    fs::create_dir_all(root.join("good-skill")).unwrap();
    fs::write(
        root.join("good-skill").join("SKILL.md"),
        "---\nname: good-skill\ndescription: valid\n---\nBody\n",
    )
    .unwrap();

    // Invalid skill (uppercase violates naming convention)
    fs::create_dir_all(root.join("BadSkill")).unwrap();
    fs::write(
        root.join("BadSkill").join("SKILL.md"),
        "---\nname: badskill\ndescription: also valid content\n---\nBody\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].meta().id, "good-skill");
    assert!(!result.warnings.is_empty());
}

#[test]
fn fs_skill_discover_empty_directory() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(&root).unwrap();

    let result = FsSkill::discover(root).unwrap();
    assert!(result.skills.is_empty());
    assert!(result.warnings.is_empty());
}

#[tokio::test]
async fn fs_skill_read_instructions_returns_full_md() {
    let (_td, skills) = make_fs_skills();
    let skill_a = skills.iter().find(|s| s.meta().id == "skill-a").unwrap();
    let md = skill_a.read_instructions().await.unwrap();
    assert!(md.contains("name: skill-a"));
    assert!(md.contains("Use skill-a for task A."));
}

#[tokio::test]
async fn fs_skill_activate_extracts_body() {
    let (_td, skills) = make_fs_skills();
    let skill_a = skills.iter().find(|s| s.meta().id == "skill-a").unwrap();
    let activation = skill_a.activate(None).await.unwrap();
    assert!(activation.instructions.contains("Use skill-a for task A."));
    // Body should not contain frontmatter
    assert!(!activation.instructions.contains("---"));
}

// ── Skill state management ─────────────────────────────────────────

#[test]
fn skill_state_tracks_activations() {
    let mut state = SkillStateValue::default();
    assert!(state.active.is_empty());

    SkillState::apply(&mut state, SkillStateUpdate::Activate("skill-a".into()));
    assert!(state.active.contains("skill-a"));
    assert_eq!(state.active.len(), 1);

    SkillState::apply(&mut state, SkillStateUpdate::Activate("skill-b".into()));
    assert_eq!(state.active.len(), 2);

    // Idempotent
    SkillState::apply(&mut state, SkillStateUpdate::Activate("skill-a".into()));
    assert_eq!(state.active.len(), 2);
}

#[test]
fn skill_state_serde_roundtrip() {
    let mut state = SkillStateValue::default();
    state.active.insert("skill-a".into());
    state.active.insert("skill-b".into());

    let json = serde_json::to_value(&state).unwrap();
    let restored: SkillStateValue = serde_json::from_value(json).unwrap();
    assert_eq!(restored.active, state.active);
}

#[test]
fn skill_state_concurrent_merge_is_union() {
    // Simulating two parallel activations merging via commutative strategy
    let mut state1 = SkillStateValue::default();
    SkillState::apply(&mut state1, SkillStateUpdate::Activate("skill-a".into()));

    let mut state2 = SkillStateValue::default();
    SkillState::apply(&mut state2, SkillStateUpdate::Activate("skill-b".into()));

    // Merge state2 into state1
    for id in state2.active {
        SkillState::apply(&mut state1, SkillStateUpdate::Activate(id));
    }
    assert_eq!(state1.active.len(), 2);
    assert!(state1.active.contains("skill-a"));
    assert!(state1.active.contains("skill-b"));
}

// ── SkillSubsystem facade ──────────────────────────────────────────

#[test]
fn subsystem_provides_registry_access() {
    let (_td, skills) = make_fs_skills();
    let subsystem = SkillSubsystem::new(make_registry(skills));
    assert_eq!(subsystem.registry().len(), 2);
    assert!(subsystem.registry().get("skill-a").is_some());
}

#[test]
fn subsystem_creates_discovery_and_instructions_plugins() {
    let (_td, skills) = make_fs_skills();
    let subsystem = SkillSubsystem::new(make_registry(skills));

    // These should be constructible without panicking
    let _discovery = subsystem.discovery_plugin();
    let _active_instructions = subsystem.active_instructions_plugin();
}

// ── Skill metadata ─────────────────────────────────────────────────

#[test]
fn fs_skill_meta_matches_frontmatter() {
    let (_td, skills) = make_fs_skills();

    let a = skills.iter().find(|s| s.meta().id == "skill-a").unwrap();
    assert_eq!(a.meta().name, "skill-a");
    assert_eq!(a.meta().description, "First test skill");
    assert!(a.meta().allowed_tools.is_empty());

    let b = skills.iter().find(|s| s.meta().id == "skill-b").unwrap();
    assert_eq!(b.meta().name, "skill-b");
    assert_eq!(b.meta().description, "Second test skill");
    assert_eq!(b.meta().allowed_tools, vec!["read_file", "search"]);
}

// ── Cross-type integration: registry + embedded + state ────────────

#[tokio::test]
async fn full_skill_lifecycle_register_activate_load() {
    // Build registry with embedded skills
    let skill_data = EmbeddedSkillData {
        skill_md: SKILL_A_MD,
        references: &[("references/guide.md", "Reference material for skill-a")],
        assets: &[],
    };
    let skill = Arc::new(EmbeddedSkill::new(&skill_data).unwrap());
    let mut registry = InMemorySkillRegistry::new();
    registry.register(skill.clone()).unwrap();

    // Verify in registry
    let found = registry.get("skill-a").unwrap();
    assert_eq!(found.meta().id, "skill-a");

    // Activate
    let activation = found.activate(None).await.unwrap();
    assert!(activation.instructions.contains("Use skill-a for task A."));

    // Track activation in state
    let mut state = SkillStateValue::default();
    SkillState::apply(&mut state, SkillStateUpdate::Activate("skill-a".into()));
    assert!(state.active.contains("skill-a"));

    // Load a reference
    let resource = found
        .load_resource(SkillResourceKind::Reference, "references/guide.md")
        .await
        .unwrap();
    if let awaken_ext_skills::skill::SkillResource::Reference(r) = resource {
        assert_eq!(r.content, "Reference material for skill-a");
    } else {
        panic!("expected reference");
    }
}
