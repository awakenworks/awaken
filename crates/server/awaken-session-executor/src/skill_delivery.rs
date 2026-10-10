// @formal entrypoint skill.invalid_workspace_or_hash_fails_closed
//! Delivered Skill bundles: their physical installer and the Session's catalogue prompt.

use awaken_ext_skills::SkillRegistry;
use awaken_resource_contract::{SkillStoreError, SkillVersion, skill_stem};
use awaken_session_types::RunError;

/// Rebuildable metadata only, derived from the one frozen registry. Keep the
/// directory unrendered until the attempt selects its filesystem view; an ACP
/// cwd is not the Skill materialization root. No bodies or version state live here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FilesystemSkillPrompt {
    entries: Vec<(String, String, String)>,
}

impl FilesystemSkillPrompt {
    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<(String, String, String)>) -> Self {
        Self { entries }
    }

    pub(crate) fn exposes_directory(&self, directory: &str) -> bool {
        self.entries.iter().any(|(_, _, entry)| entry == directory)
    }

    pub(crate) fn render(
        &self,
        path_view: &crate::managed_input_layout::AgentPathView,
        environment: Option<&crate::session_environment::SessionEnvironment>,
    ) -> Result<String, String> {
        let entries = self
            .entries
            .iter()
            .map(|(name, description, directory)| {
                let path = match environment {
                    Some(environment) => path_view.render(&format!(
                        "{}/SKILL.md",
                        environment
                            .skills()
                            .skill_directory_path(directory)
                            .map_err(|error| error.to_string())?,
                    )),
                    // Native OnToolUse reads this stable path from the exact
                    // frozen bundle until the Environment is materialized.
                    None => format!("{directory}/SKILL.md"),
                };
                Ok(format!("- {name}: {description} (`{path}`)"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(format!(
            "Available Skills are listed below. Apply a relevant Skill's complete instructions. If its complete instructions are already in the conversation, DO NOT read `SKILL.md` again: apply the provided instructions without rereading or checking the same body. Otherwise, read its `SKILL.md` from the given path before acting. Names and descriptions alone are not complete instructions. Supporting scripts and references are separate: read them from the listed directory when their content is needed and is not already available in the conversation.\n{}",
            entries.join("\n")
        ))
    }
}

/// Anthropic-compatible progressive-disclosure metadata; ordinary file tools
/// read the full instructions from the pinned or materialized `SKILL.md` path.
pub(crate) fn filesystem_skill_prompt(
    registry: &dyn SkillRegistry,
) -> Result<Option<FilesystemSkillPrompt>, String> {
    let entries = registry
        .list()?
        .into_iter()
        .filter(|skill| skill.model_invocable)
        .filter_map(|skill| skill.dir.map(|dir| (skill.name, skill.description, dir)))
        .collect::<Vec<_>>();
    Ok((!entries.is_empty()).then_some(FilesystemSkillPrompt { entries }))
}

pub(crate) const DELIVERED_SKILLS_SUBDIR: &str =
    awaken_provisioning_contract::WorkspaceLayout::DELIVERED_SKILLS_SUBDIR;

pub(crate) fn delivered_skill_directory(id: &str) -> String {
    format!("{DELIVERED_SKILLS_SUBDIR}/{}", skill_stem(id))
}

pub(crate) fn delivered_skill_content(version: &SkillVersion) -> Result<&str, String> {
    if awaken_resource_contract::skill_bundle_sha256(&version.files) != version.bundle_sha256 {
        return Err(format!(
            "Skill {} version {} bundle hash mismatch",
            version.skill_id, version.version
        ));
    }
    version
        .skill_md()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .ok_or_else(|| {
            format!(
                "Skill {} version {} has no UTF-8 SKILL.md",
                version.skill_id, version.version
            )
        })
}

/// One physical bundle installer. Managed Resource realization owns its call;
/// direct compatibility construction calls the same installer when necessary.
pub(crate) async fn materialize_delivered_skill(
    env: &crate::session_environment::SessionEnvironment,
    version: &SkillVersion,
) -> Result<(), String> {
    delivered_skill_content(version)?;
    let files = version
        .files
        .iter()
        .map(|file| (file.path.clone(), file.content.clone(), file.executable))
        .collect::<Vec<_>>();
    let directory = delivered_skill_directory(version.skill_id.as_str());
    env.skills()
        .materialize_read_only_tree(&directory, &files)
        .await
        .map_err(|error| error.to_string())
}

/// The shared closed classification of Skill repository failures.
pub(crate) fn skill_store_run_error(error: SkillStoreError) -> RunError {
    let message = error.to_string();
    match error {
        SkillStoreError::NotFound(_)
        | SkillStoreError::InvalidDefinition { .. }
        | SkillStoreError::InvalidVersion { .. }
        | SkillStoreError::BundleMismatch { .. }
        | SkillStoreError::InvalidSelection { .. } => RunError::bad_request(message),
        SkillStoreError::Io { .. }
        | SkillStoreError::Indeterminate { .. }
        | SkillStoreError::AuthorityUnavailable { .. } => RunError::unavailable(message),
        SkillStoreError::AlreadyExists(_)
        | SkillStoreError::VersionConflict(_)
        | SkillStoreError::LifecycleConflict { .. }
        | SkillStoreError::CorruptData { .. }
        | SkillStoreError::EncodingFailure { .. }
        | SkillStoreError::Reference(_) => RunError::internal(message),
    }
}

pub(crate) fn anthropic_skill(id: &str) -> Option<SkillVersion> {
    let description = match id {
        "pptx" => "Create, inspect, and modify PowerPoint presentations",
        "xlsx" => "Create, inspect, and modify Excel workbooks",
        "docx" => "Create, inspect, and modify Word documents",
        "pdf" => "Create, inspect, and modify PDF documents",
        _ => return None,
    };
    let body = format!(
        "---\nname: {id}\ndescription: {description}\n---\nUse the available sandbox tools and libraries to {description}. Validate the generated artifact before returning it."
    );
    let files = vec![awaken_resource_contract::SkillBundleFile {
        path: "SKILL.md".into(),
        content: body.into_bytes(),
        executable: false,
    }];
    Some(SkillVersion {
        id: format!("anthropic-{id}-1").into(),
        skill_id: id.into(),
        version: 1,
        name: id.into(),
        description: description.into(),
        directory: format!("/skills/{id}"),
        bundle_sha256: awaken_resource_contract::skill_bundle_sha256(&files),
        files,
        created_unix_nanos: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_types::RunErrorKind;

    #[test]
    fn session_skill_repository_failures_follow_the_closed_error_table() {
        // Cause/effect decision table: caller-controlled missing/invalid
        // selectors => 400; IO, missing authority and indeterminate backend
        // outcomes => 503; corrupt data/encoding or impossible conflicts =>
        // 500. SQL and filesystem failures now share the same cause policy.
        // Reference-index refusals preserve their owned Resource error; they
        // cannot be mistaken for a missing Skill or permission to publish. Every runtime Skill
        // entry point uses this one adapter so create, restore, and live apply
        // cannot classify the same repository cause differently.
        let rules = [
            (
                SkillStoreError::NotFound("x".into()),
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::InvalidSelection { detail: "x".into() },
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::InvalidDefinition { detail: "x".into() },
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::InvalidVersion { detail: "x".into() },
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::BundleMismatch { detail: "x".into() },
                RunErrorKind::BadRequest,
            ),
            (
                SkillStoreError::from(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "x",
                )),
                RunErrorKind::Unavailable,
            ),
            (
                SkillStoreError::AlreadyExists("x".into()),
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::VersionConflict("x".into()),
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::LifecycleConflict { detail: "x".into() },
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::Indeterminate { detail: "x".into() },
                RunErrorKind::Unavailable,
            ),
            (
                SkillStoreError::CorruptData { detail: "x".into() },
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::EncodingFailure { detail: "x".into() },
                RunErrorKind::Internal,
            ),
            (
                SkillStoreError::AuthorityUnavailable { detail: "x".into() },
                RunErrorKind::Unavailable,
            ),
            (
                SkillStoreError::Reference(
                    awaken_resource_contract::ResourcePurgeError::StaleClaim,
                ),
                RunErrorKind::Internal,
            ),
        ];
        for (cause, effect) in rules {
            assert_eq!(skill_store_run_error(cause).kind, effect);
        }
    }
}
