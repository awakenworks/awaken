//! Session-owned file, mount, Skill-cache, and Artifact projection operations.
//!
//! The environment lifecycle and process launcher remain in the parent module;
//! this module owns the backend-neutral filesystem surface used by provisioning.

use super::SessionEnvironment;
use super::container_files;
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::DiscoveredSkillFile;

/// A typed read root prevents a Workspace-relative request from silently
/// acquiring the authority of a frozen sandbox-absolute mount path.
enum FileReadRoot {
    #[cfg(test)]
    Workspace {
        subdir: String,
    },
    FrozenMount {
        path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReadAuthority {
    #[cfg(any(test, kani))]
    Workspace,
    FrozenMount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileReadPathFacts {
    absolute: bool,
    lexically_safe: bool,
}

/// Pure authority kernel consumed by both typed constructors. Filesystem and
/// parser behavior remain adapter boundaries; the privilege relation itself is
/// small enough to exhaustively prove.
const fn file_read_authority_admitted(
    authority: FileReadAuthority,
    facts: FileReadPathFacts,
) -> bool {
    facts.lexically_safe
        && match authority {
            #[cfg(any(test, kani))]
            FileReadAuthority::Workspace => !facts.absolute,
            FileReadAuthority::FrozenMount => facts.absolute,
        }
}

impl FileReadRoot {
    #[cfg(test)]
    fn workspace(subdir: &str) -> Result<Self, pc::SandboxError> {
        // Validate with the same canonical rule the Container backend uses. The
        // local backends receive the relative representation only after this
        // shared admission decision succeeds.
        container_files::workspace_path(subdir)?;
        if !file_read_authority_admitted(
            FileReadAuthority::Workspace,
            FileReadPathFacts {
                absolute: subdir.starts_with('/'),
                lexically_safe: true,
            },
        ) {
            return Err(pc::SandboxError::new("unsafe Workspace file read root"));
        }
        Ok(Self::Workspace {
            subdir: subdir.to_string(),
        })
    }

    fn frozen_mount(path: &str) -> Result<Self, pc::SandboxError> {
        let path = container_files::sandbox_absolute_path(path)?;
        if !file_read_authority_admitted(
            FileReadAuthority::FrozenMount,
            FileReadPathFacts {
                absolute: path.starts_with('/'),
                lexically_safe: true,
            },
        ) {
            return Err(pc::SandboxError::new("unsafe frozen mount read root"));
        }
        Ok(Self::FrozenMount { path })
    }

    fn local_logical(&self) -> &str {
        match self {
            #[cfg(test)]
            Self::Workspace { subdir } => subdir,
            Self::FrozenMount { path } => path,
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn file_read_authority_never_changes_path_class_or_admits_unsafe_input() {
    let authority = if kani::any() {
        FileReadAuthority::Workspace
    } else {
        FileReadAuthority::FrozenMount
    };
    let facts = FileReadPathFacts {
        absolute: kani::any(),
        lexically_safe: kani::any(),
    };
    if file_read_authority_admitted(authority, facts) {
        assert!(facts.lexically_safe);
        match authority {
            FileReadAuthority::Workspace => assert!(!facts.absolute),
            FileReadAuthority::FrozenMount => assert!(facts.absolute),
        }
    }
    if !facts.lexically_safe {
        assert!(!file_read_authority_admitted(authority, facts));
    }
}

impl SessionEnvironment {
    /// Enumerate Agent-authored outputs through the provisioning contract's
    /// canonical Artifact port. Container backends expose the same contract over
    /// their output-file transport instead of creating a second host-side scanner.
    pub(crate) async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Namespace { sandbox, .. } => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Container { sandbox, .. } => sandbox
                .read_files(sandbox.outputs_path())
                .await
                .map(|files| {
                    files
                        .into_iter()
                        .map(|file| {
                            let id = awaken_resource_contract::content_id(&file.bytes);
                            pc::Artifact {
                                id: id.clone(),
                                path: file.path,
                                size_bytes: file.bytes.len() as u64,
                                content_hash: id,
                            }
                        })
                        .collect()
                }),
        }
    }

    pub(crate) async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
            Self::Namespace { sandbox, .. } => {
                pc::Sandbox::read_artifact(sandbox.as_ref(), id).await
            }
            Self::Container { sandbox, .. } => sandbox
                .read_files(sandbox.outputs_path())
                .await?
                .into_iter()
                .find_map(|file| {
                    (awaken_resource_contract::content_id(&file.bytes) == id).then_some(file.bytes)
                })
                .ok_or_else(|| pc::SandboxError::new(format!("artifact `{id}` not found"))),
        }
    }

    /// Read a Workspace-relative directory. Absolute paths are rejected at the
    /// type-construction edge rather than interpreted differently per backend.
    #[cfg(test)]
    pub(crate) async fn list_workspace_files(
        &self,
        subdir: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        self.list_files_at(FileReadRoot::workspace(subdir)?).await
    }

    /// Read an exact sandbox-absolute mount root obtained from a validated,
    /// frozen Session resource projection.
    pub(crate) async fn list_frozen_mount_files(
        &self,
        mount_path: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        self.list_files_at(FileReadRoot::frozen_mount(mount_path)?)
            .await
    }

    async fn list_files_at(
        &self,
        root: FileReadRoot,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => Ok(sandbox.list_files(root.local_logical())),
            Self::Namespace { sandbox, .. } => Ok(sandbox.list_files(root.local_logical())),
            Self::Container { sandbox, .. } => {
                let root = match root {
                    #[cfg(test)]
                    FileReadRoot::Workspace { subdir } => {
                        container_files::read_root(&subdir, sandbox.outputs_path())?
                    }
                    FileReadRoot::FrozenMount { path } => path,
                };
                sandbox.read_files(&root).await.map(|files| {
                    files
                        .into_iter()
                        .map(|file| (file.path, file.bytes))
                        .collect()
                })
            }
        }
    }

    pub(crate) fn needs_recovered_memory_reconciliation(&self) -> bool {
        matches!(self, Self::Container { sandbox, .. } if sandbox.is_recovered())
    }

    pub(crate) fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        match self {
            Self::Workdir(sandbox) => sandbox.scan_skill_dir(subdir),
            Self::Namespace { sandbox, .. } => sandbox.scan_skill_dir(subdir),
            Self::Container { skills, .. } => skills.get(subdir),
        }
    }

