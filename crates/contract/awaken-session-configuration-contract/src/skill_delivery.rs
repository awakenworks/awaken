// @formal entrypoint skill.invalid_workspace_or_hash_fails_closed
// @formal entrypoint skill.loaded_version_equals_session_pin
//! The pinned Skill bundles a Session's Resource manifest selects: the one
//! loader every holder of a frozen manifest reads them through (the
//! Coordinator before it reserves a Run, the claiming Worker's realization
//! before it installs them), their delivered directory and their `SKILL.md`.
//!
//! A manifest pins each custom bundle by its content digest; the bytes live in
//! the content-addressed Skill store. A missing bundle or one whose bytes do
//! not hash to the pin is refused, never substituted.

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{SkillStoreError, SkillVersion, skill_stem};
use awaken_runtime_contract::runs::RunError;

use crate::{ResolvedSkillBinding, SkillBundleSource};

/// The Session-relative directory delivered Skill bundles are installed under.
pub const DELIVERED_SKILLS_SUBDIR: &str =
    awaken_provisioning_contract::WorkspaceLayout::DELIVERED_SKILLS_SUBDIR;

/// The delivered directory of Skill `id`.
#[must_use]
pub fn delivered_skill_directory(id: &str) -> String {
    format!("{DELIVERED_SKILLS_SUBDIR}/{}", skill_stem(id))
}

