// @formal surface obligation=session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal surface obligation=session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal reason: Session input admission and mount-path rules moved from the Managed adapter: input
// @formal reason: limits, Repository mount preflight, derived MemoryStore mount paths, Agent-default
// @formal reason: replacement by exact identity or mount slot, and Repository participant lowering with
// @formal reason: compensation. The frozen resolver and aggregate remain the authorities it feeds.
//! Session input requests: the admission limits, mount-path rules and
//! Agent-default replacement plan every protocol's Session inputs share.
//!
//! A protocol lowers its wire resources into [`SessionInputRequest`]s once;
//! everything after that (limits, derived MemoryStore paths, which Agent
//! binding an input replaces, Repository participants) is decided here, so no
//! two protocols can freeze different layouts for the same request.

use awaken_session_types::RunError;

/// Most File inputs one Session carries.
pub(crate) const MAX_SESSION_FILE_RESOURCES: usize = 500;
/// Most MemoryStore inputs one Session carries.
pub(crate) const MAX_SESSION_MEMORY_STORES: usize = 8;
/// Longest MemoryStore instructions, in characters.
pub(crate) const MAX_MEMORY_INSTRUCTIONS_CHARS: usize = 4_096;

/// What one Session input names.
#[derive(Debug, Clone)]
pub enum SessionInputTarget {
    File(awaken_resource_contract::FileId),
    MemoryStore(awaken_resource_contract::MemoryStoreId),
    Repository {
        remote_url: String,
        credential_material: Option<awaken_credential_contract::CredentialMaterialInput>,
        initial_branch: Option<String>,
        initial_commit: Option<String>,
    },
}

impl SessionInputTarget {
    /// Whether this target can retain an already-resolved source. The caller
    /// still selects the matching mount and retains its complete input;
    /// access and instructions are authored separately from this source rule.
    #[must_use]
    pub(crate) fn can_reuse_resolved_source(
        &self,
        source: &crate::session_resource::ResolvedInputSource,
    ) -> bool {
        use crate::session_resource::ResolvedInputSource;
        match (self, source) {
            (Self::File(requested), ResolvedInputSource::File { file_id }) => requested == file_id,
            (
                Self::MemoryStore(requested),
                ResolvedInputSource::MemoryStore {
                    memory_store_id, ..
                },
            ) => requested == memory_store_id,
            (
                Self::Repository {
                    remote_url,
                    credential_material,
                    initial_branch,
                    initial_commit,
                },
                ResolvedInputSource::Repository { config, .. },
            ) => {
                credential_material.is_none()
                    && remote_url == &config.remote_url
                    && initial_branch == &config.initial_branch
                    && initial_commit == &config.initial_commit
            }
            _ => false,
        }
    }
}

/// One requested Session input, lowered from a protocol's wire resource.
///
/// Consumers: Session creation and resource replacement, which admit, place
/// and bind every request through the rules of this module.
#[derive(Debug, Clone)]
pub struct SessionInputRequest {
    pub target: SessionInputTarget,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    pub instructions: Option<String>,
    /// A MemoryStore input without an authored path. Its mount is derived from
    /// the store definition's display name before anything is frozen, so the
    /// Session never freezes a shared literal.
    pub derived_memory_mount: bool,
}

impl SessionInputRequest {
    /// A MemoryStore input whose mount path the definition derives.
    #[must_use]
    pub fn derived_memory_store(
        memory_store_id: awaken_resource_contract::MemoryStoreId,
        access: awaken_resource_contract::ResourceAccess,
        instructions: Option<String>,
    ) -> Self {
        // This provisional spelling is never exposed or frozen: preparation
        // always replaces it with the definition-owned display-name projection.
        let mount_path = format!(
            "/mnt/memory/{}",
            memory_mount_component(memory_store_id.as_str())
        );
        Self {
            target: SessionInputTarget::MemoryStore(memory_store_id),
            mount_path,
            access,
            instructions,
            derived_memory_mount: true,
        }
    }
}

/// The family of one input, for mount-slot comparisons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) enum InputProjectionClass {
    File,
    MemoryStore,
    Repository,
}