    pub(crate) fn register_skill_dir(&self, subdir: &str) {
        if let Self::Container { skills, .. } = self {
            skills.register(subdir);
        }
    }

    pub(crate) async fn refresh_skills(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Container {
                sandbox, skills, ..
            } => skills.refresh(sandbox.as_ref()).await,
            Self::Workdir(_) | Self::Namespace { .. } => Ok(()),
        }
    }

    pub(crate) async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Namespace { sandbox, .. } => sandbox.materialize_inline(logical, contents),
            Self::Container { sandbox, .. } => {
                container_files::write(sandbox.as_ref(), logical, contents).await
            }
        }
    }

    pub(crate) async fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_read_only_tree(subdir, files),
            Self::Namespace { sandbox, .. } => sandbox.materialize_read_only_tree(subdir, files),
            Self::Container { sandbox, .. } => {
                container_files::materialize_read_only_tree(sandbox.as_ref(), subdir, files).await
            }
        }
    }

    /// Attach a governed mount through the backend's canonical live-injection
    /// port. Unsupported tiers fail closed instead of receiving a writable copy.
    pub(crate) async fn attach_mount(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        self.sandbox().attach(requirement).await
    }

    /// Rebuild the dynamic bind layout of an adopted Namespace from the frozen
    /// Session manifest. Workdir paths survive directly and container runtimes
    /// retain their own mount namespace across process ownership changes.
    pub(crate) async fn reconcile_adopted_mounts(
        &self,
        requirements: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        if let Self::Namespace { sandbox, .. } = self {
            for requirement in requirements {
                pc::Sandbox::attach(sandbox.as_ref(), requirement.clone()).await?;
            }
        }
        Ok(())
    }

    /// Write an ordinary runtime-owned workspace file. This is intentionally
    /// distinct from [`Self::attach_mount`], which carries access guarantees.
    pub(crate) async fn write_workspace_file(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Namespace { sandbox, .. } => sandbox.materialize_inline(logical, contents),
            Self::Container { sandbox, .. } => {
                let path = container_files::logical_path(logical)?;
                container_files::write(sandbox.as_ref(), &path, contents).await
            }
        }
    }

    /// Remove one path from the live resource projection. The Container adapter
    /// owns the distinction between its governed live-input root and ordinary
    /// Workspace files; callers no longer mislabel every projection as a
    /// Workspace path. Every backend applies its lexical jail and treats an
    /// absent path as an idempotent success.
    pub(crate) async fn remove_projection_path(
        &self,
        logical: &str,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.remove_inline(logical),
            Self::Namespace { sandbox, .. } => sandbox.remove_mount(logical),
            Self::Container { sandbox, .. } => {
                if awaken_sandbox_container::live_input_relative_path(logical).is_some() {
                    sandbox.remove_live_input_path(logical).await
                } else {
                    container_files::remove(sandbox.as_ref(), logical).await
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileReadRoot;

    #[test]
    fn read_root_authority_is_explicit_and_fail_closed() {
        assert!(FileReadRoot::workspace("outputs/nested").is_ok());
        assert!(FileReadRoot::workspace("/mnt/session/input").is_err());
        assert!(FileReadRoot::workspace("outputs/../secret").is_err());

        assert!(FileReadRoot::frozen_mount("/mnt/session/input").is_ok());
        assert!(FileReadRoot::frozen_mount("mnt/session/input").is_err());
        assert!(FileReadRoot::frozen_mount("/mnt/../secret").is_err());
    }
}
