use async_trait::async_trait;
use awaken_contract::{BuiltinSpec, SkillArgumentSpec, SkillSpec, SkillSpecContext, SkillSpecSink};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::SkillError;
use crate::registry::SkillRegistry;
use crate::skill::{
    ScriptResult, Skill, SkillActivation, SkillContext, SkillMeta, SkillResource, SkillResourceKind,
};
use crate::skill_md::{SkillFrontmatter, parse_skill_md};

/// Runtime skill backed by a structured [`SkillSpec`] from ConfigStore.
#[derive(Debug, Clone)]
pub struct ConfigSkill {
    meta: SkillMeta,
    instructions_md: String,
}

impl ConfigSkill {
    pub fn try_from_spec(spec: SkillSpec) -> Result<Self, SkillError> {
        validate_spec(&spec)?;
        let meta = meta_from_spec(&spec);
        Ok(Self {
            meta,
            instructions_md: spec.instructions_md,
        })
    }

    fn synthesize_skill_md(&self) -> Result<String, SkillError> {
        let fm = SkillFrontmatter {
            // `SKILL.md` frontmatter `name` is the canonical skill id. The
            // display name remains in `SkillMeta::name` for catalogs.
            name: self.meta.id.clone(),
            description: self.meta.description.clone(),
            license: None,
            compatibility: None,
            metadata: None,
            allowed_tools: if self.meta.allowed_tools.is_empty() {
                None
            } else {
                Some(self.meta.allowed_tools.join(" "))
            },
            when_to_use: self.meta.when_to_use.clone(),
            arguments: if self.meta.arguments.is_empty() {
                None
            } else {
                Some(self.meta.arguments.clone())
            },
            argument_hint: self.meta.argument_hint.clone(),
            user_invocable: Some(self.meta.user_invocable),
            disable_model_invocation: Some(!self.meta.model_invocable),
            model: self.meta.model_override.clone(),
            context: Some(match self.meta.context {
                SkillContext::Inline => "inline".to_string(),
                SkillContext::Fork => "fork".to_string(),
            }),
            paths: if self.meta.paths.is_empty() {
                None
            } else {
                Some(self.meta.paths.join("\n"))
            },
        };
        let yaml =
            serde_yaml::to_string(&fm).map_err(|e| SkillError::InvalidSkillMd(e.to_string()))?;
        Ok(format!("---\n{yaml}---\n{}", self.instructions_md))
    }
}

#[async_trait]
impl Skill for ConfigSkill {
    fn meta(&self) -> &SkillMeta {
        &self.meta
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        self.synthesize_skill_md()
    }

    async fn activate(&self, args: Option<&Value>) -> Result<SkillActivation, SkillError> {
        let mut body = self.instructions_md.clone();
        if let Some(obj) = args.and_then(|a| a.as_object()) {
            for (key, val) in obj {
                let pattern = format!("${{{key}}}");
                let replacement = match val {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                body = body.replace(&pattern, &replacement);
            }
        }
        Ok(SkillActivation { instructions: body })
    }

    async fn load_resource(
        &self,
        _kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError> {
        Err(SkillError::Unsupported(format!(
            "config-managed skill '{}' has no materialized resource: {path}",
            self.meta.id
        )))
    }

    async fn run_script(&self, script: &str, _args: &[String]) -> Result<ScriptResult, SkillError> {
        Err(SkillError::Unsupported(format!(
            "config-managed skill '{}' does not support script execution: {script}",
            self.meta.id
        )))
    }
}

/// In-memory live registry populated from DB-managed [`SkillSpec`] records.
#[derive(Clone, Default)]
pub struct ConfigSkillRegistry {
    skills: Arc<RwLock<HashMap<String, Arc<dyn Skill>>>>,
}

impl std::fmt::Debug for ConfigSkillRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigSkillRegistry")
            .field("len", &self.len())
            .finish()
    }
}

impl ConfigSkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_specs(specs: impl IntoIterator<Item = SkillSpec>) -> Result<Self, SkillError> {
        let registry = Self::new();
        registry.replace_specs(specs)?;
        Ok(registry)
    }

    pub fn replace_specs(
        &self,
        specs: impl IntoIterator<Item = SkillSpec>,
    ) -> Result<(), SkillError> {
        let next = build_skill_map(specs)?;
        *write_lock(&self.skills) = next;
        Ok(())
    }
}

