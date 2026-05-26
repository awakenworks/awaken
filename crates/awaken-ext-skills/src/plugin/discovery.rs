use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::model::Phase;
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::state::MutationBatch;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::StateKeyOptions;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use crate::SKILLS_DISCOVERY_PLUGIN_ID;
use crate::registry::SkillRegistry;
use crate::skill::SkillMeta;
use crate::state::SkillState;
use crate::visibility::{
    DefaultSkillVisibilityPolicy, SkillVisibility, SkillVisibilityAction, SkillVisibilityStateKey,
    SkillVisibilityStateValue, effective_visibility,
};

/// Injects a skills catalog into the LLM context so the model can discover and activate skills.
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

    /// Compute the run-start visibility seed (ADR-0020 D3).
    ///
    /// Only **Hidden** entries are emitted — i.e. skills with
    /// `disable-model-invocation` (`model_invocable == false`). Visible skills are
    /// deliberately left unseeded so they resolve through
    /// [`effective_visibility`] against live metadata; seeding them as explicit
    /// `Visible` would mask a later metadata change (e.g. registry hot-reload
    /// flipping a skill to non-invocable). The seed is applied insert-if-absent
    /// (`SeedBatch`) so it never clobbers a runtime `Show`/`Hide`.
    ///
    /// `paths` is **not** a visibility input: the agentskills spec has no
    /// path/glob conditional activation, so `paths`-bearing skills are surfaced
    /// by description like any other (model-invoked).
    pub(crate) fn seed_visibility_entries(&self) -> Vec<(String, SkillVisibility)> {
        self.registry
            .snapshot()
            .values()
            .map(|s| s.meta().clone())
            .filter(|m| DefaultSkillVisibilityPolicy.evaluate(m) == SkillVisibility::Hidden)
            .map(|m| (m.id, SkillVisibility::Hidden))
            .collect()
    }

    pub(crate) fn render_catalog(
        &self,
        _active: &HashSet<String>,
        visibility: Option<&SkillVisibilityStateValue>,
    ) -> String {
        let mut metas: Vec<SkillMeta> = self
            .registry
            .snapshot()
            .values()
            .filter(|s| {
                // Filter by visibility (ADR-0020) through the single source of
                // truth: explicit Show/Hide in the run-scoped state wins, else the
                // declarative metadata policy — never failing open. This keeps
                // `model_invocable=false` skills out of the catalog even when the
                // seed missed them or the state is absent.
                effective_visibility(s.meta(), visibility) != SkillVisibility::Hidden
            })
            .map(|s| s.meta().clone())
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
            // Append when_to_use if available (ADR-0020).
            if let Some(when) = &m.when_to_use {
                let when = when.trim();
                if !when.is_empty() {
                    desc = if desc.trim().is_empty() {
                        format!("When: {when}")
                    } else {
                        format!("{} — When: {when}", desc.trim())
                    };
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
            // Walk back to the nearest char boundary: `String::truncate` panics
            // when the index lands inside a multibyte UTF-8 sequence (CJK, emoji).
            let mut cut = self.max_chars;
            while cut > 0 && !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
        }

        out.trim_end().to_string()
    }
}

struct SkillDiscoveryHook {
    plugin: SkillDiscoveryPlugin,
}

#[async_trait]
impl PhaseHook for SkillDiscoveryHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let active: HashSet<String> = ctx
            .state::<SkillState>()
            .map(|s| s.active.iter().cloned().collect())
            .unwrap_or_default();

        let visibility = ctx.state::<SkillVisibilityStateKey>();
        let rendered = self.plugin.render_catalog(&active, visibility);
        if rendered.is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.schedule_action::<crate::AddContextMessage>(ContextMessage::system(
            "skill_catalog",
            rendered,
        ))?;
        Ok(cmd)
    }
}

