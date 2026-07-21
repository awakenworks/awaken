//! The neutral session-mounted resource (ADR-0038). The Managed wire adapter owns
//! the `resources[]` parse form + the DTO projection; this is the crate-boundary
//! shape both sides speak.

use std::collections::HashSet;

use awaken_resource_contract::{
    BindingId, InputBinding, InputResourceId, MemoryStoreConfigVersion, RepositoryConfigVersion,
    ResourceCatalogError, ResourceConfigSource,
};
use serde::{Deserialize, Serialize};

/// Pure Session-control-plane composer. Runtime receives only this resolved output
/// and never reads the Agent binding repository itself.
pub struct SessionInputResolver;

/// A temporary Session input and, when present, the exact Agent binding it
/// replaces. Replacement is explicit; array order is never authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputAttachment {
    pub binding: InputBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<BindingId>,
}

/// Resource-specific, secret-free configuration frozen once for a Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedInputSource {
    File {
        file_id: awaken_resource_contract::FileId,
    },
    MemoryStore {
        memory_store_id: awaken_resource_contract::MemoryStoreId,
        config: MemoryStoreConfigVersion,
    },
    Repository {
        repository_id: awaken_resource_contract::RepositoryId,
        config: RepositoryConfigVersion,
    },
}

/// One entry in the effective manifest handed to activation. Mutable Memory
/// content versions and Git revisions are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedInput {
    pub binding_id: BindingId,
    pub source: ResolvedInputSource,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Exact immutable Skill version selected for a Session. Bundle bytes remain in
/// the resource repository; this durable pin is secret-free and retry-safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSkillBinding {
    pub skill_id: String,
    pub version: u64,
    pub bundle_sha256: String,
}

/// Durable, secret-free result of the Session control plane's one resolution.
/// Inputs and Skill capabilities remain distinct collections because Skills are
/// executable capabilities, not mounted user inputs. `skills = None` means a
/// legacy record whose selection was not frozen; `Some([])` explicitly selects no
/// Skills.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSessionResources {
    pub inputs: Vec<ResolvedInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<ResolvedSkillBinding>>,
}

