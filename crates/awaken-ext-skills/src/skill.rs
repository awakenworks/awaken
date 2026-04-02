use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::SkillError;
use crate::skill_md::{SkillArgumentDef, parse_skill_md};

/// Execution context for a skill activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillContext {
    #[default]
    Inline,
    Fork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,

    // --- Visibility & invocation control (ADR-0020) ---
    /// Hint for the LLM about when to use this skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Formal argument definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<SkillArgumentDef>,
    /// Free-text argument hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Whether the user can invoke this skill via `/name` (default: true).
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Whether the LLM can invoke this skill (default: true).
    /// Inverse of the frontmatter `disable-model-invocation`.
    #[serde(default = "default_true")]
    pub model_invocable: bool,
    /// Model override when this skill is activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Execution mode.
    #[serde(default)]
    pub context: SkillContext,
    /// Glob patterns for conditional activation (empty = unconditional).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl SkillMeta {
    /// Create a `SkillMeta` with only the required fields; all visibility/invocation
    /// fields are set to their defaults.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        allowed_tools: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            allowed_tools,
            when_to_use: None,
            arguments: Vec::new(),
            argument_hint: None,
            user_invocable: true,
            model_invocable: true,
            model_override: None,
            context: SkillContext::default(),
            paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillResourceKind {
    Reference,
    Asset,
}

impl SkillResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Asset => "asset",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedReference {
    pub skill: String,
    pub path: String,
    pub sha256: String,
    pub truncated: bool,
    pub content: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptResult {
    pub skill: String,
    pub script: String,
    pub sha256: String,
    pub truncated_stdout: bool,
    pub truncated_stderr: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedAsset {
    pub skill: String,
    pub path: String,
    pub sha256: String,
    pub truncated: bool,
    pub bytes: u64,
    pub media_type: Option<String>,
    pub encoding: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "resource", rename_all = "snake_case")]
pub enum SkillResource {
    Reference(LoadedReference),
    Asset(LoadedAsset),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillActivation {
    pub instructions: String,
}

/// A single skill with its own IO capabilities.
///
/// Each implementation encapsulates how to read instructions, load resources,
/// and run scripts.
#[async_trait]
pub trait Skill: Send + Sync + std::fmt::Debug {
    /// Metadata for this skill (id, name, description, allowed_tools).
    fn meta(&self) -> &SkillMeta;

    /// Read the raw SKILL.md content.
    async fn read_instructions(&self) -> Result<String, SkillError>;

    /// Resolve activation instructions for this skill.
    ///
    /// By default this parses the `SKILL.md` body and substitutes `${ARG_NAME}`
    /// patterns with values from the activation arguments.
    async fn activate(&self, args: Option<&Value>) -> Result<SkillActivation, SkillError> {
        let raw = self.read_instructions().await?;
        let doc = parse_skill_md(&raw).map_err(|e| SkillError::InvalidSkillMd(e.to_string()))?;
        let mut body = doc.body;

        // Substitute ${ARG_NAME} patterns from the activation arguments.
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

    /// Load a resource (reference or asset) by relative path.
    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError>;

    /// Run a script by relative path with arguments.
    async fn run_script(&self, script: &str, args: &[String]) -> Result<ScriptResult, SkillError>;
}

/// Build a stable map key for skill materials.
pub fn material_key(skill_id: &str, relative_path: &str) -> String {
    format!("{skill_id}:{relative_path}")
}

/// Collect skills into a map, failing on duplicate IDs.
pub fn collect_skills(
    skills: Vec<Arc<dyn Skill>>,
) -> Result<HashMap<String, Arc<dyn Skill>>, SkillError> {
    let mut map: HashMap<String, Arc<dyn Skill>> = HashMap::new();
    for skill in skills {
        let id = skill.meta().id.clone();
        if map.contains_key(&id) {
            return Err(SkillError::DuplicateSkillId(id));
        }
        map.insert(id, skill);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_skills_rejects_duplicates() {
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
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
            async fn run_script(
                &self,
                _script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
        }

        fn meta(id: &str) -> SkillMeta {
            SkillMeta::new(id, id, format!("{id} skill"), Vec::new())
        }

        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(meta("a"))),
            Arc::new(MockSkill(meta("a"))),
        ];
        let err = collect_skills(skills).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateSkillId(ref id) if id == "a"));
    }

