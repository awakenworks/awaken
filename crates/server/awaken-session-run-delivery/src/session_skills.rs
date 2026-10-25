//! The Skills a Session's Runs offer, as admission reads them: the Agent's
//! selected Skill ids, whether its tool policy can disclose a Skill through
//! the filesystem, and the one frozen registry slash-command expansion reads.
//!
//! A Managed Session discloses each attached Skill only through its pinned
//! `SKILL.md` path, so attached Skills require a filesystem tool. The registry
//! holds exactly the pinned bundles the Session's manifest selects, verified
//! against their digests; no live catalog or Environment root enters it.

use std::collections::BTreeSet;
use std::sync::Arc;

use awaken_ext_skills::{
    SkillEnvironment, SkillFile, SkillProvenance, SkillRegistry, SkillSource, SourceSkillRegistry,
};
use awaken_resource_contract::SkillVersion;
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_session_configuration_contract::SessionToolConfiguration;
use awaken_session_configuration_contract::skill_delivery::{
    delivered_skill_content, delivered_skill_directory,
};
use awaken_session_types::RunError;

/// The tools a filesystem-disclosed Skill can be read with.
const FILESYSTEM_TOOLS: [&str; 6] = ["bash", "read", "write", "edit", "glob", "grep"];

/// A frozen snapshot of Skill files, scanned as-is. Consumers: the Session
/// Skill registries built from pinned bundles or a frozen repository scan.
pub struct SkillSnapshot {
    files: Vec<SkillFile>,
}

impl SkillSnapshot {
    #[must_use]
    pub fn new(files: Vec<SkillFile>) -> Self {
        Self { files }
    }
}

impl SkillSource for SkillSnapshot {
    fn scan(&self) -> Result<Vec<SkillFile>, String> {
        Ok(self.files.clone())
    }
}

/// Whether a bundle needs a filesystem: it declares one, or carries
/// supporting files beside its `SKILL.md`.
#[must_use]
pub fn requires_filesystem(version: &SkillVersion, content: &str) -> bool {
    let declared = awaken_ext_skills::parse_skill_md(version.skill_id.to_string(), content);
    declared.environment == SkillEnvironment::Filesystem
        || version
            .files
            .iter()
            .any(|file| file.path != "SKILL.md" && !file.path.ends_with("/SKILL.md"))
}

/// The registry file of one pinned bundle, refused unless its bytes hash to
/// its digest. It lives at its delivered directory when the Session delivers
/// Skills through the filesystem or the bundle needs one.
pub fn delivered_skill_file(
    version: &SkillVersion,
    filesystem_delivery: bool,
) -> Result<SkillFile, String> {
    let content = delivered_skill_content(version)?.to_string();
    let dir = (filesystem_delivery || requires_filesystem(version, &content))
        .then(|| delivered_skill_directory(version.skill_id.as_str()));
    Ok(SkillFile {
        id: version.skill_id.to_string(),
        content,
        dir,
    })
}

/// The Skill ids a published Agent selects; an unpublished Agent selects none
/// explicitly.
#[must_use]
pub fn published_skill_ids(snapshot: Option<&ExecutableAgentSnapshot>) -> Option<BTreeSet<String>> {
    snapshot.map(|snapshot| {
        snapshot
            .resolved_spec
            .plugin_config
            .agent
            .skills
            .iter()
            .map(|skill| skill.skill_id.clone())
            .collect()
    })
}

/// Whether the Session's tool policy enables `tool`: the Session's Agent
/// toolset decides when it has one, else the publication's tool descriptors
/// (an unpublished Agent allows every tool).
#[must_use]
pub fn session_allows_agent_tool(
    tools: Option<&SessionToolConfiguration>,
    publication: Option<&ExecutableAgentSnapshot>,
    tool: &str,
) -> bool {
    let toolsets = tools.map(|tools| tools.toolsets.as_slice()).or_else(|| {
        publication.map(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .toolsets
                .as_slice()
        })
    });
    if let Some(agent) = toolsets.and_then(|toolsets| {
        toolsets
            .iter()
            .find(|toolset| matches!(toolset.source, awaken_agent_contract::ToolsetSource::Agent))
    }) {
        return agent.policy_for(tool).enabled;
    }
    publication.is_none_or(|snapshot| {
        snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|descriptor| descriptor.id == tool)
    })
}

/// Whether the Session's tool policy enables any filesystem tool.
#[must_use]
pub fn session_allows_filesystem_tools(
    tools: Option<&SessionToolConfiguration>,
    publication: Option<&ExecutableAgentSnapshot>,
) -> bool {
    FILESYSTEM_TOOLS
        .iter()
        .any(|tool| session_allows_agent_tool(tools, publication, tool))
}

/// The refusal of attached Skills a Session cannot disclose.
pub const SKILLS_REQUIRE_FILESYSTEM: &str = "Managed Agent Skills require filesystem progressive disclosure, but this Session disables every filesystem tool";

