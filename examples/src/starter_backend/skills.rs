use std::path::PathBuf;
use std::sync::Arc;

use awaken_contract::{BuiltinSpec, SkillSpecSink, parse_skill_allowed_tool_token};
use awaken_ext_skills::{
    ActiveSkillInstructionsPlugin, CompositeSkillRegistry, ConfigSkillRegistry, EmbeddedSkill,
    EmbeddedSkillData, FsSkillRegistryManager, InMemorySkillRegistry, Skill, SkillDiscoveryPlugin,
    SkillError, SkillRegistry, SkillRegistryManagerError, snapshot_skill_specs,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::plugins::Plugin;

pub struct StarterSkillRegistries {
    seed_registry: Arc<dyn SkillRegistry>,
    live_registry: Arc<dyn SkillRegistry>,
    managed_registry: Arc<ConfigSkillRegistry>,
}

impl StarterSkillRegistries {
    pub fn live_registry(&self) -> Arc<dyn SkillRegistry> {
        self.live_registry.clone()
    }

    pub fn spec_sink(&self) -> Arc<dyn SkillSpecSink> {
        self.managed_registry.clone()
    }

    pub async fn seed_specs(&self) -> Result<Vec<BuiltinSpec>, SkillError> {
        let mut specs = Vec::new();
        for skill in self.seed_registry.snapshot().into_values() {
            if let Some(reason) = db_managed_unsupported_reason(skill.as_ref()) {
                tracing::warn!(
                    skill_id = %skill.meta().id,
                    reason = %reason,
                    "Skills: using local skill registry fallback; not seeding as DB-managed skill"
                );
                continue;
            }
            let single = InMemorySkillRegistry::from_skills(vec![skill]);
            let single_specs = snapshot_skill_specs(&single).await?;
            if single_specs.iter().any(|spec| {
                if let BuiltinSpec::Skill(skill) = spec {
                    let value = match serde_json::to_value(skill) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(
                                skill_id = %skill.id,
                                error = %error,
                                "Skills: local skill could not be serialized; not seeding"
                            );
                            return true;
                        }
                    };
                    if let Err(error) = awaken_contract::validate_skill_spec(value) {
                        tracing::warn!(
                            skill_id = %skill.id,
                            error = %error,
                            "Skills: local skill is not DB-managed compatible; not seeding"
                        );
                        return true;
                    }
                }
                false
            }) {
                continue;
            }
            specs.extend(single_specs);
        }
        specs.sort_by(|a, b| a.id().cmp(b.id()));
        Ok(specs)
    }
}

pub fn install_plugins(
    mut builder: AgentRuntimeBuilder,
    has_skills_dir: bool,
) -> (AgentRuntimeBuilder, StarterSkillRegistries) {
    let seed_registry = seed_skill_registry(has_skills_dir);
    let managed_registry = Arc::new(ConfigSkillRegistry::new());
    let fallback_registry: Arc<dyn SkillRegistry> = Arc::new(UnsupportedSeedSkillRegistry {
        inner: seed_registry.clone(),
    });
    let live_registry: Arc<dyn SkillRegistry> = Arc::new(StarterLiveSkillRegistry {
        managed: managed_registry.clone(),
        fallback: fallback_registry,
    });

    builder = builder.with_plugin(
        "skills-discovery",
        Arc::new(SkillDiscoveryPlugin::new(live_registry.clone())) as Arc<dyn Plugin>,
    );
    builder = builder.with_plugin(
        "skills-active-instructions",
        Arc::new(ActiveSkillInstructionsPlugin::new(live_registry.clone())) as Arc<dyn Plugin>,
    );

    (
        builder,
        StarterSkillRegistries {
            seed_registry,
            live_registry,
            managed_registry,
        },
    )
}

#[derive(Clone)]
struct StarterLiveSkillRegistry {
    managed: Arc<ConfigSkillRegistry>,
    fallback: Arc<dyn SkillRegistry>,
}

impl SkillRegistry for StarterLiveSkillRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        self.managed.get(id).or_else(|| self.fallback.get(id))
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> std::collections::HashMap<String, Arc<dyn Skill>> {
        let mut snapshot = self.fallback.snapshot();
        snapshot.extend(self.managed.snapshot());
        snapshot
    }

    fn start_periodic_refresh(
        &self,
        interval: std::time::Duration,
    ) -> Result<(), SkillRegistryManagerError> {
        self.fallback.start_periodic_refresh(interval)
    }

    fn stop_periodic_refresh(&self) -> bool {
        self.fallback.stop_periodic_refresh()
    }

    fn periodic_refresh_running(&self) -> bool {
        self.fallback.periodic_refresh_running()
    }
}

#[derive(Clone)]
struct UnsupportedSeedSkillRegistry {
    inner: Arc<dyn SkillRegistry>,
}