impl InputProjectionClass {
    #[must_use]
    pub(crate) fn of_request(target: &SessionInputTarget) -> Self {
        match target {
            SessionInputTarget::File(_) => Self::File,
            SessionInputTarget::MemoryStore(_) => Self::MemoryStore,
            SessionInputTarget::Repository { .. } => Self::Repository,
        }
    }

    #[must_use]
    pub(crate) fn of_binding(target: &awaken_resource_contract::InputResourceId) -> Self {
        match target {
            awaken_resource_contract::InputResourceId::File(_) => Self::File,
            awaken_resource_contract::InputResourceId::MemoryStore(_) => Self::MemoryStore,
            awaken_resource_contract::InputResourceId::Repository(_) => Self::Repository,
        }
    }

    #[must_use]
    pub(crate) fn of_resolved(source: &crate::session_resource::ResolvedInputSource) -> Self {
        match source {
            crate::session_resource::ResolvedInputSource::File { .. } => Self::File,
            crate::session_resource::ResolvedInputSource::MemoryStore { .. } => Self::MemoryStore,
            crate::session_resource::ResolvedInputSource::Repository { .. } => Self::Repository,
        }
    }
}

/// MemoryStore inputs: at most [`MAX_SESSION_MEMORY_STORES`], each with
/// instructions of at most [`MAX_MEMORY_INSTRUCTIONS_CHARS`] characters.
pub(crate) fn admit_memory_inputs(requests: &[SessionInputRequest]) -> Result<(), RunError> {
    let mut memory_count = 0usize;
    for request in requests {
        if !matches!(request.target, SessionInputTarget::MemoryStore(_)) {
            continue;
        }
        memory_count += 1;
        if request.instructions.as_ref().is_some_and(|instructions| {
            instructions.chars().count() > MAX_MEMORY_INSTRUCTIONS_CHARS
        }) {
            return Err(RunError::bad_request(format!(
                "memory store instructions support at most {MAX_MEMORY_INSTRUCTIONS_CHARS} characters"
            )));
        }
    }
    if memory_count > MAX_SESSION_MEMORY_STORES {
        return Err(RunError::bad_request(format!(
            "a Session supports at most {MAX_SESSION_MEMORY_STORES} memory stores"
        )));
    }
    Ok(())
}

/// Validate the complete explicit Repository mount set before any catalog,
/// Registry, Vault or provider effect.
pub(crate) fn preflight_repository_mount_paths(
    requests: &[SessionInputRequest],
) -> Result<(), RunError> {
    let paths = requests
        .iter()
        .filter(|request| matches!(&request.target, SessionInputTarget::Repository { .. }))
        .map(|request| request.mount_path.as_str())
        .collect::<Vec<_>>();
    awaken_provisioning_contract::validate_repository_mount_paths(&paths, &[])
        .map_err(|error| RunError::bad_request(error.to_string()))
}

/// At most [`MAX_SESSION_FILE_RESOURCES`] File inputs.
pub(crate) fn admit_file_count(requests: &[SessionInputRequest]) -> Result<(), RunError> {
    if requests
        .iter()
        .filter(|request| matches!(request.target, SessionInputTarget::File(_)))
        .count()
        > MAX_SESSION_FILE_RESOURCES
    {
        return Err(RunError::bad_request(format!(
            "a Session supports at most {MAX_SESSION_FILE_RESOURCES} files"
        )));
    }
    Ok(())
}

/// MemoryStore mount component derived from a display label: lowercase
/// alphanumeric runs joined by `-`, at most 80 characters, `store` if empty.
#[must_use]
pub(crate) fn memory_mount_component(label: &str) -> String {
    let mut component = String::new();
    let mut separated = false;
    for character in label.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if separated && !component.is_empty() {
                component.push('-');
            }
            component.push(character);
            separated = false;
        } else {
            separated = true;
        }
        if component.chars().count() >= 80 {
            break;
        }
    }
    let component = component.trim_matches('-');
    if component.is_empty() {
        "store".to_string()
    } else {
        component.to_string()
    }
}