impl ResolvedSessionResources {
    /// Validate identity and mount invariants before an adapter performs any
    /// resource-specific side effect such as sealing a credential or creating a
    /// catalog definition.
    pub fn validate_new_binding(
        &self,
        binding_id: &BindingId,
        mount_path: &str,
    ) -> Result<(), SessionInputError> {
        let id = binding_id.as_str();
        if id.trim().is_empty()
            || self
                .inputs
                .iter()
                .any(|input| input.binding_id == *binding_id)
        {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        let mount_path = normalized_mount(mount_path)?;
        if self
            .inputs
            .iter()
            .any(|input| input.mount_path == mount_path)
        {
            return Err(SessionInputError::MountCollision(mount_path));
        }
        Ok(())
    }

    /// Add one already-resolved input while preserving unique binding ids and
    /// normalized, collision-free mount paths.
    pub fn attach(&self, input: ResolvedInput) -> Result<Self, SessionInputError> {
        self.validate_new_binding(&input.binding_id, &input.mount_path)?;
        let mut next = self.clone();
        next.inputs.push(input);
        validate_resolved_inputs(&mut next.inputs)?;
        Ok(next)
    }

    /// Replace one live binding without re-resolving any unaffected input.
    pub fn replace(&self, input: ResolvedInput) -> Result<Self, SessionInputError> {
        let mut next = self.clone();
        let current = next
            .inputs
            .iter_mut()
            .find(|current| current.binding_id == input.binding_id)
            .ok_or_else(|| SessionInputError::UnknownBinding(input.binding_id.to_string()))?;
        *current = input;
        validate_resolved_inputs(&mut next.inputs)?;
        Ok(next)
    }

    /// Remove one live binding, returning both the new manifest and removed input.
    pub fn detach(
        &self,
        binding_id: &BindingId,
    ) -> Result<(Self, ResolvedInput), SessionInputError> {
        let mut next = self.clone();
        let index = next
            .inputs
            .iter()
            .position(|input| &input.binding_id == binding_id)
            .ok_or_else(|| SessionInputError::UnknownBinding(binding_id.to_string()))?;
        let removed = next.inputs.remove(index);
        Ok((next, removed))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionInputError {
    #[error("binding id `{0}` is empty or duplicated")]
    InvalidBindingId(String),
    #[error("unsafe resource mount path `{0}`")]
    UnsafeMountPath(String),
    #[error("multiple resources claim mount path `{0}` without one explicit Session replacement")]
    MountCollision(String),
    #[error("Session attachment replaces unknown Agent binding `{0}`")]
    UnknownReplacement(String),
    #[error("Session input binding `{0}` was not found")]
    UnknownBinding(String),
    #[error(transparent)]
    Catalog(#[from] awaken_resource_contract::ResourceCatalogError),
}

fn normalized_mount(path: &str) -> Result<String, SessionInputError> {
    if path.contains('\0') {
        return Err(SessionInputError::UnsafeMountPath(path.into()));
    }
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| matches!(*component, "." | ".."))
    {
        return Err(SessionInputError::UnsafeMountPath(path.into()));
    }
    Ok(format!("/{}", components.join("/")))
}

fn validate_bindings(bindings: &mut [InputBinding]) -> Result<(), SessionInputError> {
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for binding in bindings {
        let id = binding.binding_id.as_str();
        if id.trim().is_empty() || !ids.insert(id.to_string()) {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        binding.mount_path = normalized_mount(&binding.mount_path)?;
        if !paths.insert(binding.mount_path.clone()) {
            return Err(SessionInputError::MountCollision(
                binding.mount_path.clone(),
            ));
        }
    }
    Ok(())
}

fn validate_resolved_inputs(inputs: &mut [ResolvedInput]) -> Result<(), SessionInputError> {
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for input in inputs {
        let id = input.binding_id.as_str();
        if id.trim().is_empty() || !ids.insert(id.to_string()) {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        input.mount_path = normalized_mount(&input.mount_path)?;
        if !paths.insert(input.mount_path.clone()) {
            return Err(SessionInputError::MountCollision(input.mount_path.clone()));
        }
    }
    Ok(())
}

impl SessionInputResolver {
    /// Compose typed Agent defaults and Session attachments. Only an explicit
    /// `replaces` removes an Agent binding; all remaining id/path collisions fail.
    pub fn compose(
        agent_defaults: &[InputBinding],
        session_attachments: &[SessionInputAttachment],
    ) -> Result<Vec<InputBinding>, SessionInputError> {
        let mut defaults = agent_defaults.to_vec();
        validate_bindings(&mut defaults)?;
        let mut attachments = session_attachments
            .iter()
            .map(|attachment| attachment.binding.clone())
            .collect::<Vec<_>>();
        validate_bindings(&mut attachments)?;

        let default_ids = defaults
            .iter()
            .map(|binding| binding.binding_id.clone())
            .collect::<HashSet<_>>();
        let mut replaced = HashSet::new();
        for attachment in session_attachments {
            if let Some(binding_id) = &attachment.replaces
                && default_ids.contains(binding_id)
            {
                replaced.insert(binding_id.clone());
            } else if let Some(binding_id) = &attachment.replaces {
                return Err(SessionInputError::UnknownReplacement(
                    binding_id.to_string(),
                ));
            }
        }

        let mut effective = attachments;
        effective.extend(
            defaults
                .into_iter()
                .filter(|binding| !replaced.contains(&binding.binding_id)),
        );
        validate_bindings(&mut effective)?;
        Ok(effective)
    }

    /// Compose and resolve the current Memory/Repository configuration exactly
    /// once. The caller supplies a trusted Workspace after the edge PEP has made
    /// its authorization decision; this method contains no authorization policy.
    pub fn resolve_inputs(
        workspace_id: &str,
        catalog: Option<&dyn ResourceConfigSource>,
        agent_defaults: &[InputBinding],
        session_attachments: &[SessionInputAttachment],
    ) -> Result<ResolvedSessionResources, SessionInputError> {
        let inputs = Self::compose(agent_defaults, session_attachments)?
            .into_iter()
            .map(|binding| {
                let source = match binding.target {
                    InputResourceId::File(file_id) => ResolvedInputSource::File { file_id },
                    InputResourceId::MemoryStore(memory_store_id) => {
                        let config = catalog
                            .ok_or_else(|| {
                                ResourceCatalogError::NotFound(memory_store_id.to_string())
                            })?
                            .resolve_memory_store(workspace_id, memory_store_id.as_str())?;
                        ResolvedInputSource::MemoryStore {
                            memory_store_id,
                            config,
                        }
                    }
                    InputResourceId::Repository(repository_id) => {
                        let config = catalog
                            .ok_or_else(|| {
                                ResourceCatalogError::NotFound(repository_id.to_string())
                            })?
                            .resolve_repository(workspace_id, repository_id.as_str())?;
                        ResolvedInputSource::Repository {
                            repository_id,
                            config,
                        }
                    }
                };
                let access = if matches!(&source, ResolvedInputSource::File { .. }) {
                    awaken_resource_contract::ResourceAccess::ReadOnly
                } else {
                    binding.access
                };
                Ok(ResolvedInput {
                    binding_id: binding.binding_id,
                    source,
                    mount_path: binding.mount_path,
                    access,
                    instructions: binding.instructions,
                })
            })
            .collect::<Result<Vec<_>, SessionInputError>>()?;
        Ok(ResolvedSessionResources {
            inputs,
            skills: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_resource_contract::{
        ClonePolicy, ConfigVersion, ExtractionPolicy, FileId, MemoryStoreId, RecallPolicy,
        RepositoryId, ResourceCatalogError, ResourceConfigSource, RetentionPolicy,
    };

    #[derive(Default)]
    struct Catalog {
        memory_resolves: AtomicUsize,
        repository_resolves: AtomicUsize,
    }

    impl ResourceConfigSource for Catalog {
        fn resolve_memory_store(
            &self,
            workspace_id: &str,
            id: &str,
        ) -> Result<MemoryStoreConfigVersion, ResourceCatalogError> {
            self.memory_resolves.fetch_add(1, Ordering::Relaxed);
            if workspace_id != "workspace-a" || id != "memory-1" {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            Ok(MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: ConfigVersion(4),
                recall_policy: RecallPolicy::default(),
                extraction_policy: ExtractionPolicy::default(),
                retention_policy: RetentionPolicy::default(),
            })
        }

        fn resolve_repository(
            &self,
            workspace_id: &str,
            id: &str,
        ) -> Result<RepositoryConfigVersion, ResourceCatalogError> {
            self.repository_resolves.fetch_add(1, Ordering::Relaxed);
            if workspace_id != "workspace-a" || id != "repo-1" {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            Ok(RepositoryConfigVersion {
                repository_id: id.into(),
                version: ConfigVersion(7),
                remote_url: "https://example.test/repo.git".into(),
                credential_binding: Some("credential-1".into()),
                initial_branch: Some("main".into()),
                clone_policy: ClonePolicy::default(),
            })
        }
    }

    fn binding(
        binding_id: &str,
        target: InputResourceId,
        mount_path: &str,
        access: awaken_resource_contract::ResourceAccess,
    ) -> InputBinding {
        InputBinding {
            binding_id: BindingId::from(binding_id),
            target,
            mount_path: mount_path.into(),
            access,
            instructions: None,
        }
    }

    #[test]
    fn typed_attachment_requires_explicit_replacement() {
        let defaults = vec![binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            "/mnt/context",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        )];
        let file = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "mnt/context",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        assert!(matches!(
            SessionInputResolver::compose(
                &defaults,
                &[SessionInputAttachment {
                    binding: file.clone(),
                    replaces: None,
                }]
            ),
            Err(SessionInputError::MountCollision(_))
        ));

        let effective = SessionInputResolver::resolve_inputs(
            "workspace-a",
            Some(&Catalog::default()),
            &defaults,
            &[SessionInputAttachment {
                binding: file,
                replaces: Some(BindingId::from("memory")),
            }],
        )
        .unwrap();
        assert_eq!(effective.inputs.len(), 1);
        assert_eq!(effective.inputs[0].mount_path, "/mnt/context");
        assert_eq!(
            effective.inputs[0].access,
            awaken_resource_contract::ResourceAccess::ReadOnly
        );
    }

    #[test]
    fn mutable_resources_resolve_once_without_content_pins_or_authorization() {
        let catalog = Catalog::default();
        let defaults = vec![
            binding(
                "memory",
                InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
                "/mnt/memory",
                awaken_resource_contract::ResourceAccess::ReadOnly,
            ),
            binding(
                "repo",
                InputResourceId::Repository(RepositoryId::from("repo-1")),
                "/workspace/repo",
                awaken_resource_contract::ResourceAccess::ReadWrite,
            ),
        ];

        let effective =
            SessionInputResolver::resolve_inputs("workspace-a", Some(&catalog), &defaults, &[])
                .unwrap();

        assert_eq!(catalog.memory_resolves.load(Ordering::Relaxed), 1);
        assert_eq!(catalog.repository_resolves.load(Ordering::Relaxed), 1);
        let wire = serde_json::to_string(&effective).unwrap();
        assert!(wire.contains("\"version\":4"));
        assert!(wire.contains("\"version\":7"));
        for forbidden in ["commit", "tree", "head", "principal", "policy_decision"] {
            assert!(
                !wire.contains(forbidden),
                "unexpected `{forbidden}` in {wire}"
            );
        }
    }

    #[test]
    fn unsafe_path_and_cross_workspace_fail_closed() {
        let unsafe_binding = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "/workspace/../secret",
            awaken_resource_contract::ResourceAccess::ReadOnly,
        );
        assert!(matches!(
            SessionInputResolver::compose(&[unsafe_binding], &[]),
            Err(SessionInputError::UnsafeMountPath(_))
        ));

        let memory_binding = binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            "/mnt/memory",
            awaken_resource_contract::ResourceAccess::ReadOnly,
        );
        assert!(matches!(
            SessionInputResolver::resolve_inputs(
                "workspace-b",
                Some(&Catalog::default()),
                &[memory_binding],
                &[]
            ),
            Err(SessionInputError::Catalog(ResourceCatalogError::NotFound(
                _
            )))
        ));
    }

    #[test]
    fn file_inputs_need_no_catalog_but_configured_resources_fail_closed_without_one() {
        let file_binding = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "/mnt/file",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        let resolved =
            SessionInputResolver::resolve_inputs("workspace-a", None, &[file_binding], &[])
                .unwrap();
        assert_eq!(resolved.inputs.len(), 1);
        assert_eq!(
            resolved.inputs[0].access,
            awaken_resource_contract::ResourceAccess::ReadOnly
        );

        for target in [
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            InputResourceId::Repository(RepositoryId::from("repo-1")),
        ] {
            let configured = binding(
                "configured",
                target,
                "/mnt/configured",
                awaken_resource_contract::ResourceAccess::ReadOnly,
            );
            assert!(matches!(
                SessionInputResolver::resolve_inputs("workspace-a", None, &[configured], &[]),
                Err(SessionInputError::Catalog(ResourceCatalogError::NotFound(
                    _
                )))
            ));
        }
    }

    fn resolved_file(binding_id: &str, file_id: &str, mount_path: &str) -> ResolvedInput {
        ResolvedInput {
            binding_id: BindingId::from(binding_id),
            source: ResolvedInputSource::File {
                file_id: FileId::from(file_id),
            },
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    #[test]
    fn effective_manifest_mutations_preserve_identity_and_invariants() {
        let initial = ResolvedSessionResources::default()
            .attach(resolved_file("input-a", "file-a", "mnt/a"))
            .unwrap();
        assert_eq!(initial.inputs[0].mount_path, "/mnt/a");

        let duplicate_path = initial.attach(resolved_file("input-b", "file-b", "/mnt/a"));
        assert!(matches!(
            duplicate_path,
            Err(SessionInputError::MountCollision(_))
        ));

        let mut replacement = resolved_file("input-a", "file-a", "/mnt/renamed");
        replacement.instructions = Some("read this first".into());
        let replaced = initial.replace(replacement).unwrap();
        assert_eq!(replaced.inputs[0].binding_id.as_str(), "input-a");
        assert_eq!(replaced.inputs[0].mount_path, "/mnt/renamed");

        let (detached, removed) = replaced.detach(&BindingId::from("input-a")).unwrap();
        assert!(detached.inputs.is_empty());
        assert_eq!(removed.binding_id.as_str(), "input-a");
        assert!(matches!(
            detached.detach(&BindingId::from("missing")),
            Err(SessionInputError::UnknownBinding(_))
        ));
    }
}
