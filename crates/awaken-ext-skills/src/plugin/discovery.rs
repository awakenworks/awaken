use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use awaken_runtime::plugins::{ConfigSchema, Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::StateKeyOptions;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};
use awaken_runtime_contract::contract::context_message::ContextMessage;
use awaken_runtime_contract::model::Phase;
use awaken_runtime_contract::{PluginConfigKey, StateError};

use crate::SKILLS_DISCOVERY_PLUGIN_ID;
use crate::registry::SkillRegistry;
use crate::skill::SkillMeta;
use crate::state::SkillState;
use crate::visibility::{
    SkillVisibility, SkillVisibilityStateKey, SkillVisibilityStateValue, effective_visibility,
};

struct CatalogSkill {
    meta: SkillMeta,
    has_resources: bool,
    has_scripts: bool,
}

/// Agent-level skill catalog filter.
///
/// Stored in `AgentSpec.sections["skills"]`. A missing `allowlist` means the
/// agent follows the full published skill registry. An empty list means no
/// skills are shown to the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SkillDiscoveryConfig {
    #[serde(alias = "ids", skip_serializing_if = "Option::is_none")]
    pub allowlist: Option<Vec<String>>,
}

impl SkillDiscoveryConfig {
    fn allows(&self, skill_id: &str) -> bool {
        match &self.allowlist {
            Some(ids) => ids.iter().any(|id| id == skill_id),
            None => true,
        }
    }
}

pub struct SkillDiscoveryConfigKey;

impl PluginConfigKey for SkillDiscoveryConfigKey {
    const KEY: &'static str = "skills";
    type Config = SkillDiscoveryConfig;
}

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

    #[cfg(test)]
    pub(crate) fn render_catalog(
        &self,
        _active: &HashSet<String>,
        visibility: Option<&SkillVisibilityStateValue>,
    ) -> String {
        self.render_catalog_with_config(_active, visibility, &SkillDiscoveryConfig::default())
    }

    pub(crate) fn render_catalog_with_config(
        &self,
        _active: &HashSet<String>,
        visibility: Option<&SkillVisibilityStateValue>,
        config: &SkillDiscoveryConfig,
    ) -> String {
        let mut entries: Vec<CatalogSkill> = self
            .registry
            .snapshot()
            .values()
            .filter(|s| {
                // Filter by visibility (ADR-0020) through the single source of
                // truth: explicit Show/Hide in the run-scoped state wins, else the
                // declarative metadata policy — never failing open. This keeps
                // `model_invocable=false` skills out of the catalog even when the
                // seed missed them or the state is absent.
                config.allows(&s.meta().id)
                    && effective_visibility(s.meta(), visibility) != SkillVisibility::Hidden
            })
            .map(|s| CatalogSkill {
                meta: s.meta().clone(),
                has_resources: !s.materialized_resource_paths().is_empty(),
                has_scripts: !s.materialized_script_paths().is_empty(),
            })
            .collect();

        if entries.is_empty() {
            return String::new();
        }

        entries.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));

        let total = entries.len();
        let usage = skill_usage_block(
            entries.iter().any(|entry| entry.has_resources),
            entries.iter().any(|entry| entry.has_scripts),
        );
        const CLOSE: &str = "</available_skills>\n";
        // Budget reserved so the closing tag, an optional truncation note, and the
        // usage block always fit — this keeps the emitted structure well-formed
        // rather than hard-cutting through a tag mid-render.
        let reserve = CLOSE.len() + usage.len() + 96;

        let mut out = String::new();
        out.push_str("<available_skills>\n");

        let mut shown = 0usize;
        for entry in entries.into_iter().take(self.max_entries) {
            let m = entry.meta;
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

            let mut block = String::from("<skill>\n");
            block.push_str(&format!("<name>{}</name>\n", id));
            if !desc.trim().is_empty() {
                block.push_str(&format!("<description>{}</description>\n", desc));
            }
            block.push_str("</skill>\n");

            // Stop before this skill would push us past the budget, leaving room
            // for the closing structure. The first skill is always emitted.
            if shown > 0 && out.len() + block.len() + reserve > self.max_chars {
                break;
            }
            out.push_str(&block);
            shown += 1;
        }

        out.push_str(CLOSE);

        if shown < total {
            out.push_str(&format!(
                "Note: available_skills truncated (total={}, shown={}).\n",
                total, shown
            ));
        }

        out.push_str(&usage);

        // Last-resort cap for a pathologically small budget (smaller than the
        // structural overhead): walk back to a char boundary so `truncate` never
        // panics on a multibyte (CJK/emoji) sequence.
        if out.len() > self.max_chars {
            let mut cut = self.max_chars;
            while cut > 0 && !out.is_char_boundary(cut) {
                cut -= 1;
            }
            out.truncate(cut);
        }

        out.trim_end().to_string()
    }
}