/// The derived mount path of a MemoryStore input: its display-name path,
/// qualified by its id and then a counter until no used mount collides.
#[must_use]
pub(crate) fn unique_memory_mount_path(
    display_name: &str,
    memory_store_id: &str,
    used_mounts: &std::collections::BTreeSet<String>,
) -> String {
    let base = format!("mnt/memory/{}", memory_mount_component(display_name));
    if !used_mounts.contains(&base) {
        return format!("/{base}");
    }
    let qualified = format!("{base}-{}", memory_mount_component(memory_store_id));
    if !used_mounts.contains(&qualified) {
        return format!("/{qualified}");
    }
    for suffix in 2_u16.. {
        let candidate = format!("{qualified}-{suffix}");
        if !used_mounts.contains(&candidate) {
            return format!("/{candidate}");
        }
    }
    unreachable!("the finite Session resource set always has an available suffix")
}

/// Lower one request into the shared typed binding language. The caller
/// supplies the platform id for a Repository after configuring it.
///
/// # Panics
///
/// A Repository request without its configured platform id.
#[must_use]
pub(crate) fn input_binding(
    binding_id: String,
    request: &SessionInputRequest,
    repository_id: Option<awaken_resource_contract::RepositoryId>,
) -> awaken_resource_contract::InputBinding {
    use awaken_resource_contract::{BindingId, InputBinding, InputResourceId};

    let target = match &request.target {
        SessionInputTarget::File(file_id) => InputResourceId::File(file_id.clone()),
        SessionInputTarget::MemoryStore(memory_store_id) => {
            InputResourceId::MemoryStore(memory_store_id.clone())
        }
        SessionInputTarget::Repository { .. } => InputResourceId::Repository(
            repository_id.expect("Repository lowering supplies a platform id"),
        ),
    };
    InputBinding {
        binding_id: BindingId::new(binding_id),
        target,
        mount_path: request.mount_path.clone(),
        access: request.access,
        instructions: request.instructions.clone(),
    }
}

pub(super) fn plan_session_input_attachments(
    session_id: &str,
    requests: &[SessionInputRequest],
    agent_defaults: &[awaken_resource_contract::InputBinding],
) -> Result<Vec<crate::session_resource::SessionInputAttachment>, String> {
    requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let repository_id = matches!(&request.target, SessionInputTarget::Repository { .. })
                .then(|| awaken_resource_contract::RepositoryId::from("preflight"));
            let mut binding = input_binding(
                format!("session:{session_id}:input:{index}"),
                request,
                repository_id,
            );
            let replaces = if let SessionInputTarget::MemoryStore(requested) = &request.target {
                // A MemoryStore input has no authored path or binding id. Its
                // exact logical resource identity is therefore the only stable
                // key shared with an Agent default. Never infer a replacement
                // from array order, cardinality, or the derived display path.
                let mut exact = agent_defaults.iter().filter(|default| {
                    matches!(
                        &default.target,
                        awaken_resource_contract::InputResourceId::MemoryStore(existing)
                            if existing == requested
                    )
                });
                let replacement = exact.next().map(|default| default.binding_id.clone());
                if exact.next().is_some() {
                    return Err(format!(
                        "MemoryStore `{requested}` matches multiple Agent bindings; the Managed Session input cannot select one"
                    ));
                }
                replacement
            } else {
                let normalized = binding.mount_path.trim_start_matches('/');
                agent_defaults
                    .iter()
                    .find(|default| {
                        default.mount_path.trim_start_matches('/') == normalized
                            && InputProjectionClass::of_binding(&default.target)
                                == InputProjectionClass::of_request(&request.target)
                    })
                    .map(|default| default.binding_id.clone())
            };
            if let Some(replaced) = &replaces {
                binding.binding_id.clone_from(replaced);
            }
            Ok(crate::session_resource::SessionInputAttachment { binding, replaces })
        })
        .collect()
}

async fn memory_store_definition(
    inventory: Option<&dyn awaken_resource_contract::ResourceInventory>,
    workspace_id: &str,
    memory_store_id: &str,
) -> Result<awaken_resource_contract::MemoryStoreDefinition, RunError> {
    inventory
        .ok_or_else(|| {
            RunError::bad_request("memory resources require a configured Resource Registry")
        })?
        .find_memory_store(workspace_id, memory_store_id)
        .await
        .map_err(|error| RunError::bad_request(error.to_string()))?
        .ok_or_else(|| {
            RunError::bad_request(format!(
                "MemoryStore `{memory_store_id}` not found in this workspace"
            ))
        })
}

