use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tirea_contract::runtime::state::StateSpec;
use tirea_state::{GSet, State};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillWarning {
    pub path: PathBuf,
    pub reason: String,
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

/// Persisted skill state — only the CRDT active set.
///
/// Material content (instructions, references, scripts, assets) is delivered
/// inline via `ToolResult` / `with_user_message` and never stored in state,
/// avoiding parallel-branch conflicts on HashMap writes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "skills")]
pub struct SkillState {
    /// Activated skill IDs (grow-only set for conflict-free parallel merges).
    #[serde(default)]
    #[tirea(lattice)]
    pub active: GSet<String>,
}

/// Action type for [`SkillState`] reducer.
pub enum SkillStateAction {
    /// Mark a skill as activated (insert into the grow-only set).
    Activate(String),
}

impl StateSpec for SkillState {
    type Action = SkillStateAction;

    fn reduce(&mut self, action: Self::Action) {
        match action {
            SkillStateAction::Activate(id) => {
                self.active.insert(id);
            }
        }
    }
}

/// Build a stable map key for skill materials.
pub fn material_key(skill_id: &str, relative_path: &str) -> String {
    format!("{skill_id}:{relative_path}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "resource", rename_all = "snake_case")]
pub enum SkillResource {
    Reference(LoadedReference),
    Asset(LoadedAsset),
}

#[derive(Debug, thiserror::Error)]
pub enum SkillMaterializeError {
    #[error("invalid relative path: {0}")]
    InvalidPath(String),

    #[error("path is outside skill root")]
    PathEscapesRoot,

    #[error("unsupported path (expected under {0})")]
    UnsupportedPath(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("script runtime not supported for: {0}")]
    UnsupportedRuntime(String),

    #[error("script timed out after {0}s")]
    Timeout(u64),

    #[error("invalid script arguments: {0}")]
    InvalidScriptArgs(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("unknown skill: {0}")]
    UnknownSkill(String),

    #[error("invalid SKILL.md: {0}")]
    InvalidSkillMd(String),

    #[error("materialize error: {0}")]
    Materialize(#[from] SkillMaterializeError),

    #[error("io error: {0}")]
    Io(String),

    #[error("duplicate skill id: {0}")]
    DuplicateSkillId(String),

    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

/// A single skill with its own IO capabilities.
///
/// Each implementation encapsulates how to read instructions, load resources,
/// and run scripts. This replaces the old `SkillRegistry` trait where a single
/// registry handled all skills and required `skill_id` parameters.
#[async_trait]
pub trait Skill: Send + Sync + std::fmt::Debug {
    /// Metadata for this skill (id, name, description, allowed_tools).
    fn meta(&self) -> &SkillMeta;

    /// Read the raw SKILL.md content.
    async fn read_instructions(&self) -> Result<String, SkillError>;

    /// Load a resource (reference or asset) by relative path.
    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError>;

    /// Run a script by relative path with arguments.
    async fn run_script(&self, script: &str, args: &[String]) -> Result<ScriptResult, SkillError>;
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
    fn skill_error_preserves_materialize_variant() {
        let err: SkillError = SkillMaterializeError::PathEscapesRoot.into();
        assert!(matches!(
            err,
            SkillError::Materialize(SkillMaterializeError::PathEscapesRoot)
        ));
    }

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
            SkillMeta {
                id: id.to_string(),
                name: id.to_string(),
                description: format!("{id} skill"),
                allowed_tools: Vec::new(),
            }
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
            SkillMeta {
                id: id.to_string(),
                name: id.to_string(),
                description: format!("{id} skill"),
                allowed_tools: Vec::new(),
            }
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
}