    #[test]
    fn collect_skills_succeeds_for_unique_ids() {
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
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
            async fn run_script(
                &self,
                _script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
        }

        fn meta(id: &str) -> SkillMeta {
            SkillMeta::new(id, id, format!("{id} skill"), Vec::new())
        }

        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(meta("alpha"))),
            Arc::new(MockSkill(meta("beta"))),
        ];
        let map = collect_skills(skills).unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("alpha"));
        assert!(map.contains_key("beta"));
    }

    #[test]
    fn material_key_format() {
        assert_eq!(
            material_key("my-skill", "references/guide.md"),
            "my-skill:references/guide.md"
        );
        assert_eq!(material_key("", "path"), ":path");
    }

    #[test]
    fn skill_meta_serde_roundtrip() {
        let meta = SkillMeta::new("test", "test", "A test", vec!["Bash".into(), "Read".into()]);
        let json = serde_json::to_value(&meta).unwrap();
        let parsed: SkillMeta = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, meta);
    }

    #[test]
    fn skill_activation_serde_roundtrip() {
        let activation = SkillActivation {
            instructions: "Do something.".into(),
        };
        let json = serde_json::to_value(&activation).unwrap();
        let parsed: SkillActivation = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, activation);
    }

    #[test]
    fn skill_resource_kind_as_str() {
        assert_eq!(SkillResourceKind::Reference.as_str(), "reference");
        assert_eq!(SkillResourceKind::Asset.as_str(), "asset");
    }

    #[test]
    fn skill_resource_kind_serde_roundtrip() {
        let kind = SkillResourceKind::Asset;
        let json = serde_json::to_value(kind).unwrap();
        assert_eq!(json.as_str(), Some("asset"));
        let parsed: SkillResourceKind = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, kind);
    }

    #[test]
    fn skill_resource_reference_variant_serde() {
        let r = SkillResource::Reference(LoadedReference {
            skill: "s1".into(),
            path: "references/a.md".into(),
            sha256: "abc".into(),
            truncated: false,
            content: "hello".into(),
            bytes: 5,
        });
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "reference");
        let parsed: SkillResource = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn skill_resource_asset_variant_serde() {
        let a = SkillResource::Asset(LoadedAsset {
            skill: "s1".into(),
            path: "assets/img.png".into(),
            sha256: "def".into(),
            truncated: false,
            bytes: 100,
            media_type: Some("image/png".into()),
            encoding: "base64".into(),
            content: "aGVsbG8=".into(),
        });
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["kind"], "asset");
        let parsed: SkillResource = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn loaded_reference_fields() {
        let r = LoadedReference {
            skill: "s".into(),
            path: "p".into(),
            sha256: "h".into(),
            truncated: true,
            content: "c".into(),
            bytes: 1,
        };
        assert!(r.truncated);
        assert_eq!(r.bytes, 1);
    }

    #[test]
    fn script_result_fields() {
        let s = ScriptResult {
            skill: "s".into(),
            script: "scripts/run.sh".into(),
            sha256: "abc".into(),
            truncated_stdout: false,
            truncated_stderr: true,
            exit_code: 1,
            stdout: "out".into(),
            stderr: "err".into(),
        };
        assert_eq!(s.exit_code, 1);
        assert!(s.truncated_stderr);
    }

    #[tokio::test]
    async fn default_skill_activation_uses_skill_md_body() {
        #[derive(Debug)]
        struct MockSkill {
            meta: SkillMeta,
            skill_md: String,
        }

        #[async_trait]
        impl Skill for MockSkill {
            fn meta(&self) -> &SkillMeta {
                &self.meta
            }

            async fn read_instructions(&self) -> Result<String, SkillError> {
                Ok(self.skill_md.clone())
            }

            async fn load_resource(
                &self,
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }

            async fn run_script(
                &self,
                _script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
        }

        let skill = MockSkill {
            meta: SkillMeta::new("demo", "demo", "demo", Vec::new()),
            skill_md: "---\nname: demo\ndescription: Demo\n---\nUse the demo flow.\n".to_string(),
        };

        let activation = skill.activate(None).await.unwrap();
        assert_eq!(activation.instructions, "Use the demo flow.\n");
    }

    #[tokio::test]
    async fn activate_substitutes_arg_patterns() {
        #[derive(Debug)]
        struct MockSkill {
            meta: SkillMeta,
            skill_md: String,
        }

        #[async_trait]
        impl Skill for MockSkill {
            fn meta(&self) -> &SkillMeta {
                &self.meta
            }
            async fn read_instructions(&self) -> Result<String, SkillError> {
                Ok(self.skill_md.clone())
            }
            async fn load_resource(
                &self,
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
            async fn run_script(
                &self,
                _script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
        }

        let skill = MockSkill {
            meta: SkillMeta::new("demo", "demo", "demo", Vec::new()),
            skill_md: "---\nname: demo\ndescription: Demo\n---\nProcess ${FILE} in ${MODE} mode.\n"
                .to_string(),
        };

        let args = serde_json::json!({"FILE": "main.rs", "MODE": "strict"});
        let activation = skill.activate(Some(&args)).await.unwrap();
        assert_eq!(activation.instructions, "Process main.rs in strict mode.\n");
    }

    #[tokio::test]
    async fn activate_preserves_unmatched_patterns() {
        #[derive(Debug)]
        struct MockSkill {
            meta: SkillMeta,
            skill_md: String,
        }

        #[async_trait]
        impl Skill for MockSkill {
            fn meta(&self) -> &SkillMeta {
                &self.meta
            }
            async fn read_instructions(&self) -> Result<String, SkillError> {
                Ok(self.skill_md.clone())
            }
            async fn load_resource(
                &self,
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
            async fn run_script(
                &self,
                _script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, SkillError> {
                Err(SkillError::Unsupported("mock".into()))
            }
        }

        let skill = MockSkill {
            meta: SkillMeta::new("demo", "demo", "demo", Vec::new()),
            skill_md: "---\nname: demo\ndescription: Demo\n---\nHello ${NAME}, ${UNKNOWN} stays.\n"
                .to_string(),
        };

        let args = serde_json::json!({"NAME": "world"});
        let activation = skill.activate(Some(&args)).await.unwrap();
        assert_eq!(activation.instructions, "Hello world, ${UNKNOWN} stays.\n");
    }

    #[test]
    fn skill_meta_new_defaults() {
        let meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        assert!(meta.user_invocable);
        assert!(meta.model_invocable);
        assert_eq!(meta.context, SkillContext::Inline);
        assert!(meta.paths.is_empty());
        assert!(meta.arguments.is_empty());
        assert!(meta.when_to_use.is_none());
        assert!(meta.model_override.is_none());
        assert!(meta.argument_hint.is_none());
    }

    #[test]
    fn skill_context_default_is_inline() {
        assert_eq!(SkillContext::default(), SkillContext::Inline);
    }

    #[test]
    fn skill_context_serde_roundtrip() {
        let ctx = SkillContext::Fork;
        let json = serde_json::to_value(ctx).unwrap();
        assert_eq!(json.as_str(), Some("fork"));
        let parsed: SkillContext = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, SkillContext::Fork);
    }
}