/// Normalize definition-owned mounts before either caller configures a participant.
/// Explicit paths and Agent defaults reserve slots; no derived request means
/// no Inventory capability or read is required. Identity/layout planning stays
/// with the caller and retains its original order after this one normalization.
pub(super) async fn normalize_session_input_mounts(
    inventory: Option<&dyn awaken_resource_contract::ResourceInventory>,
    owner_scope: &str,
    requests: &[SessionInputRequest],
    agent_defaults: &[awaken_resource_contract::InputBinding],
) -> Result<Vec<SessionInputRequest>, RunError> {
    let mut used_mounts = agent_defaults
        .iter()
        .map(|binding| binding.mount_path.trim_start_matches('/').to_string())
        .chain(
            requests
                .iter()
                .filter(|request| !request.derived_memory_mount)
                .map(|request| request.mount_path.trim_start_matches('/').to_string()),
        )
        .collect::<std::collections::BTreeSet<_>>();
    // Resolve every definition-owned mount before entering either durable
    // Repository participant. A later malformed Memory/File input cannot
    // strand an earlier Registry/Vault participant.
    let mut normalized = Vec::with_capacity(requests.len());
    for request in requests {
        let mut request = request.clone();
        if request.derived_memory_mount {
            let SessionInputTarget::MemoryStore(memory_store_id) = &request.target else {
                unreachable!("only MemoryStore inputs derive a mount path")
            };
            let definition =
                memory_store_definition(inventory, owner_scope, memory_store_id.as_str()).await?;
            request.mount_path =
                unique_memory_mount_path(&definition.name, memory_store_id.as_str(), &used_mounts);
            // All explicit paths were seeded before the loop, including later
            // requests. Only newly derived paths need to extend that same set.
            used_mounts.insert(request.mount_path.trim_start_matches('/').to_string());
        }
        normalized.push(request);
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_source_reuse_follows_exact_target_identity() {
        use crate::session_resource::ResolvedInputSource;
        use awaken_resource_contract::{
            ClonePolicy, ConfigVersion, MemoryStoreConfigVersion, RepositoryConfigVersion,
            RetentionPolicy,
        };

        // Cause/effect graph: C1 target/source families match; C2 File/Memory
        // ids match; C3 Repository material is absent; C4 URL, C5 branch and C6
        // commit match. E1 reuse the existing source; E2 resolve a new source.
        // Rules: R1 !C1 => E2; R2 File/Memory+C1+C2 => E1, !C2 => E2;
        // R3 Repository+C1+C3+C4+C5+C6 => E1; each single failed C3-C6 => E2.
        // The 3x3 table owns R1 and the same-family positives. R2's different
        // ids and R3's independent failures follow. Matching None, branch and
        // commit selections remain distinct accepted rows; branch+commit is
        // excluded by Repository aggregate validation, not this predicate.
        // Mount, access and instructions are not source facts: the original
        // caller still selects same_mount, clones the complete ResolvedInput,
        // then replaces access/instructions. The real K1 and Memory manifest
        // tests own binding/config/pin preservation; this pure table does not
        // qualify Registry/Vault, root-CAS or Runtime effects.
        let repository_source =
            |branch: Option<&str>, commit: Option<&str>| ResolvedInputSource::Repository {
                repository_id: "repository".into(),
                config: RepositoryConfigVersion {
                    repository_id: "repository".into(),
                    version: ConfigVersion(7),
                    remote_url: "https://example.test/repository.git".into(),
                    credential_binding: Some("repository:credential".into()),
                    initial_branch: branch.map(str::to_string),
                    initial_commit: commit.map(str::to_string),
                    clone_policy: ClonePolicy::default(),
                },
                credential: None,
            };
        let repository_target =
            |material: bool, url: &str, branch: Option<&str>, commit: Option<&str>| {
                SessionInputTarget::Repository {
                    remote_url: url.into(),
                    credential_material: material.then(|| {
                        awaken_credential_contract::CredentialMaterialInput::structured(
                            "github",
                            awaken_credential_contract::http_basic_material(
                                awaken_agent_contract::RedactedString::new("user"),
                                awaken_agent_contract::RedactedString::new("replacement"),
                            ),
                        )
                    }),
                    initial_branch: branch.map(str::to_string),
                    initial_commit: commit.map(str::to_string),
                }
            };
        let url = "https://example.test/repository.git";
        let sources = [
            ResolvedInputSource::File {
                file_id: "file".into(),
            },
            ResolvedInputSource::MemoryStore {
                memory_store_id: "memory".into(),
                config: MemoryStoreConfigVersion {
                    memory_store_id: "memory".into(),
                    version: ConfigVersion(4),
                    retention_policy: RetentionPolicy::default(),
                },
            },
            repository_source(None, None),
        ];
        let targets = [
            SessionInputTarget::File("file".into()),
            SessionInputTarget::MemoryStore("memory".into()),
            repository_target(false, url, None, None),
        ];
        let expected = [
            [true, false, false],
            [false, true, false],
            [false, false, true],
        ];
        for (target_index, target) in targets.iter().enumerate() {
            for (source_index, source) in sources.iter().enumerate() {
                assert_eq!(
                    target.can_reuse_resolved_source(source),
                    expected[target_index][source_index],
                    "R1/R2/R3 family {target_index}/{source_index}"
                );
            }
        }
        assert!(
            !SessionInputTarget::File("other-file".into()).can_reuse_resolved_source(&sources[0]),
            "R2 different File identity"
        );
        assert!(
            !SessionInputTarget::MemoryStore("other-memory".into())
                .can_reuse_resolved_source(&sources[1]),
            "R2 different Memory identity"
        );
        for (rule, target) in [
            ("C3 material", repository_target(true, url, None, None)),
            (
                "C4 URL",
                repository_target(false, "https://example.test/other.git", None, None),
            ),
            (
                "C5 branch",
                repository_target(false, url, Some("main"), None),
            ),
            (
                "C6 commit",
                repository_target(false, url, None, Some("0123456789abcdef")),
            ),
        ] {
            assert!(!target.can_reuse_resolved_source(&sources[2]), "R3 {rule}");
        }
        for (branch, commit) in [(Some("main"), None), (None, Some("0123456789abcdef"))] {
            assert!(
                repository_target(false, url, branch, commit)
                    .can_reuse_resolved_source(&repository_source(branch, commit)),
                "R3 matching checkout selection"
            );
        }
    }

    #[test]
    fn derived_memory_mounts_are_unique_and_display_named() {
        // Causes: C1 the display-name path is free; C2 it is used; C3 the
        // id-qualified path is used too; C4 the label has no alphanumerics.
        // Effects: E1 the display path; E2 the id-qualified path; E3 a counter
        // suffix; E4 the `store` fallback component.
        let used = |paths: &[&str]| {
            paths
                .iter()
                .map(|path| (*path).to_string())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            unique_memory_mount_path("Team Notes", "mem_1", &used(&[])),
            "/mnt/memory/team-notes",
            "E1"
        );
        assert_eq!(
            unique_memory_mount_path("Team Notes", "mem_1", &used(&["mnt/memory/team-notes"])),
            "/mnt/memory/team-notes-mem-1",
            "E2"
        );
        assert_eq!(
            unique_memory_mount_path(
                "Team Notes",
                "mem_1",
                &used(&["mnt/memory/team-notes", "mnt/memory/team-notes-mem-1"])
            ),
            "/mnt/memory/team-notes-mem-1-2",
            "E3"
        );
        assert_eq!(memory_mount_component("!!!"), "store", "E4");
    }

    #[test]
    fn admission_limits_decision_table() {
        let memory = |instructions: Option<String>| {
            SessionInputRequest::derived_memory_store(
                awaken_resource_contract::MemoryStoreId::from("mem"),
                awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions,
            )
        };
        let file = || SessionInputRequest {
            target: SessionInputTarget::File(awaken_resource_contract::FileId::from("file")),
            mount_path: "/mnt/session/uploads/file".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
            derived_memory_mount: false,
        };
        assert!(
            admit_memory_inputs(&vec![memory(None); MAX_SESSION_MEMORY_STORES]).is_ok(),
            "M1"
        );
        assert!(
            admit_memory_inputs(&vec![memory(None); MAX_SESSION_MEMORY_STORES + 1]).is_err(),
            "M2"
        );
        assert!(
            admit_memory_inputs(&[memory(Some("x".repeat(MAX_MEMORY_INSTRUCTIONS_CHARS + 1)))])
                .is_err(),
            "M3"
        );
        assert!(
            admit_file_count(&vec![file(); MAX_SESSION_FILE_RESOURCES]).is_ok(),
            "F1"
        );
        assert!(
            admit_file_count(&vec![file(); MAX_SESSION_FILE_RESOURCES + 1]).is_err(),
            "F2"
        );
    }

    fn parsed_memory(id: &str, mount_path: &str) -> SessionInputRequest {
        SessionInputRequest {
            target: SessionInputTarget::MemoryStore(id.into()),
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
            derived_memory_mount: true,
        }
    }

    fn binding(
        binding_id: &str,
        target: awaken_resource_contract::InputResourceId,
        mount_path: &str,
    ) -> awaken_resource_contract::InputBinding {
        awaken_resource_contract::InputBinding {
            binding_id: binding_id.into(),
            target,
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }
    }

    #[test]
    fn managed_memory_replacement_uses_exact_identity_without_path_or_order_fallback() {
        use awaken_resource_contract::{FileId, InputResourceId, MemoryStoreId};

        // Managed replacement cause/effect table:
        // | Rule | exact Memory id matches | count | input | Effect |
        // | R1 | yes | one | Memory, different derived path | explicit replace; preserve binding_id |
        // | R2 | no | zero | Memory, colliding path | no replace; resolver rejects collision |
        // | R3 | yes | multiple | Memory | reject ambiguity; never select first |
        // | R4 | n/a | n/a | File, explicit matching path | retain path replacement |
        // Constraints: Memory wire has neither binding_id nor mount_path; array
        // order, a sole Memory item, and a display-name path are not identity.
        let memory_id = MemoryStoreId::from("memory-1");
        let exact_default = binding(
            "profile-memory",
            InputResourceId::MemoryStore(memory_id.clone()),
            "/agent/path",
        );
        let r1 = plan_session_input_attachments(
            "session-1",
            &[parsed_memory("memory-1", "/mnt/memory/catalog-name")],
            std::slice::from_ref(&exact_default),
        )
        .expect("R1 exact identity");
        assert_eq!(
            r1[0].replaces.as_ref(),
            Some(&exact_default.binding_id),
            "R1"
        );
        assert_eq!(r1[0].binding.binding_id, exact_default.binding_id, "R1");
        assert_eq!(r1[0].binding.mount_path, "/mnt/memory/catalog-name", "R1");

        let different_default = binding(
            "other-memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-other")),
            "/mnt/memory/catalog-name",
        );
        let r2 = plan_session_input_attachments(
            "session-2",
            &[parsed_memory("memory-1", "/mnt/memory/catalog-name")],
            std::slice::from_ref(&different_default),
        )
        .expect("R2 remains additive");
        assert_eq!(r2[0].replaces, None, "R2 no path fallback");
        assert!(
            awaken_session_configuration_contract::resource::SessionInputResolver::effective_bindings(
                &[different_default],
                &r2,
            )
            .is_err(),
            "R2 colliding additive Memory inputs fail closed"
        );

        let duplicate_defaults = vec![
            exact_default.clone(),
            binding(
                "profile-memory-copy",
                InputResourceId::MemoryStore(memory_id),
                "/agent/other-path",
            ),
        ];
        assert!(
            plan_session_input_attachments(
                "session-3",
                &[parsed_memory("memory-1", "/mnt/memory/catalog-name")],
                &duplicate_defaults,
            )
            .is_err(),
            "R3 duplicate identity is ambiguous"
        );

        let file_default = binding(
            "profile-file",
            InputResourceId::File(FileId::from("file-default")),
            "/workspace/input",
        );
        let file = SessionInputRequest {
            target: SessionInputTarget::File(FileId::from("file-session")),
            mount_path: "/workspace/input".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
            derived_memory_mount: false,
        };
        let r4 = plan_session_input_attachments(
            "session-4",
            &[file],
            std::slice::from_ref(&file_default),
        )
        .expect("R4 explicit File path");
        assert_eq!(
            r4[0].replaces.as_ref(),
            Some(&file_default.binding_id),
            "R4"
        );
    }
}
