use crate::tool_filter::{is_scope_allowed, SCOPE_ALLOWED_SKILLS_KEY, SCOPE_EXCLUDED_SKILLS_KEY};
use crate::{SkillMeta, SkillRegistry, SkillState, SKILLS_DISCOVERY_PLUGIN_ID};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::{BeforeInferenceContext, PluginPhaseContext};
use tirea_contract::runtime::plugin::AgentPlugin;

/// Injects a skills catalog into the LLM context so the model can discover and activate skills.
///
/// This is intentionally non-persistent: the catalog is rebuilt from the registry snapshot per step.
#[derive(Clone)]
pub struct SkillDiscoveryPlugin {
    registry: Arc<dyn SkillRegistry>,
    max_entries: usize,
    max_chars: usize,
}

impl std::fmt::Debug for SkillDiscoveryPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillDiscoveryPlugin")
            .field("max_entries", &self.max_entries)
            .field("max_chars", &self.max_chars)
            .finish_non_exhaustive()
    }
}

impl SkillDiscoveryPlugin {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            max_entries: 32,
            max_chars: 16 * 1024,
        }
    }

    pub fn with_limits(mut self, max_entries: usize, max_chars: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self.max_chars = max_chars.max(256);
        self
    }

    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn render_catalog(
        &self,
        _active: &HashSet<String>,
        runtime: Option<&tirea_contract::RunConfig>,
    ) -> String {
        let mut metas: Vec<SkillMeta> = self
            .registry
            .snapshot()
            .values()
            .map(|s| s.meta().clone())
            .filter(|m| {
                is_scope_allowed(
                    runtime,
                    &m.id,
                    SCOPE_ALLOWED_SKILLS_KEY,
                    SCOPE_EXCLUDED_SKILLS_KEY,
                )
            })
            .collect();

        if metas.is_empty() {
            return String::new();
        }

        metas.sort_by(|a, b| a.id.cmp(&b.id));

        let total = metas.len();
        let mut out = String::new();
        out.push_str("<available_skills>\n");

        let mut shown = 0usize;
        for m in metas.into_iter().take(self.max_entries) {
            let id = Self::escape_text(&m.id);
            let mut desc = m.description.clone();
            if m.name != m.id && !m.name.trim().is_empty() {
                if desc.trim().is_empty() {
                    desc = m.name.clone();
                } else {
                    desc = format!("{}: {}", m.name.trim(), desc.trim());
                }
            }
            let desc = Self::escape_text(&desc);

            out.push_str("<skill>\n");
            out.push_str(&format!("<name>{}</name>\n", id));
            if !desc.trim().is_empty() {
                out.push_str(&format!("<description>{}</description>\n", desc));
            }
            out.push_str("</skill>\n");
            shown += 1;

            if out.len() >= self.max_chars {
                break;
            }
        }

        out.push_str("</available_skills>\n");

        if shown < total {
            out.push_str(&format!(
                "Note: available_skills truncated (total={}, shown={}).\n",
                total, shown
            ));
        }

        out.push_str("<skills_usage>\n");
        out.push_str("If a listed skill is relevant, call tool \"skill\" with {\"skill\": \"<id or name>\"} before answering.\n");
        out.push_str("Skill resources are not auto-loaded: use \"load_skill_resource\" with {\"skill\": \"<id>\", \"path\": \"references/<file>|assets/<file>\"}.\n");
        out.push_str("To run skill scripts: use \"skill_script\" with {\"skill\": \"<id>\", \"script\": \"scripts/<file>\", \"args\": [..]}.\n");
        out.push_str("</skills_usage>");

        if out.len() > self.max_chars {
            out.truncate(self.max_chars);
        }

        out.trim_end().to_string()
    }
}

#[async_trait]
impl AgentPlugin for SkillDiscoveryPlugin {
    fn id(&self) -> &str {
        SKILLS_DISCOVERY_PLUGIN_ID
    }

    async fn before_inference(&self, step: &mut BeforeInferenceContext<'_, '_>) {
        let (active, scope) = {
            let skill_state = step.state_of::<SkillState>();
            let active: HashSet<String> = skill_state
                .active()
                .ok()
                .unwrap_or_default()
                .into_iter()
                .collect();
            // Clone scope to release immutable borrow on step
            (active, step.run_config().clone())
        };

        let rendered = self.render_catalog(&active, Some(&scope));
        if rendered.is_empty() {
            return;
        }

        step.add_system_context(rendered);
    }
}