/// The `SKILL.md` of a delivered bundle, refused unless its bytes hash to the
/// bundle's own digest.
pub fn delivered_skill_content(version: &SkillVersion) -> Result<&str, String> {
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

/// The built-in Anthropic-compatible Skill `id`, at its one version.
#[must_use]
pub fn anthropic_skill(id: &str) -> Option<SkillVersion> {
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

/// Load the exact bundles `bindings` pin, in order. A custom bundle is read
/// from `source` under the fence `fence_for` selects for it.
///
/// Decision table (each binding; the first refusal ends the load):
/// | Rule | kind | bundle | Effect |
/// |---|---|---|---|
/// | P1 | Anthropic | known id at the pinned version | the built-in bundle |
/// | P2 | Anthropic | unknown id or another version | `NotFound` |
/// | P3 | custom | no source | `AuthorityUnavailable` |
/// | P4 | custom | the source has no such bundle | `NotFound` |
/// | P5 | custom | the source fails | `Indeterminate` |
/// | P6 | any | bytes whose digest is not the pin | `CorruptData` |
/// | P7 | custom | bytes whose digest is the pin | the stored bundle |
pub async fn load_pinned_skills<C: Sync>(
    source: Option<&dyn SkillBundleSource<C>>,
    workspace: &str,
    bindings: &[ResolvedSkillBinding],
    fence_for: impl Fn(&ResolvedSkillBinding) -> Result<Option<C>, SkillStoreError>,
) -> Result<Vec<SkillVersion>, SkillStoreError> {
    let mut versions = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let version = match binding.kind {
            AgentSkillKind::Anthropic => anthropic_skill(&binding.skill_id)
                .filter(|version| version.version == binding.version)
                .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?,
            AgentSkillKind::Custom => {
                let fence = fence_for(binding)?;
                source
                    .ok_or_else(|| SkillStoreError::AuthorityUnavailable {
                        detail: "no Skill bundle source".into(),
                    })?
                    .load(workspace, binding, fence.as_ref())
                    .await
                    .map_err(|error| SkillStoreError::Indeterminate {
                        detail: error.to_string(),
                    })?
                    .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?
            }
        };
        if version.bundle_sha256 != binding.bundle_sha256 {
            return Err(SkillStoreError::CorruptData {
                detail: format!(
                    "Skill {} version {} hash changed",
                    binding.skill_id, binding.version
                ),
            });
        }
        versions.push(version);
    }
    Ok(versions)
}

/// Resolve an Agent's Skill selections to the exact versions a Session pins:
/// a built-in Skill at its one version, a custom Skill through the Workspace's
/// catalog.
pub async fn resolve_skill_selections(
    catalog: Option<&dyn crate::SkillCatalogApplication>,
    workspace: &str,
    selections: &[awaken_agent_contract::AgentSkillBinding],
) -> Result<Vec<ResolvedSkillBinding>, SkillStoreError> {
    awaken_agent_contract::validate_agent_skills(selections)
        .map_err(|error| SkillStoreError::InvalidSelection { detail: error })?;
    let mut bindings = Vec::with_capacity(selections.len());
    for selection in selections {
        bindings.push(match selection.kind {
            AgentSkillKind::Anthropic => {
                let version = anthropic_skill(&selection.skill_id)
                    .ok_or_else(|| SkillStoreError::NotFound(selection.skill_id.clone()))?;
                ResolvedSkillBinding {
                    kind: selection.kind,
                    skill_id: selection.skill_id.clone(),
                    version: version.version,
                    bundle_sha256: version.bundle_sha256,
                }
            }
            AgentSkillKind::Custom => {
                catalog
                    .ok_or_else(|| SkillStoreError::AuthorityUnavailable {
                        detail: "no Skill catalog application".into(),
                    })?
                    .resolve_custom(workspace, &selection.skill_id, &selection.version)
                    .await?
            }
        });
    }
    Ok(bindings)
}

/// The shared closed classification of Skill repository failures.
pub fn skill_store_run_error(error: SkillStoreError) -> RunError {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SkillBundleSourceError;
    use awaken_resource_contract::{SkillBundleFile, skill_bundle_sha256};
    use awaken_runtime_contract::runs::RunErrorKind;

    fn version(skill_id: &str, body: &[u8]) -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: body.to_vec(),
            executable: false,
        }];
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

    fn binding(kind: AgentSkillKind, version: &SkillVersion) -> ResolvedSkillBinding {
        ResolvedSkillBinding {
            kind,
            skill_id: version.skill_id.to_string(),
            version: version.version,
            bundle_sha256: version.bundle_sha256.clone(),
        }
    }

    /// Serves one stored bundle, or fails, by Skill id.
    struct Store {
        stored: Vec<SkillVersion>,
        failing: &'static str,
    }

    #[async_trait::async_trait]
    impl SkillBundleSource for Store {
        async fn load(
            &self,
            _workspace_id: &str,
            binding: &ResolvedSkillBinding,
            _fence: Option<&()>,
        ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
            if binding.skill_id == self.failing {
                return Err(SkillBundleSourceError::new("store unavailable"));
            }
            Ok(self
                .stored
                .iter()
                .find(|version| version.skill_id.as_str() == binding.skill_id)
                .cloned())
        }
    }

    async fn load(
        source: Option<&Store>,
        bindings: &[ResolvedSkillBinding],
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        load_pinned_skills(
            source.map(|store| store as &dyn SkillBundleSource),
            "workspace",
            bindings,
            |_| Ok(None),
        )
        .await
    }

    #[tokio::test]
    async fn pinned_skill_loading_follows_its_decision_table() {
        let stored = version("custom", b"---\nname: custom\n---\nbody");
        let store = Store {
            stored: vec![stored.clone()],
            failing: "failing",
        };
        let builtin = anthropic_skill("pdf").expect("built-in pdf");

        let loaded = load(
            Some(&store),
            &[binding(AgentSkillKind::Anthropic, &builtin)],
        )
        .await
        .expect("P1");
        assert_eq!(loaded, vec![builtin.clone()], "P1");

        let mut other_version = binding(AgentSkillKind::Anthropic, &builtin);
        other_version.version = 2;
        assert!(
            matches!(
                load(Some(&store), &[other_version]).await,
                Err(SkillStoreError::NotFound(_))
            ),
            "P2"
        );

        let custom = binding(AgentSkillKind::Custom, &stored);
        assert!(
            matches!(
                load(None, std::slice::from_ref(&custom)).await,
                Err(SkillStoreError::AuthorityUnavailable { .. })
            ),
            "P3"
        );

        let missing = binding(
            AgentSkillKind::Custom,
            &version("missing", b"---\nname: missing\n---\nbody"),
        );
        assert!(
            matches!(
                load(Some(&store), &[missing]).await,
                Err(SkillStoreError::NotFound(_))
            ),
            "P4"
        );

        let failing = binding(
            AgentSkillKind::Custom,
            &version("failing", b"---\nname: failing\n---\nbody"),
        );
        assert!(
            matches!(
                load(Some(&store), &[failing]).await,
                Err(SkillStoreError::Indeterminate { .. })
            ),
            "P5"
        );

        let mut repinned = custom.clone();
        repinned.bundle_sha256 = "sha256:other".into();
        assert!(
            matches!(
                load(Some(&store), &[repinned]).await,
                Err(SkillStoreError::CorruptData { .. })
            ),
            "P6"
        );

        let loaded = load(Some(&store), &[custom]).await.expect("P7");
        assert_eq!(loaded, vec![stored], "P7");
    }

    #[test]
    fn delivered_content_requires_the_bundle_digest_and_a_skill_md() {
        let exact = version("custom", b"---\nname: custom\n---\nbody");
        assert_eq!(
            delivered_skill_content(&exact).expect("exact bundle"),
            "---\nname: custom\n---\nbody"
        );
        let mut tampered = exact.clone();
        tampered.files[0].content = b"tampered".to_vec();
        assert!(delivered_skill_content(&tampered).is_err(), "digest");
        let mut binary = version("binary", &[0xff, 0xfe]);
        binary.bundle_sha256 = skill_bundle_sha256(&binary.files);
        assert!(delivered_skill_content(&binary).is_err(), "UTF-8");
        assert_eq!(
            delivered_skill_directory("custom"),
            format!("{DELIVERED_SKILLS_SUBDIR}/custom")
        );
    }

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
