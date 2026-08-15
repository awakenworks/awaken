//! Session-owned file, mount, Skill-cache, and Artifact projection operations.
//!
//! The environment lifecycle and process launcher remain in the parent module;
//! this module owns the backend-neutral filesystem surface used by provisioning.

use super::SessionEnvironment;
use super::container_files;
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::DiscoveredSkillFile;

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

    pub(crate) async fn list_files(
        &self,
        subdir: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => Ok(sandbox.list_files(subdir)),
            Self::Namespace { sandbox, .. } => Ok(sandbox.list_files(subdir)),
            Self::Container { sandbox, .. } => {
                let root = if subdir.starts_with('/') {
                    subdir.to_string()
                } else {
                    container_files::read_root(subdir, sandbox.outputs_path())?
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

    /// Remove one path from the live resource projection. Every backend applies
    /// the same lexical jail and treats an absent path as an idempotent success.
    pub(crate) async fn remove_workspace_path(
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