#[async_trait]
impl AgentBehavior for SkillDiscoveryPlugin {
    fn id(&self) -> &str {
        SKILLS_DISCOVERY_PLUGIN_ID
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let active: HashSet<String> = ctx
            .snapshot_of::<SkillState>()
            .ok()
            .map(|s| s.active.into_iter().collect())
            .unwrap_or_default();

        let rendered = self.render_catalog(&active, Some(ctx.run_config()));
        if rendered.is_empty() {
            return PhaseOutput::default();
        }

        PhaseOutput::new().system_context(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsSkill, InMemorySkillRegistry, Skill};
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;
    use tirea_contract::runtime::plugin::phase::{BeforeInferenceContext, StepContext};
    use tirea_contract::testing::TestFixture;
    use tirea_contract::thread::Thread;
    use tirea_contract::runtime::tool_call::ToolDescriptor;

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

    async fn run_before_inference(plugin: &SkillDiscoveryPlugin, step: &mut StepContext<'_>) {
        let mut ctx = BeforeInferenceContext::new(step);
        AgentPlugin::before_inference(plugin, &mut ctx).await;
    }

    #[tokio::test]
    async fn injects_catalog_with_usage() {
        let fix = TestFixture::new();
        let (_td, skills) = make_skills();
        let p = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(10, 8 * 1024);
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        assert_eq!(step.system_context.len(), 1);
        let s = &step.system_context[0];
        assert!(s.contains("<available_skills>"));
        assert!(s.contains("<skills_usage>"));
        assert!(s.contains("&amp;"));
        assert!(s.contains("&lt;"));
        assert!(s.contains("&gt;"));
    }

    #[tokio::test]
    async fn marks_active_skills() {
        let fix = TestFixture::new();
        let (_td, skills) = make_skills();
        let p = SkillDiscoveryPlugin::new(make_registry(skills));
        let thread = Thread::with_initial_state(
            "s",
            json!({
                "skills": {
                    "active": ["a"],
                    "instructions": {"a": "Do X"},
                    "references": {},
                    "scripts": {}
                }
            }),
        );
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        let s = &step.system_context[0];
        assert!(s.contains("<name>a-skill</name>"));
    }

    #[tokio::test]
    async fn does_not_inject_when_skills_empty() {
        let fix = TestFixture::new();
        let p = SkillDiscoveryPlugin::new(make_registry(vec![]));
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        assert!(step.system_context.is_empty());
    }

    #[tokio::test]
    async fn does_not_inject_when_all_skills_invalid() {
        let fix = TestFixture::new();
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
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        assert!(step.system_context.is_empty());
    }

    #[tokio::test]
    async fn injects_only_valid_skills_and_never_warnings() {
        let fix = TestFixture::new();
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
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;

        assert_eq!(step.system_context.len(), 1);
        let s = &step.system_context[0];
        assert!(s.contains("<name>good-skill</name>"));
        assert!(!s.contains("BadSkill"));
        assert!(!s.contains("skills_warnings"));
        assert!(!s.contains("Skipped skill"));
    }

    #[tokio::test]
    async fn truncates_by_entry_limit_and_emits_note() {
        let fix = TestFixture::new();
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
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        let s = &step.system_context[0];
        assert!(s.contains("<available_skills>"));
        assert!(s.contains("truncated"));
        assert_eq!(s.matches("<skill>").count(), 2);
    }

    #[tokio::test]
    async fn truncates_by_char_limit() {
        let fix = TestFixture::new();
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
        let thread = Thread::with_initial_state("s", json!({}));
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        let s = &step.system_context[0];
        assert!(s.len() <= 256);
    }

    #[tokio::test]
    async fn filters_catalog_by_runtime_skill_policy() {
        let mut fix = TestFixture::new();
        let (_td, skills) = make_skills();
        let p = SkillDiscoveryPlugin::new(make_registry(skills));
        let thread = Thread::with_initial_state("s", json!({}));
        fix.run_config
            .set(SCOPE_ALLOWED_SKILLS_KEY, vec!["a-skill"])
            .unwrap();
        let mut step = StepContext::new(
            fix.ctx(),
            &thread.id,
            &thread.messages,
            vec![ToolDescriptor::new("t", "t", "t")],
        );
        run_before_inference(&p, &mut step).await;
        assert_eq!(step.system_context.len(), 1);
        let s = &step.system_context[0];
        assert!(s.contains("<name>a-skill</name>"));
        assert!(!s.contains("<name>b-skill</name>"));
    }
}