impl SkillSpecSink for ConfigSkillRegistry {
    fn validate_skill_specs(&self, specs: &[SkillSpec]) -> Result<(), String> {
        build_skill_map(specs.iter().cloned())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn replace_skill_specs(&self, specs: Vec<SkillSpec>) -> Result<(), String> {
        self.replace_specs(specs).map_err(|error| error.to_string())
    }
}

impl SkillRegistry for ConfigSkillRegistry {
    fn len(&self) -> usize {
        read_lock(&self.skills).len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        read_lock(&self.skills).get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = read_lock(&self.skills).keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        read_lock(&self.skills).clone()
    }
}

/// Convert a runtime skill registry into built-in `skills` seed records.
pub async fn snapshot_skill_specs(
    registry: &dyn SkillRegistry,
) -> Result<Vec<BuiltinSpec>, SkillError> {
    let mut out = Vec::new();
    for skill in registry.snapshot().into_values() {
        let meta = skill.meta().clone();
        let raw = skill.read_instructions().await?;
        let doc = parse_skill_md(&raw).map_err(|e| SkillError::InvalidSkillMd(e.to_string()))?;
        let spec = SkillSpec {
            id: meta.id,
            name: meta.name,
            description: meta.description,
            instructions_md: doc.body,
            allowed_tools: meta.allowed_tools,
            when_to_use: meta.when_to_use,
            arguments: meta
                .arguments
                .into_iter()
                .map(|argument| SkillArgumentSpec {
                    name: argument.name,
                    description: argument.description,
                    required: argument.required,
                })
                .collect(),
            argument_hint: meta.argument_hint,
            user_invocable: meta.user_invocable,
            model_invocable: meta.model_invocable,
            model_override: meta.model_override,
            context: match meta.context {
                SkillContext::Inline => SkillSpecContext::Inline,
                SkillContext::Fork => SkillSpecContext::Fork,
            },
            paths: meta.paths,
        };
        out.push(BuiltinSpec::skill(spec));
    }
    out.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(out)
}

fn build_skill_map(
    specs: impl IntoIterator<Item = SkillSpec>,
) -> Result<HashMap<String, Arc<dyn Skill>>, SkillError> {
    let mut next: HashMap<String, Arc<dyn Skill>> = HashMap::new();
    for spec in specs {
        let skill = Arc::new(ConfigSkill::try_from_spec(spec)?) as Arc<dyn Skill>;
        let id = skill.meta().id.trim().to_string();
        if id.is_empty() {
            return Err(SkillError::InvalidArguments(
                "skill id must be non-empty".into(),
            ));
        }
        if next.insert(id.clone(), skill).is_some() {
            return Err(SkillError::DuplicateSkillId(id));
        }
    }
    Ok(next)
}

fn validate_spec(spec: &SkillSpec) -> Result<(), SkillError> {
    let value =
        serde_json::to_value(spec).map_err(|e| SkillError::InvalidArguments(e.to_string()))?;
    awaken_contract::validate_skill_spec(value)
        .map(|_| ())
        .map_err(|e| SkillError::InvalidArguments(e.to_string()))
}

fn meta_from_spec(spec: &SkillSpec) -> SkillMeta {
    let mut meta = SkillMeta::new(
        spec.id.clone(),
        spec.name.clone(),
        spec.description.clone(),
        spec.allowed_tools.clone(),
    );
    meta.when_to_use = spec.when_to_use.clone();
    meta.arguments = spec
        .arguments
        .iter()
        .map(|argument| crate::skill_md::SkillArgumentDef {
            name: argument.name.clone(),
            description: argument.description.clone(),
            required: argument.required,
        })
        .collect();
    meta.argument_hint = spec.argument_hint.clone();
    meta.user_invocable = spec.user_invocable;
    meta.model_invocable = spec.model_invocable;
    meta.model_override = spec.model_override.clone();
    meta.context = match spec.context {
        SkillSpecContext::Inline => SkillContext::Inline,
        SkillSpecContext::Fork => SkillContext::Fork,
    };
    meta.paths = spec.paths.clone();
    meta
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedSkill, EmbeddedSkillData};

    fn spec(id: &str) -> SkillSpec {
        SkillSpec {
            id: id.into(),
            name: "Database Management".into(),
            description: "Helps with database operations".into(),
            instructions_md: "Hello ${name}".into(),
            allowed_tools: vec!["db_query".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn config_skill_activate_substitutes_arguments() {
        let skill = ConfigSkill::try_from_spec(spec("db-management")).unwrap();
        let activation = skill
            .activate(Some(&serde_json::json!({"name": "Ada"})))
            .await
            .unwrap();
        assert_eq!(activation.instructions, "Hello Ada");
    }

    #[tokio::test]
    async fn config_skill_read_instructions_uses_id_for_frontmatter_name() {
        let skill = ConfigSkill::try_from_spec(spec("db-management")).unwrap();
        let raw = skill.read_instructions().await.unwrap();
        let doc = parse_skill_md(&raw).unwrap();
        assert_eq!(doc.frontmatter.name, "db-management");
        assert_eq!(skill.meta().name, "Database Management");
    }

    #[test]
    fn registry_replace_specs_swaps_snapshot() {
        let registry = ConfigSkillRegistry::new();
        registry.replace_specs([spec("db-a")]).unwrap();
        assert_eq!(registry.ids(), vec!["db-a".to_string()]);
        registry.replace_specs([spec("db-b")]).unwrap();
        assert_eq!(registry.ids(), vec!["db-b".to_string()]);
        assert!(registry.get("db-a").is_none());
    }

    #[test]
    fn registry_rejects_duplicate_spec_ids() {
        let registry = ConfigSkillRegistry::new();
        let err = registry
            .replace_specs([spec("db-a"), spec("db-a")])
            .unwrap_err();
        assert!(matches!(err, SkillError::DuplicateSkillId(ref id) if id == "db-a"));
    }

    #[tokio::test]
    async fn snapshot_skill_specs_round_trips_embedded_skill() {
        const SKILL_MD: &str = "---\nname: db-management\ndescription: Helps with database operations\nallowed-tools: db_query\n---\nInspect schema first.\n";
        let skill = EmbeddedSkill::new(&EmbeddedSkillData {
            skill_md: SKILL_MD,
            references: &[],
            assets: &[],
        })
        .unwrap();
        let registry = crate::InMemorySkillRegistry::from_skills(vec![Arc::new(skill)]);
        let specs = snapshot_skill_specs(&registry).await.unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].namespace(), "skills");
        assert_eq!(specs[0].id(), "db-management");
    }
}
