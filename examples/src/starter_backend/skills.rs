use std::path::PathBuf;
use std::sync::Arc;

use awaken_contract::{BuiltinSpec, SkillSpecSink};
use awaken_ext_skills::{
    ActiveSkillInstructionsPlugin, CompositeSkillRegistry, ConfigSkillRegistry, EmbeddedSkill,
    EmbeddedSkillData, FsSkillRegistryManager, InMemorySkillRegistry, SkillDiscoveryPlugin,
    SkillError, SkillRegistry, snapshot_skill_specs,
};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::plugins::Plugin;

pub struct StarterSkillRegistries {
    seed_registry: Arc<dyn SkillRegistry>,
    managed_registry: Arc<ConfigSkillRegistry>,
}

impl StarterSkillRegistries {
    pub fn live_registry(&self) -> Arc<dyn SkillRegistry> {
        self.managed_registry.clone()
    }

    pub fn spec_sink(&self) -> Arc<dyn SkillSpecSink> {
        self.managed_registry.clone()
    }

    pub async fn seed_specs(&self) -> Result<Vec<BuiltinSpec>, SkillError> {
        snapshot_skill_specs(self.seed_registry.as_ref()).await
    }
}

pub fn install_plugins(
    mut builder: AgentRuntimeBuilder,
    has_skills_dir: bool,
) -> (AgentRuntimeBuilder, StarterSkillRegistries) {
    let seed_registry = seed_skill_registry(has_skills_dir);
    let managed_registry = Arc::new(ConfigSkillRegistry::new());
    let live_registry: Arc<dyn SkillRegistry> = managed_registry.clone();

    builder = builder.with_plugin(
        "skills-discovery",
        Arc::new(SkillDiscoveryPlugin::new(live_registry.clone())) as Arc<dyn Plugin>,
    );
    builder = builder.with_plugin(
        "skills-active-instructions",
        Arc::new(ActiveSkillInstructionsPlugin::new(live_registry)) as Arc<dyn Plugin>,
    );

    (
        builder,
        StarterSkillRegistries {
            seed_registry,
            managed_registry,
        },
    )
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
