use std::fs;
use std::path::Path;

use async_trait::async_trait;

use crate::error::{SkillError, SkillMaterializeError};
use crate::materialize::{load_asset_material, load_reference_material, run_script_material};
use crate::skill::{ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind};

use super::FsSkill;

#[async_trait]
impl Skill for FsSkill {
    fn meta(&self) -> &SkillMeta {
        &self.meta
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        fs::read_to_string(&self.skill_md_path).map_err(|e| {
            SkillError::Io(format!(
                "failed to read SKILL.md for skill '{}': {e}",
                self.meta.id
            ))
        })
    }

    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError> {
        let root = self.root_dir.clone();
        let skill_id = self.meta.id.clone();
        let path = path.to_string();

        let materialized: Result<SkillResource, SkillMaterializeError> =
            tokio::task::spawn_blocking(move || match kind {
                SkillResourceKind::Reference => {
                    load_reference_material(&skill_id, &root, &path).map(SkillResource::Reference)
                }
                SkillResourceKind::Asset => {
                    load_asset_material(&skill_id, &root, &path).map(SkillResource::Asset)
                }
            })
            .await
            .map_err(|e| SkillError::Io(e.to_string()))?;

        materialized.map_err(SkillError::from)
    }

    async fn run_script(&self, script: &str, args: &[String]) -> Result<ScriptResult, SkillError> {
        let result: Result<ScriptResult, SkillMaterializeError> =
            run_script_material(&self.meta.id, &self.root_dir, script, args).await;
        result.map_err(SkillError::from)
    }

    fn materialized_resource_paths(&self) -> Vec<(SkillResourceKind, String)> {
        let mut paths = list_materialized_paths(&self.root_dir, "references")
            .into_iter()
            .map(|path| (SkillResourceKind::Reference, path))
            .chain(
                list_materialized_paths(&self.root_dir, "assets")
                    .into_iter()
                    .map(|path| (SkillResourceKind::Asset, path)),
            )
            .collect::<Vec<_>>();
        paths.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));
        paths
    }

    fn materialized_script_paths(&self) -> Vec<String> {
        list_materialized_paths(&self.root_dir, "scripts")
    }
}

fn list_materialized_paths(root: &Path, first_component: &str) -> Vec<String> {
    let base = root.join(first_component);
    if !base.is_dir() {
        return Vec::new();
    }

    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.is_file()
                && let Ok(rel) = path.strip_prefix(base)
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                out.push(format!(
                    "{}/{}",
                    base.file_name().unwrap().to_string_lossy(),
                    rel
                ));
            }
        }
    }

    let mut out = Vec::new();
    walk(&base, &base, &mut out);
    out.sort();
    out
}