impl SkillRegistry for UnsupportedSeedSkillRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        self.inner
            .get(id)
            .filter(|skill| db_managed_unsupported_reason(skill.as_ref()).is_some())
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> std::collections::HashMap<String, Arc<dyn Skill>> {
        self.inner
            .snapshot()
            .into_iter()
            .filter(|(_, skill)| db_managed_unsupported_reason(skill.as_ref()).is_some())
            .collect()
    }

    fn start_periodic_refresh(
        &self,
        interval: std::time::Duration,
    ) -> Result<(), SkillRegistryManagerError> {
        self.inner.start_periodic_refresh(interval)
    }

    fn stop_periodic_refresh(&self) -> bool {
        self.inner.stop_periodic_refresh()
    }

    fn periodic_refresh_running(&self) -> bool {
        self.inner.periodic_refresh_running()
    }
}

fn db_managed_unsupported_reason(skill: &dyn Skill) -> Option<String> {
    let meta = skill.meta();
    if !meta.paths.is_empty() {
        return Some("paths are not supported for DB-managed skills".to_string());
    }
    if !skill.materialized_resource_paths().is_empty() {
        return Some("resources are not persisted for DB-managed skills".to_string());
    }
    if !skill.materialized_script_paths().is_empty() {
        return Some("scripts are not persisted for DB-managed skills".to_string());
    }
    let mut argument_names = std::collections::HashSet::new();
    for argument in &meta.arguments {
        if argument.name.trim() != argument.name {
            return Some(
                "argument names with surrounding whitespace are not supported for DB-managed skills"
                    .to_string(),
            );
        }
        if !argument_names.insert(argument.name.as_str()) {
            return Some(
                "duplicate argument names are not supported for DB-managed skills".to_string(),
            );
        }
    }
    for token in &meta.allowed_tools {
        if parse_skill_allowed_tool_token(token.clone())
            .map(|parsed| parsed.scope.is_some())
            .unwrap_or(false)
        {
            return Some(
                "scoped allowed_tools are not supported for DB-managed skills".to_string(),
            );
        }
    }
    None
}

fn seed_skill_registry(has_skills_dir: bool) -> Arc<dyn SkillRegistry> {
    static GREETING_SKILL_MD: &str = "---
name: greeting
description: Adds friendly greeting behavior
---
Always greet the user warmly and ask how you can help today.
";
    let embedded_data = EmbeddedSkillData {
        skill_md: GREETING_SKILL_MD,
        references: &[],
        assets: &[],
    };
    let embedded_skill =
        EmbeddedSkill::new(&embedded_data).expect("invalid embedded greeting skill");
    let embedded_registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(
        vec![Arc::new(embedded_skill)],
    ));

    if !has_skills_dir {
        return embedded_registry;
    }

    match FsSkillRegistryManager::discover_roots(vec![PathBuf::from("./skills")]) {
        Ok(fs_manager) => {
            let fs_registry: Arc<dyn SkillRegistry> = Arc::new(fs_manager);
            match CompositeSkillRegistry::try_new([embedded_registry.clone(), fs_registry]) {
                Ok(composite) => Arc::new(composite),
                Err(e) => {
                    tracing::warn!(error = %e, "Skills: composite merge conflict, using embedded only");
                    embedded_registry
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Skills: failed to discover from ./skills/");
            embedded_registry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::SkillSpec;

    fn embedded_skill(skill_md: &'static str) -> Arc<dyn Skill> {
        Arc::new(
            EmbeddedSkill::new(&EmbeddedSkillData {
                skill_md,
                references: &[],
                assets: &[],
            })
            .expect("valid embedded skill"),
        )
    }

    fn embedded_skill_with_reference(skill_md: &'static str) -> Arc<dyn Skill> {
        Arc::new(
            EmbeddedSkill::new(&EmbeddedSkillData {
                skill_md,
                references: &[("references/schema.md", "schema")],
                assets: &[],
            })
            .expect("valid embedded skill with reference"),
        )
    }

    #[tokio::test]
    async fn starter_seed_skips_unsupported_rich_skills_and_keeps_live_fallback() {
        let simple = embedded_skill(
            "---
name: simple
description: Simple skill
---
Use simple guidance.
",
        );
        let rich = embedded_skill_with_reference(
            "---
name: rich
description: Rich skill
---
Use rich guidance.
",
        );
        let seed_registry = Arc::new(InMemorySkillRegistry::from_skills(vec![simple, rich]))
            as Arc<dyn SkillRegistry>;
        let managed_registry = Arc::new(ConfigSkillRegistry::new());
        let fallback = Arc::new(UnsupportedSeedSkillRegistry {
            inner: seed_registry.clone(),
        }) as Arc<dyn SkillRegistry>;
        let live_registry = Arc::new(StarterLiveSkillRegistry {
            managed: managed_registry.clone(),
            fallback,
        }) as Arc<dyn SkillRegistry>;
        let registries = StarterSkillRegistries {
            seed_registry,
            live_registry: live_registry.clone(),
            managed_registry: managed_registry.clone(),
        };

        let specs = registries.seed_specs().await.expect("seed specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id(), "simple");

        let managed_specs: Vec<SkillSpec> = specs
            .into_iter()
            .map(|spec| match spec {
                BuiltinSpec::Skill(skill) => skill,
                _ => panic!("expected skill spec"),
            })
            .collect();
        managed_registry
            .replace_specs(managed_specs)
            .expect("publish simple seed");

        assert!(live_registry.get("simple").is_some());
        assert!(
            live_registry.get("rich").is_some(),
            "resource-bearing local skills should remain available through fallback"
        );
    }
}
