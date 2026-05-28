use async_trait::async_trait;
use awaken_runtime_contract::{
    BuiltinSpec, PreparedSkillSpecs, SkillArgumentSpec, SkillSpec, SkillSpecContext, SkillSpecSink,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::SkillError;
use crate::registry::SkillRegistry;
use crate::skill::{
    ScriptResult, Skill, SkillActivation, SkillContext, SkillMeta, SkillResource,
    SkillResourceKind, render_activation_instructions,
};
use crate::skill_md::{SkillFrontmatter, parse_skill_md};

/// Runtime skill backed by a structured [`SkillSpec`] from ConfigStore.
#[derive(Debug, Clone)]
pub struct ConfigSkill {
    meta: SkillMeta,
    instructions_md: String,
    activation_fingerprint: String,
}

impl ConfigSkill {
    pub fn try_from_spec(spec: SkillSpec) -> Result<Self, SkillError> {
        validate_spec(&spec)?;
        let activation_fingerprint = spec_fingerprint(&spec)?;
        let meta = meta_from_spec(&spec);
        Ok(Self {
            meta,
            instructions_md: spec.instructions_md,
            activation_fingerprint,
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

    fn activation_fingerprint(&self) -> Option<&str> {
        Some(&self.activation_fingerprint)
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        self.synthesize_skill_md()
    }

    async fn activate(&self, args: Option<&Value>) -> Result<SkillActivation, SkillError> {
        let instructions =
            render_activation_instructions(&self.meta, self.instructions_md.clone(), args)?;
        Ok(SkillActivation { instructions })
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
        self.prepare_specs(specs)?.commit();
        Ok(())
    }

    fn prepare_specs(
        &self,
        specs: impl IntoIterator<Item = SkillSpec>,
    ) -> Result<Box<dyn PreparedSkillSpecs>, SkillError> {
        let next = build_skill_map(specs)?;
        Ok(Box::new(PreparedConfigSkillSpecs {
            target: Arc::clone(&self.skills),
            next,
        }))
    }
}

impl SkillSpecSink for ConfigSkillRegistry {
    fn prepare_skill_specs(
        &self,
        specs: Vec<SkillSpec>,
    ) -> Result<Box<dyn PreparedSkillSpecs>, String> {
        self.prepare_specs(specs).map_err(|error| error.to_string())
    }
}

struct PreparedConfigSkillSpecs {
    target: Arc<RwLock<HashMap<String, Arc<dyn Skill>>>>,
    next: HashMap<String, Arc<dyn Skill>>,
}

impl PreparedSkillSpecs for PreparedConfigSkillSpecs {
    fn commit(self: Box<Self>) {
        let Self { target, next } = *self;
        *write_lock(&target) = next;
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
        if !meta.paths.is_empty() {
            return Err(SkillError::Unsupported(format!(
                "cannot snapshot skill '{}' as DB-managed skill: paths are not supported",
                meta.id
            )));
        }
        let resources = skill.materialized_resource_paths();
        let scripts = skill.materialized_script_paths();
        if !resources.is_empty() || !scripts.is_empty() {
            return Err(SkillError::Unsupported(format!(
                "cannot snapshot skill '{}' as DB-managed skill: resources/scripts are not persisted",
                meta.id
            )));
        }
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
    awaken_runtime_contract::validate_skill_spec(value)
        .map(|_| ())
        .map_err(|e| SkillError::InvalidArguments(e.to_string()))
}

fn spec_fingerprint(spec: &SkillSpec) -> Result<String, SkillError> {
    let bytes =
        serde_json::to_vec(spec).map_err(|e| SkillError::InvalidArguments(e.to_string()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("sha256:{digest:x}"))
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

    #[test]
    fn config_skill_activation_fingerprint_changes_when_spec_changes() {
        let original = ConfigSkill::try_from_spec(spec("db-management")).unwrap();
        let updated = ConfigSkill::try_from_spec(SkillSpec {
            instructions_md: "Goodbye ${name}".into(),
            ..spec("db-management")
        })
        .unwrap();

        assert_ne!(
            original.activation_fingerprint(),
            updated.activation_fingerprint()
        );
    }

    #[tokio::test]
    async fn config_skill_activate_requires_declared_required_arguments() {
        let skill = ConfigSkill::try_from_spec(SkillSpec {
            instructions_md: "Use ${dialect}".into(),
            arguments: vec![SkillArgumentSpec {
                name: "dialect".into(),
                description: None,
                required: true,
            }],
            ..spec("db-management")
        })
        .unwrap();

        let err = skill.activate(None).await.unwrap_err();
        assert!(err.to_string().contains("requires named arguments"));

        let err = skill
            .activate(Some(&serde_json::json!({"dialect": null})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not be null"));

        let activation = skill
            .activate(Some(&serde_json::json!({"dialect": "postgres"})))
            .await
            .unwrap();
        assert_eq!(activation.instructions, "Use postgres");
    }

    #[tokio::test]
    async fn config_skill_activate_rejects_non_object_unknown_and_non_scalar_args() {
        let skill = ConfigSkill::try_from_spec(SkillSpec {
            instructions_md: "Use ${dialect}".into(),
            arguments: vec![SkillArgumentSpec {
                name: "dialect".into(),
                description: None,
                required: false,
            }],
            ..spec("db-management")
        })
        .unwrap();

        let err = skill
            .activate(Some(&serde_json::json!("postgres")))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("requires named arguments as an object")
        );

        let err = skill
            .activate(Some(&serde_json::json!({"unknown": "x"})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown argument"));

        let err = skill
            .activate(Some(&serde_json::json!({"dialect": {"name": "postgres"}})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be a scalar"));
    }

    #[tokio::test]
    async fn config_skill_read_instructions_uses_id_for_frontmatter_name() {
        let skill = ConfigSkill::try_from_spec(spec("db-management")).unwrap();
        let raw = skill.read_instructions().await.unwrap();
        let doc = parse_skill_md(&raw).unwrap();
        assert_eq!(doc.frontmatter.name, "db-management");
        assert_eq!(skill.meta().name, "Database Management");
    }

    #[tokio::test]
    async fn config_skill_read_instructions_preserves_rich_frontmatter() {
        let skill = ConfigSkill::try_from_spec(SkillSpec {
            when_to_use: Some("When schema context is needed".into()),
            arguments: vec![SkillArgumentSpec {
                name: "dialect".into(),
                description: Some("SQL dialect".into()),
                required: true,
            }],
            argument_hint: Some("dialect=postgres".into()),
            user_invocable: false,
            model_invocable: false,
            model_override: Some("analysis-model".into()),
            context: SkillSpecContext::Fork,
            ..spec("db-management")
        })
        .unwrap();

        let raw = skill.read_instructions().await.unwrap();
        let doc = parse_skill_md(&raw).unwrap();

        assert_eq!(doc.frontmatter.name, "db-management");
        assert_eq!(doc.frontmatter.allowed_tools.as_deref(), Some("db_query"));
        assert_eq!(
            doc.frontmatter.when_to_use.as_deref(),
            Some("When schema context is needed")
        );
        assert_eq!(doc.frontmatter.arguments.unwrap()[0].name, "dialect");
        assert_eq!(
            doc.frontmatter.argument_hint.as_deref(),
            Some("dialect=postgres")
        );
        assert_eq!(doc.frontmatter.user_invocable, Some(false));
        assert_eq!(doc.frontmatter.disable_model_invocation, Some(true));
        assert_eq!(doc.frontmatter.model.as_deref(), Some("analysis-model"));
        assert_eq!(doc.frontmatter.context.as_deref(), Some("fork"));
        assert!(doc.frontmatter.paths.is_none());
        assert_eq!(doc.body, "Hello ${name}");
    }

    #[test]
    fn config_skill_rejects_paths_until_resources_are_persisted() {
        let err = ConfigSkill::try_from_spec(SkillSpec {
            paths: vec!["migrations/**".into()],
            ..spec("db-management")
        })
        .unwrap_err();

        assert!(err.to_string().contains("paths are not supported"));
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

    #[test]
    fn registry_rejects_invalid_replacement_without_losing_existing_snapshot() {
        let registry = ConfigSkillRegistry::new();
        registry.replace_specs([spec("db-a")]).unwrap();

        let err = registry
            .replace_specs([SkillSpec {
                id: "INVALID".into(),
                ..spec("db-b")
            }])
            .unwrap_err();

        assert!(matches!(err, SkillError::InvalidArguments(_)));
        assert_eq!(registry.ids(), vec!["db-a".to_string()]);
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

    #[tokio::test]
    async fn snapshot_skill_specs_rejects_resource_bearing_embedded_skill() {
        const SKILL_MD: &str =
            "---\nname: rich-skill\ndescription: Has resources\n---\nUse resources.\n";
        let skill = EmbeddedSkill::new(&EmbeddedSkillData {
            skill_md: SKILL_MD,
            references: &[("references/guide.md", "guide")],
            assets: &[("assets/logo.txt", "bG9nbw==", Some("text/plain"))],
        })
        .unwrap();
        let registry = crate::InMemorySkillRegistry::from_skills(vec![Arc::new(skill)]);

        let err = snapshot_skill_specs(&registry).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("resources/scripts are not persisted")
        );
    }

    #[tokio::test]
    async fn snapshot_skill_specs_rejects_paths_and_scripts() {
        #[derive(Debug)]
        struct MaterializedSkill {
            meta: SkillMeta,
            scripts: Vec<String>,
        }

        #[async_trait::async_trait]
        impl Skill for MaterializedSkill {
            fn meta(&self) -> &SkillMeta {
                &self.meta
            }

            async fn read_instructions(&self) -> Result<String, SkillError> {
                Ok(format!(
                    "---\nname: {}\ndescription: {}\n---\nBody\n",
                    self.meta.id, self.meta.description
                ))
            }

            async fn load_resource(
                &self,
                _kind: SkillResourceKind,
                path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported(path.to_string()))
            }

            async fn run_script(
                &self,
                script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported(script.to_string()))
            }

            fn materialized_script_paths(&self) -> Vec<String> {
                self.scripts.clone()
            }
        }

        let mut path_meta = SkillMeta::new("path-skill", "Path Skill", "Has paths", vec![]);
        path_meta.paths = vec!["src/**".into()];
        let registry =
            crate::InMemorySkillRegistry::from_skills(vec![Arc::new(MaterializedSkill {
                meta: path_meta,
                scripts: Vec::new(),
            })]);
        let err = snapshot_skill_specs(&registry).await.unwrap_err();
        assert!(err.to_string().contains("paths are not supported"));

        let registry =
            crate::InMemorySkillRegistry::from_skills(vec![Arc::new(MaterializedSkill {
                meta: SkillMeta::new("script-skill", "Script Skill", "Has scripts", vec![]),
                scripts: vec!["scripts/run.sh".into()],
            })]);
        let err = snapshot_skill_specs(&registry).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("resources/scripts are not persisted")
        );
    }

    #[tokio::test]
    async fn snapshot_skill_specs_sorts_and_preserves_metadata() {
        let registry = ConfigSkillRegistry::from_specs([
            SkillSpec {
                id: "zeta".into(),
                name: "Zeta".into(),
                description: "Zeta skill".into(),
                instructions_md: "Zeta body.".into(),
                ..Default::default()
            },
            SkillSpec {
                id: "alpha".into(),
                name: "Alpha".into(),
                description: "Alpha skill".into(),
                instructions_md: "Alpha body.".into(),
                allowed_tools: vec!["db_query".into()],
                when_to_use: Some("When alpha is needed".into()),
                arguments: vec![SkillArgumentSpec {
                    name: "dialect".into(),
                    description: Some("SQL dialect".into()),
                    required: true,
                }],
                argument_hint: Some("dialect=postgres".into()),
                user_invocable: false,
                model_invocable: false,
                model_override: Some("analysis-model".into()),
                context: SkillSpecContext::Fork,
                paths: Vec::new(),
            },
        ])
        .unwrap();

        let specs = snapshot_skill_specs(&registry).await.unwrap();

        assert_eq!(
            specs.iter().map(BuiltinSpec::id).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        let BuiltinSpec::Skill(alpha) = &specs[0] else {
            panic!("expected skill spec");
        };
        assert_eq!(alpha.instructions_md, "Alpha body.");
        assert_eq!(alpha.allowed_tools, vec!["db_query"]);
        assert_eq!(alpha.when_to_use.as_deref(), Some("When alpha is needed"));
        assert_eq!(alpha.arguments[0].name, "dialect");
        assert_eq!(alpha.argument_hint.as_deref(), Some("dialect=postgres"));
        assert!(!alpha.user_invocable);
        assert!(!alpha.model_invocable);
        assert_eq!(alpha.model_override.as_deref(), Some("analysis-model"));
        assert_eq!(alpha.context, SkillSpecContext::Fork);
        assert!(alpha.paths.is_empty());
    }
}
