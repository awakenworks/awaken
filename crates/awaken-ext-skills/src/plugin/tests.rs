use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::sync::Arc;

use tempfile::TempDir;

use crate::registry::SkillRegistry;
use crate::registry::{FsSkill, InMemorySkillRegistry};
use crate::skill::Skill;

use super::{ActiveSkillInstructionsPlugin, SkillDiscoveryPlugin};

fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
    Arc::new(InMemorySkillRegistry::from_skills(skills))
}

fn make_skills() -> (TempDir, Vec<Arc<dyn Skill>>) {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("a-skill")).unwrap();
    fs::create_dir_all(root.join("b-skill")).unwrap();
    let mut fa = fs::File::create(root.join("a-skill").join("SKILL.md")).unwrap();
    fa.write_all(b"---\nname: a-skill\ndescription: Desc & \"<tag>\"\n---\nBody\n")
        .unwrap();
    fs::write(
        root.join("b-skill").join("SKILL.md"),
        "---\nname: b-skill\ndescription: ok\n---\nBody\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    (td, skills)
}

#[test]
fn render_catalog_contains_available_skills_and_usage() {
    let (_td, skills) = make_skills();
    let p = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(10, 8 * 1024);
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.contains("<available_skills>"));
    assert!(s.contains("<skills_usage>"));
    assert!(s.contains("&amp;"));
    assert!(s.contains("&lt;"));
    assert!(s.contains("&gt;"));
}

#[test]
fn render_catalog_marks_skills() {
    let (_td, skills) = make_skills();
    let p = SkillDiscoveryPlugin::new(make_registry(skills));
    let active: HashSet<String> = ["a-skill".to_string()].into();
    let s = p.render_catalog(&active);
    assert!(s.contains("<name>a-skill</name>"));
}

#[test]
fn render_catalog_returns_empty_for_no_skills() {
    let p = SkillDiscoveryPlugin::new(make_registry(vec![]));
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.is_empty());
}

#[test]
fn render_catalog_empty_for_all_invalid_skills() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("BadSkill")).unwrap();
    fs::write(
        root.join("BadSkill").join("SKILL.md"),
        "---\nname: badskill\ndescription: ok\n---\nBody\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    assert!(result.skills.is_empty());

    let skills = FsSkill::into_arc_skills(result.skills);
    let p = SkillDiscoveryPlugin::new(make_registry(skills));
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.is_empty());
}

#[test]
fn render_catalog_only_valid_skills_and_never_warnings() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("good-skill")).unwrap();
    fs::create_dir_all(root.join("BadSkill")).unwrap();
    fs::write(
        root.join("good-skill").join("SKILL.md"),
        "---\nname: good-skill\ndescription: ok\n---\nBody\n",
    )
    .unwrap();
    fs::write(
        root.join("BadSkill").join("SKILL.md"),
        "---\nname: badskill\ndescription: ok\n---\nBody\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let p = SkillDiscoveryPlugin::new(make_registry(skills));
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.contains("<name>good-skill</name>"));
    assert!(!s.contains("BadSkill"));
}

#[test]
fn render_catalog_truncates_by_entry_limit() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    for i in 0..5 {
        let name = format!("s{i}");
        fs::create_dir_all(root.join(&name)).unwrap();
        fs::write(
            root.join(&name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: ok\n---\nBody\n"),
        )
        .unwrap();
    }
    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let p = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(2, 8 * 1024);
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.contains("<available_skills>"));
    assert!(s.contains("truncated"));
    assert_eq!(s.matches("<skill>").count(), 2);
}

#[test]
fn render_catalog_truncates_by_char_limit() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("s")).unwrap();
    fs::write(
        root.join("s").join("SKILL.md"),
        "---\nname: s\ndescription: A very long description\n---\nBody",
    )
    .unwrap();
    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let p = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(10, 256);
    let active = HashSet::new();
    let s = p.render_catalog(&active);
    assert!(s.len() <= 256);
}

#[tokio::test]
async fn active_instructions_renders_for_activated_skill() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("docx")).unwrap();
    fs::write(
        root.join("docx").join("SKILL.md"),
        "---\nname: docx\ndescription: ok\n---\n# DOCX\nUse docx-js.\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["docx".to_string()])
        .await;
    assert!(rendered.contains("<active_skill_instructions>"));
    assert!(rendered.contains("Use docx-js."));
}

#[tokio::test]
async fn active_instructions_empty_for_no_active_skills() {
    let (_td, skills) = make_skills();
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin.render_active_instructions(vec![]).await;
    assert!(rendered.is_empty());
}

#[tokio::test]
async fn active_instructions_skips_unknown_skill() {
    let (_td, skills) = make_skills();
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["nonexistent-skill".to_string()])
        .await;
    assert!(
        rendered.is_empty(),
        "unknown skill should produce empty output"
    );
}

#[tokio::test]
async fn active_instructions_deduplicates_ids() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("docx")).unwrap();
    fs::write(
        root.join("docx").join("SKILL.md"),
        "---\nname: docx\ndescription: ok\n---\nUse docx.\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["docx".to_string(), "docx".to_string()])
        .await;
    // Should only contain one skill_instruction block despite duplicate IDs
    assert_eq!(
        rendered.matches("<skill_instruction").count(),
        1,
        "duplicate IDs should be deduped"
    );
}

#[tokio::test]
async fn active_instructions_renders_sorted_order() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("z-skill")).unwrap();
    fs::create_dir_all(root.join("a-skill")).unwrap();
    fs::write(
        root.join("z-skill").join("SKILL.md"),
        "---\nname: z-skill\ndescription: ok\n---\nZ instructions.\n",
    )
    .unwrap();
    fs::write(
        root.join("a-skill").join("SKILL.md"),
        "---\nname: a-skill\ndescription: ok\n---\nA instructions.\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["z-skill".to_string(), "a-skill".to_string()])
        .await;
    let a_pos = rendered.find("a-skill").expect("should contain a-skill");
    let z_pos = rendered.find("z-skill").expect("should contain z-skill");
    assert!(
        a_pos < z_pos,
        "skills should be rendered in sorted order (a before z)"
    );
}

#[tokio::test]
async fn active_instructions_skips_skill_with_empty_body() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("empty-body")).unwrap();
    fs::write(
        root.join("empty-body").join("SKILL.md"),
        "---\nname: empty-body\ndescription: ok\n---\n   \n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["empty-body".to_string()])
        .await;
    assert!(
        rendered.is_empty(),
        "skill with whitespace-only body should produce empty output"
    );
}

#[tokio::test]
async fn active_instructions_mixed_valid_and_unknown() {
    let td = TempDir::new().unwrap();
    let root = td.path().join("skills");
    fs::create_dir_all(root.join("real-skill")).unwrap();
    fs::write(
        root.join("real-skill").join("SKILL.md"),
        "---\nname: real-skill\ndescription: ok\n---\nReal instructions.\n",
    )
    .unwrap();

    let result = FsSkill::discover(root).unwrap();
    let skills = FsSkill::into_arc_skills(result.skills);
    let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
    let rendered = plugin
        .render_active_instructions(vec!["nonexistent".to_string(), "real-skill".to_string()])
        .await;
    assert!(rendered.contains("<active_skill_instructions>"));
    assert!(rendered.contains("Real instructions."));
    assert!(!rendered.contains("nonexistent"));
}
