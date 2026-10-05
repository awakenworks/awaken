//! The Agent-facing Skill catalogue prompt: rebuildable metadata derived from the
//! frozen Skill registry. The Session slot holds it; Skills behavior builds it.

use awaken_ext_skills::SkillRegistry;

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