impl Plugin for SkillDiscoveryPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: SKILLS_DISCOVERY_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<SkillState>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: awaken_contract::state::KeyScope::Run,
        })?;

        registrar.register_key::<SkillVisibilityStateKey>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            scope: awaken_contract::state::KeyScope::Run,
        })?;

        registrar.register_phase_hook(
            SKILLS_DISCOVERY_PLUGIN_ID,
            Phase::BeforeInference,
            SkillDiscoveryHook {
                plugin: self.clone(),
            },
        )?;

        // Register skill tools
        let registry = self.registry.clone();
        registrar.register_tool(
            crate::SKILL_ACTIVATE_TOOL_ID,
            Arc::new(crate::tools::SkillActivateTool::new(registry.clone())),
        )?;
        registrar.register_tool(
            crate::SKILL_LOAD_RESOURCE_TOOL_ID,
            Arc::new(crate::tools::LoadSkillResourceTool::new(registry.clone())),
        )?;
        registrar.register_tool(
            crate::SKILL_SCRIPT_TOOL_ID,
            Arc::new(crate::tools::SkillScriptTool::new(registry)),
        )?;

        Ok(())
    }

    /// Seed run-scoped skill visibility at run start (ADR-0020 D3): skills with
    /// `disable-model-invocation` start `Hidden`. Applied insert-if-absent
    /// (`SeedBatch`) so an already-present runtime `Show`/`Hide` is preserved
    /// across re-activation / handoff / resume. Initial visibility derives only
    /// from skill metadata; `_agent_spec` is not consulted (config-driven initial
    /// visibility is future scope, ADR-0020 D5).
    fn on_activate(
        &self,
        _agent_spec: &AgentSpec,
        patch: &mut MutationBatch,
    ) -> Result<(), StateError> {
        let entries = self.seed_visibility_entries();
        if !entries.is_empty() {
            patch.update::<SkillVisibilityStateKey>(SkillVisibilityAction::SeedBatch { entries });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SkillError;
    use crate::registry::InMemorySkillRegistry;
    use crate::skill::{ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind};
    use awaken_contract::state::{Snapshot, StateKey, StateMap};

    #[derive(Debug)]
    struct MockSkill(SkillMeta);

    #[async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.0
        }
        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok(String::new())
        }
        async fn load_resource(
            &self,
            _: SkillResourceKind,
            _: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
        async fn run_script(&self, _: &str, _: &[String]) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
    }

    fn mock_meta(id: &str) -> SkillMeta {
        SkillMeta::new(id, id, format!("{id} desc"), vec![])
    }

    fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_skills(skills))
    }

    fn make_ctx_with_active(active: Vec<String>) -> PhaseContext {
        let mut state_map = StateMap::default();
        let mut val = crate::state::SkillStateValue::default();
        for id in active {
            crate::state::SkillState::apply(&mut val, crate::state::SkillStateUpdate::Activate(id));
        }
        state_map.insert::<crate::state::SkillState>(val);
        let snapshot = Snapshot::new(0, Arc::new(state_map));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    fn make_ctx_no_state() -> PhaseContext {
        let snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    #[tokio::test]
    async fn hook_run_schedules_catalog_when_skills_exist() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill(mock_meta("s1")))];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_no_state();
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            !cmd.scheduled_actions().is_empty(),
            "should schedule AddContextMessage with catalog when skills exist"
        );
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_registry_empty() {
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![]));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_no_state();
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(cmd.is_empty(), "should be empty when no skills in registry");
    }

    #[tokio::test]
    async fn hook_run_with_active_state_still_renders_catalog() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("s1"))),
            Arc::new(MockSkill(mock_meta("s2"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_with_active(vec!["s1".into()]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(!cmd.scheduled_actions().is_empty());
    }

    #[test]
    fn render_catalog_no_description_tag_when_both_name_and_id_match_and_desc_empty() {
        let skill: Arc<dyn Skill> = Arc::new(MockSkill(SkillMeta::new("s1", "s1", "  ", vec![])));
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![skill]));
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.contains("<name>s1</name>"));
        assert!(!s.contains("<description>"));
    }

    #[test]
    fn render_catalog_truncation_handles_multibyte_without_panic() {
        // `max_chars` may fall inside a multibyte UTF-8 sequence (CJK, emoji).
        // `String::truncate` panics on a non-char-boundary index, so rendering
        // must walk back to the nearest boundary instead.
        let skill: Arc<dyn Skill> =
            Arc::new(MockSkill(SkillMeta::new("s", "s", "中".repeat(80), vec![])));
        let mut plugin = SkillDiscoveryPlugin::new(make_registry(vec![skill]));
        // Sweep cut points across several byte offsets; with 3-byte chars at
        // least some of these land mid-character.
        for max in 56..=64 {
            plugin.max_chars = max;
            let out = plugin.render_catalog(&HashSet::new(), None);
            assert!(
                out.len() <= max,
                "output must respect max_chars (max={max})"
            );
        }
    }

    #[test]
    fn render_catalog_char_limit_truncates_output() {
        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();
        for i in 0..10 {
            skills.push(Arc::new(MockSkill(mock_meta(&format!("s{i}")))));
        }
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(100, 256);
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.len() <= 256);
    }

    #[test]
    fn render_catalog_entry_limit_shows_truncation_note() {
        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();
        for i in 0..5 {
            skills.push(Arc::new(MockSkill(mock_meta(&format!("s{i}")))));
        }
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(2, 16 * 1024);
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.contains("truncated"));
        assert_eq!(s.matches("<skill>").count(), 2);
    }

    // --- Visibility seeding (ADR-0020 D3) -----------------------------------

    fn hidden_meta(id: &str) -> SkillMeta {
        let mut m = mock_meta(id);
        m.model_invocable = false; // frontmatter `disable-model-invocation: true`
        m
    }

    fn path_conditional_meta(id: &str) -> SkillMeta {
        let mut m = mock_meta(id);
        m.paths = vec!["src/**/*.rs".to_string()];
        m
    }

    #[test]
    fn on_activate_seeds_visibility_state_key() {
        use awaken_contract::registry_spec::AgentSpec;
        use awaken_contract::state::MutationBatch;

        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill(hidden_meta("no_model_invoke")))];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        let spec = AgentSpec::new("agent");
        let mut batch = MutationBatch::new();
        Plugin::on_activate(&plugin, &spec, &mut batch).unwrap();

        assert!(
            !batch.is_empty(),
            "on_activate must seed the visibility state when skills exist"
        );
    }

    #[test]
    fn seeded_visibility_excludes_hidden_skill_from_catalog() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("shown"))),
            Arc::new(MockSkill(hidden_meta("nope"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        // Apply the seed exactly as SkillVisibilityStateKey::apply(SetBatch) would.
        let mut state = SkillVisibilityStateValue::default();
        for (id, vis) in plugin.seed_visibility_entries() {
            state.modes.insert(id, vis);
        }

        let catalog = plugin.render_catalog(&HashSet::new(), Some(&state));
        assert!(catalog.contains("<name>shown</name>"));
        assert!(
            !catalog.contains("<name>nope</name>"),
            "disable-model-invocation skill must not appear in the catalog"
        );
    }

    // --- Fail-open visibility default (FIX #12) ------------------------------

    #[test]
    fn render_catalog_none_visibility_hides_non_model_invocable() {
        // With no visibility state at all, a `model_invocable=false` skill must
        // fall back to the declarative metadata policy (Hidden), while a normal
        // skill remains visible.
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("normal"))),
            Arc::new(MockSkill(hidden_meta("no_invoke"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        let catalog = plugin.render_catalog(&HashSet::new(), None);
        assert!(
            catalog.contains("<name>normal</name>"),
            "a normal skill must render when visibility state is missing"
        );
        assert!(
            !catalog.contains("<name>no_invoke</name>"),
            "disable-model-invocation skill must not fail open when state is None"
        );
    }

    #[test]
    fn render_catalog_omitted_skill_falls_back_to_metadata_policy() {
        // A skill absent from the state map must not fail open blindly: it falls
        // back to the metadata policy. A `disable-model-invocation` skill stays
        // Hidden; a path-conditional skill stays Visible (hiding deferred).
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("shown"))),
            Arc::new(MockSkill(path_conditional_meta("cond"))),
            Arc::new(MockSkill(hidden_meta("blocked"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        // State knows about `shown` only; `cond` and `blocked` are absent.
        let mut state = SkillVisibilityStateValue::default();
        state.modes.insert("shown".into(), SkillVisibility::Visible);

        let catalog = plugin.render_catalog(&HashSet::new(), Some(&state));
        assert!(catalog.contains("<name>shown</name>"));
        assert!(
            catalog.contains("<name>cond</name>"),
            "path-conditional skill stays visible until the promote hook exists"
        );
        assert!(
            !catalog.contains("<name>blocked</name>"),
            "disable-model-invocation skill absent from state must fall back to Hidden"
        );
    }

    #[test]
    fn render_catalog_explicit_state_overrides_metadata_policy() {
        // Explicit Show on an otherwise-hidden skill wins; explicit Hide on an
        // otherwise-visible skill wins.
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(hidden_meta("promoted"))),
            Arc::new(MockSkill(mock_meta("suppressed"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        let mut state = SkillVisibilityStateValue::default();
        state
            .modes
            .insert("promoted".into(), SkillVisibility::Visible);
        state
            .modes
            .insert("suppressed".into(), SkillVisibility::Hidden);

        let catalog = plugin.render_catalog(&HashSet::new(), Some(&state));
        assert!(
            catalog.contains("<name>promoted</name>"),
            "explicit Show must override the Hidden metadata default"
        );
        assert!(
            !catalog.contains("<name>suppressed</name>"),
            "explicit Hide must override the Visible metadata default"
        );
    }

    fn make_ctx_with_visibility(entries: Vec<(String, SkillVisibility)>) -> PhaseContext {
        let mut state_map = StateMap::default();
        let mut val = SkillVisibilityStateValue::default();
        for (id, vis) in entries {
            val.modes.insert(id, vis);
        }
        state_map.insert::<SkillVisibilityStateKey>(val);
        let snapshot = Snapshot::new(0, Arc::new(state_map));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    #[tokio::test]
    async fn hook_skips_catalog_when_all_skills_hidden() {
        // The runtime read path: the hook reads SkillVisibilityStateKey from the
        // phase context and must honor it. All skills hidden => no catalog message.
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill(mock_meta("only")))];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_with_visibility(vec![("only".into(), SkillVisibility::Hidden)]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            cmd.is_empty(),
            "hook must emit no catalog when every skill is hidden"
        );
    }

    #[tokio::test]
    async fn hook_renders_only_visible_skills_from_seeded_state() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("shown"))),
            Arc::new(MockSkill(mock_meta("gone"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook {
            plugin: plugin.clone(),
        };

        let ctx = make_ctx_with_visibility(vec![
            ("shown".into(), SkillVisibility::Visible),
            ("gone".into(), SkillVisibility::Hidden),
        ]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            !cmd.scheduled_actions().is_empty(),
            "a visible skill must still produce a catalog"
        );
    }

    #[test]
    fn on_activate_empty_registry_produces_no_seed() {
        use awaken_contract::registry_spec::AgentSpec;
        use awaken_contract::state::MutationBatch;

        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![]));
        let spec = AgentSpec::new("agent");
        let mut batch = MutationBatch::new();
        Plugin::on_activate(&plugin, &spec, &mut batch).unwrap();

        assert!(batch.is_empty(), "empty registry has nothing to seed");
    }

    #[tokio::test]
    async fn activation_seed_committed_and_first_inference_excludes_blocked_skill() {
        // End-to-end: install → run-start on_activate seed → commit through the
        // real StateStore → first BeforeInference hook reads the committed state
        // → catalog excludes the disable-model-invocation skill.
        use awaken_contract::registry_spec::AgentSpec;
        use awaken_contract::state::MutationBatch;
        use awaken_runtime::state::StateStore;

        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("shown"))),
            Arc::new(MockSkill(hidden_meta("blocked"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        // Install so the run-scoped visibility key is registered for commit.
        let store = StateStore::new();
        store.install_plugin(plugin.clone()).unwrap();

        // Run-start activation produces the seed; commit it for real.
        let mut batch = MutationBatch::new();
        Plugin::on_activate(&plugin, &AgentSpec::new("agent"), &mut batch).unwrap();
        store.commit(batch).unwrap();

        // The committed seed records only the blocked skill (Hidden). The visible
        // skill is left unseeded — it resolves Visible via the metadata fallback.
        let seeded = store
            .read::<SkillVisibilityStateKey>()
            .expect("on_activate must commit a visibility seed");
        assert_eq!(seeded.explicit("blocked"), Some(SkillVisibility::Hidden));
        assert_eq!(
            seeded.explicit("shown"),
            None,
            "a model-invocable skill must not be seeded with an explicit entry"
        );

        // First BeforeInference: the hook reads the committed snapshot state.
        let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
        let hook = SkillDiscoveryHook { plugin };
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();

        let actions = cmd.scheduled_actions();
        assert_eq!(
            actions.len(),
            1,
            "a single catalog message must be scheduled"
        );
        let rendered = serde_json::to_string(&actions[0].payload).unwrap();
        assert!(
            rendered.contains("shown"),
            "the visible skill must appear in the first-inference catalog"
        );
        assert!(
            !rendered.contains("blocked"),
            "a seeded-Hidden skill must never reach the first-inference catalog"
        );
    }

    #[tokio::test]
    async fn runtime_override_survives_reactivation_seed() {
        // A runtime Hide on a normally-visible skill must NOT be reset when the
        // plugin re-activates (handoff / resume / sub-agent) and re-seeds. The
        // insert-if-absent SeedBatch guarantees the explicit override wins.
        use awaken_contract::registry_spec::AgentSpec;
        use awaken_contract::state::MutationBatch;
        use awaken_runtime::state::StateStore;

        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("a"))),
            Arc::new(MockSkill(hidden_meta("blocked"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let store = StateStore::new();
        store.install_plugin(plugin.clone()).unwrap();

        // First activation seeds blocked=Hidden.
        let mut batch = MutationBatch::new();
        Plugin::on_activate(&plugin, &AgentSpec::new("agent"), &mut batch).unwrap();
        store.commit(batch).unwrap();

        // A tool/user hides the otherwise-visible skill "a" at runtime.
        let mut override_batch = MutationBatch::new();
        crate::visibility::hide_skill(&mut override_batch, "a");
        store.commit(override_batch).unwrap();

        // Re-activation re-runs the seed (insert-if-absent).
        let mut reseed = MutationBatch::new();
        Plugin::on_activate(&plugin, &AgentSpec::new("agent"), &mut reseed).unwrap();
        store.commit(reseed).unwrap();

        let state = store.read::<SkillVisibilityStateKey>().unwrap();
        assert_eq!(
            state.explicit("a"),
            Some(SkillVisibility::Hidden),
            "runtime Hide must survive re-activation"
        );
        assert_eq!(state.explicit("blocked"), Some(SkillVisibility::Hidden));
    }

    #[test]
    fn seed_visibility_entries_only_covers_hidden_skills() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("a"))),             // visible
            Arc::new(MockSkill(hidden_meta("b"))),           // model_invocable=false
            Arc::new(MockSkill(path_conditional_meta("c"))), // paths, still visible
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let entries = plugin.seed_visibility_entries();
        assert_eq!(
            entries,
            vec![("b".to_string(), SkillVisibility::Hidden)],
            "only the disable-model-invocation skill is seeded"
        );
    }

    #[test]
    fn paths_skill_is_visible_and_action_controllable() {
        // The agentskills spec has no path/glob conditional activation, so a
        // `paths`-bearing skill is surfaced like any other (Visible, model-invoked
        // by description). It is still controllable through the generic
        // Show/Hide action mechanism.
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![Arc::new(MockSkill(
            path_conditional_meta("cond"),
        ))]));

        let mut state = SkillVisibilityStateValue::default();
        for (id, vis) in plugin.seed_visibility_entries() {
            state.modes.insert(id, vis);
        }
        assert!(
            plugin
                .render_catalog(&HashSet::new(), Some(&state))
                .contains("<name>cond</name>"),
            "a paths-bearing skill is visible by default (paths does not gate visibility)"
        );

        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Hide {
                skill_id: "cond".into(),
            },
        );
        assert!(
            !plugin
                .render_catalog(&HashSet::new(), Some(&state))
                .contains("<name>cond</name>"),
            "an explicit Hide must remove the skill from the catalog"
        );

        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::ShowBatch {
                skill_ids: vec!["cond".into()],
            },
        );
        assert!(
            plugin
                .render_catalog(&HashSet::new(), Some(&state))
                .contains("<name>cond</name>"),
            "a subsequent ShowBatch must re-promote the skill"
        );
    }
}