fn skill_usage_block(has_resources: bool, has_scripts: bool) -> String {
    let mut out = String::from("<skills_usage>\n");
    out.push_str(
        "If a listed skill is relevant, call tool \"skill\" with {\"skill\": \"<id or name>\"} before answering.\n",
    );
    if has_resources {
        out.push_str(
            "Skill resources are not auto-loaded: use \"load_skill_resource\" with {\"skill\": \"<id>\", \"path\": \"references/<file>|assets/<file>\"}.\n",
        );
    }
    if has_scripts {
        out.push_str(
            "Only skills that list scripts can run them: use \"skill_script\" with {\"skill\": \"<id>\", \"script\": \"scripts/<file>\", \"args\": [..]}.\n",
        );
    }
    out.push_str("</skills_usage>");
    out
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
        let config = ctx.config::<SkillDiscoveryConfigKey>()?;
        let rendered = self
            .plugin
            .render_catalog_with_config(&active, visibility, &config);
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

    fn config_schemas(&self) -> Vec<ConfigSchema> {
        vec![
            ConfigSchema::for_key::<SkillDiscoveryConfigKey>()
                .with_display_name("Skills")
                .with_description("Restrict the skill catalog visible to this agent.")
                .with_category("context"),
        ]
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<SkillState>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: awaken_runtime_contract::state::KeyScope::Run,
        })?;

        registrar.register_key::<SkillVisibilityStateKey>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            scope: awaken_runtime_contract::state::KeyScope::Run,
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

    // No `on_activate` seed: visibility is resolved at render time by
    // `effective_visibility` (hard gate on `disable-model-invocation`, else
    // explicit runtime override, else Visible). Seeding metadata-derived defaults
    // into the run-scoped override map is unnecessary (no fail-open) and harmful
    // (a stale seed would mask later metadata changes), so the run-scoped state
    // holds only genuine runtime `Show`/`Hide` overrides.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SkillError;
    use crate::registry::InMemorySkillRegistry;
    use crate::skill::{ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind};
    use awaken_runtime_contract::state::{Snapshot, StateKey, StateMap};

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
    fn render_catalog_with_config_filters_to_skill_allowlist() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("s1"))),
            Arc::new(MockSkill(mock_meta("s2"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let catalog = plugin.render_catalog_with_config(
            &HashSet::new(),
            None,
            &SkillDiscoveryConfig {
                allowlist: Some(vec!["s2".into()]),
            },
        );

        assert!(!catalog.contains("<name>s1</name>"));
        assert!(catalog.contains("<name>s2</name>"));
    }

    #[test]
    fn skill_discovery_config_accepts_legacy_ids_alias() {
        let config: SkillDiscoveryConfig =
            serde_json::from_value(serde_json::json!({"ids": ["s1"]})).unwrap();
        assert_eq!(config.allowlist, Some(vec!["s1".to_string()]));
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
        // Hidden; a `paths`-bearing skill stays Visible (paths does not gate it).
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
            "a paths-bearing skill stays visible (paths does not gate visibility)"
        );
        assert!(
            !catalog.contains("<name>blocked</name>"),
            "disable-model-invocation skill absent from state must fall back to Hidden"
        );
    }

    #[test]
    fn render_catalog_explicit_hide_wins_but_hard_gate_holds() {
        // For a model-invocable skill, an explicit Hide overrides the Visible
        // default. For a disable-model-invocation skill, the hard gate holds even
        // against an explicit Show — it can never be revealed to the model.
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(hidden_meta("blocked"))),
            Arc::new(MockSkill(mock_meta("suppressed"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));

        let mut state = SkillVisibilityStateValue::default();
        state
            .modes
            .insert("blocked".into(), SkillVisibility::Visible); // attempted Show
        state
            .modes
            .insert("suppressed".into(), SkillVisibility::Hidden);

        let catalog = plugin.render_catalog(&HashSet::new(), Some(&state));
        assert!(
            !catalog.contains("<name>blocked</name>"),
            "the hard gate must keep a disable-model-invocation skill out, even with Show"
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

    #[tokio::test]
    async fn first_inference_excludes_disabled_skill_without_seed() {
        // End-to-end with NO seed: install → first BeforeInference hook resolves
        // visibility via effective_visibility (hard gate) against an empty state →
        // the disable-model-invocation skill never reaches the catalog, the normal
        // one does. Proves the fail-open fix without any run-start seeding.
        use awaken_runtime::state::StateStore;

        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("shown"))),
            Arc::new(MockSkill(hidden_meta("blocked"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let store = StateStore::new();
        store.install_plugin(plugin.clone()).unwrap();

        // No seed is committed; the visibility state is absent.
        assert!(store.read::<SkillVisibilityStateKey>().is_none());

        let ctx = PhaseContext::new(Phase::BeforeInference, store.snapshot());
        let hook = SkillDiscoveryHook { plugin };
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();

        let actions = cmd.scheduled_actions();
        assert_eq!(actions.len(), 1, "a catalog message must be scheduled");
        let rendered = serde_json::to_string(&actions[0].payload).unwrap();
        assert!(
            rendered.contains("shown"),
            "the model-invocable skill must appear in the first-inference catalog"
        );
        assert!(
            !rendered.contains("blocked"),
            "disable-model-invocation must hard-gate the skill out, with no seed"
        );
    }

    #[test]
    fn paths_skill_is_visible_and_action_controllable() {
        use crate::visibility::SkillVisibilityAction;
        // The agentskills spec has no path/glob conditional activation, so a
        // `paths`-bearing skill is surfaced like any other (Visible, model-invoked
        // by description). It is still controllable through the generic
        // Show/Hide action mechanism.
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![Arc::new(MockSkill(
            path_conditional_meta("cond"),
        ))]));

        // No seed; start from empty state. The paths skill resolves Visible.
        let mut state = SkillVisibilityStateValue::default();
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