/// The frozen Skill registry of a Managed Session: exactly its pinned bundles
/// the Agent selects, each at its delivered directory.
///
/// Decision table (causes → effect):
/// | Rule | selected pinned bundle | filesystem tool enabled | bundle digest | Effect |
/// |---|---|---|---|---|
/// | K1 | none | - | - | no registry |
/// | K2 | some | no | - | refused: attached Skills need a filesystem tool |
/// | K3 | some | yes | mismatched | refused |
/// | K4 | some | yes | exact | a registry of the selected bundles only |
pub fn managed_skill_registry(
    versions: Vec<SkillVersion>,
    publication: Option<&ExecutableAgentSnapshot>,
    tools: Option<&SessionToolConfiguration>,
) -> Result<Option<Arc<dyn SkillRegistry>>, RunError> {
    let selected = published_skill_ids(publication);
    let versions = versions
        .into_iter()
        .filter(|version| {
            selected
                .as_ref()
                .is_none_or(|selected| selected.contains(version.skill_id.as_str()))
        })
        .collect::<Vec<_>>();
    if versions.is_empty() {
        return Ok(None);
    }
    if !session_allows_filesystem_tools(tools, publication) {
        return Err(RunError::bad_request(SKILLS_REQUIRE_FILESYSTEM));
    }
    let files = versions
        .iter()
        .map(|version| delivered_skill_file(version, true))
        .collect::<Result<Vec<_>, _>>()
        .map_err(RunError::internal)?;
    Ok(Some(Arc::new(SourceSkillRegistry::new(
        Arc::new(SkillSnapshot::new(files)),
        SkillProvenance::Delivered,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{SkillBundleFile, skill_bundle_sha256};

    fn version_with(skill_id: &str, files: Vec<SkillBundleFile>) -> SkillVersion {
        SkillVersion {
            id: format!("{skill_id}-1").into(),
            skill_id: skill_id.into(),
            version: 1,
            name: skill_id.into(),
            description: String::new(),
            directory: format!("/skills/{skill_id}"),
            bundle_sha256: skill_bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }
    }

    fn skill_md(body: &str) -> SkillBundleFile {
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: body.as_bytes().to_vec(),
            executable: false,
        }
    }

    #[test]
    fn instruction_only_and_filesystem_bundles_select_the_minimum_substrate() {
        let pure_body = "---\ndescription: think\n---\nThink carefully.";
        let pure = version_with("pure", vec![skill_md(pure_body)]);
        assert!(!requires_filesystem(&pure, pure_body));

        let declared_body = "---\nenvironment: filesystem\n---\nRead the workspace.";
        let declared = version_with("declared", vec![skill_md(declared_body)]);
        assert!(requires_filesystem(&declared, declared_body));

        let bundled = version_with(
            "bundled",
            vec![
                skill_md(pure_body),
                SkillBundleFile {
                    path: "references/guide.md".into(),
                    content: b"guide".to_vec(),
                    executable: false,
                },
            ],
        );
        assert!(
            requires_filesystem(&bundled, pure_body),
            "supporting files override instruction-only metadata"
        );
    }

    fn publication(skills: &[&str], tools: &[&str]) -> ExecutableAgentSnapshot {
        let mut snapshot = ExecutableAgentSnapshot::builder("agent")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse(
                    "provider", "model", "native",
                )
                .expect("valid model"),
            )
            .build();
        snapshot.resolved_spec.plugin_config.agent.skills = skills
            .iter()
            .map(|id| awaken_agent_contract::AgentSkillBinding::custom(*id))
            .collect();
        snapshot.resolved_spec.tool_descriptors = tools
            .iter()
            .map(|id| {
                awaken_runtime_contract::resolved::ToolDescriptor::pinned(
                    "test",
                    *id,
                    *id,
                    serde_json::json!({"type": "object"}),
                )
            })
            .collect();
        snapshot
    }

    #[test]
    fn managed_skill_registry_follows_its_decision_table() {
        let selected = version_with(
            "selected",
            vec![skill_md("---\nname: selected\n---\nUse it.")],
        );
        let other = version_with("other", vec![skill_md("---\nname: other\n---\nUse it.")]);

        // K1: no selected pinned bundle builds no registry.
        let agent = publication(&["selected"], &["read"]);
        assert!(
            managed_skill_registry(vec![other.clone()], Some(&agent), None)
                .expect("K1")
                .is_none(),
            "K1"
        );

        // K2: a selected bundle without any filesystem tool is refused.
        let no_filesystem = publication(&["selected"], &["web_search"]);
        let refused = managed_skill_registry(vec![selected.clone()], Some(&no_filesystem), None)
            .err()
            .expect("K2");
        assert_eq!(refused.message, SKILLS_REQUIRE_FILESYSTEM, "K2");

        // K3: a bundle whose bytes do not hash to its digest is refused.
        let mut tampered = selected.clone();
        tampered.files[0].content = b"tampered".to_vec();
        assert!(
            managed_skill_registry(vec![tampered], Some(&agent), None).is_err(),
            "K3"
        );

        // K4: the registry holds the selected bundle only, at its delivered
        // directory.
        let registry = managed_skill_registry(vec![selected, other], Some(&agent), None)
            .expect("K4")
            .expect("K4 registry");
        let skill = registry
            .get("selected")
            .expect("K4 read")
            .expect("K4 skill");
        assert_eq!(skill.dir, Some(delivered_skill_directory("selected")), "K4");
        assert!(
            registry.get("other").expect("K4 read").is_none(),
            "K4 filter"
        );
    }
}
