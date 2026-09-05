//! Agent-visible projection of managed Resource instructions and logical paths.
//!
//! Durable authority remains the frozen `ResolvedInput`; this module owns only
//! validated prompt-template rendering for the selected Session Environment.

use super::{managed_file_mount_path, managed_resource_mount_path};

const RESOURCE_PROMPT_VARIABLE_PREFIX: &str = "{{awaken.";
const RESOURCE_PROMPT_VARIABLES: &[&str] = &[
    "{{awaken.workspace}}",
    "{{awaken.outputs}}",
    "{{awaken.resource.binding}}",
    "{{awaken.resource.path}}",
    "{{awaken.resource.directory}}",
    "{{awaken.resource.logical_path}}",
    "{{awaken.resource.access}}",
];

/// Provider-relative view of a sandbox-logical path presented to an Agent.
/// `SandboxAbsolute` is valid inside Namespace/Container. `WorkdirRelative`
/// deliberately strips the provider-private host root, so a trusted local ACP
/// process receives a usable path without leaking the operator's absolute path
/// into a potentially remote model request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum AgentPathView {
    #[default]
    SandboxAbsolute,
    WorkdirRelative {
        logical_cwd: String,
    },
}

impl AgentPathView {
    pub(crate) fn sandbox_absolute() -> Self {
        Self::SandboxAbsolute
    }

    pub(crate) fn workdir_relative(logical_cwd: impl Into<String>) -> Self {
        Self::WorkdirRelative {
            logical_cwd: logical_cwd.into(),
        }
    }

    fn render(&self, logical: &str) -> String {
        self.render_with_separator(logical, std::path::MAIN_SEPARATOR)
    }

    fn render_with_separator(&self, logical: &str, separator: char) -> String {
        match self {
            Self::SandboxAbsolute => logical.to_string(),
            Self::WorkdirRelative { logical_cwd } => {
                relative_logical_path(logical_cwd, logical, separator)
            }
        }
    }

    fn workspace(&self) -> String {
        match self {
            Self::SandboxAbsolute => awaken_provisioning_contract::WorkspaceLayout::ROOT.into(),
            Self::WorkdirRelative { logical_cwd } => relative_logical_path(
                logical_cwd,
                awaken_provisioning_contract::WorkspaceLayout::ROOT,
                std::path::MAIN_SEPARATOR,
            ),
        }
    }
}

/// Compute a portable relative path between two already-validated logical
/// sandbox paths. This never observes a provider-private host prefix.
fn relative_logical_path(from: &str, to: &str, separator: char) -> String {
    let from = from
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let to = to
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = std::iter::repeat_n("..", from.len() - common)
        .chain(to[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/");
    if result.is_empty() {
        result.push('.');
    }
    if separator != '/' {
        result = result.replace('/', &separator.to_string());
    }
    result
}

#[cfg(test)]
mod resource_prompt_tests {
    use super::*;
    use awaken_resource_contract::{BindingId, FileId, ResourceAccess};
    use awaken_session_contract::{ResolvedInput, ResolvedInputSource};

    fn file(instructions: Option<&str>) -> ResolvedInput {
        ResolvedInput {
            binding_id: BindingId::from("input-file"),
            source: ResolvedInputSource::File {
                file_id: FileId::from("file-1"),
            },
            mount_path: "brief.md".into(),
            access: ResourceAccess::ReadOnly,
            instructions: instructions.map(str::to_owned),
        }
    }

    #[test]
    fn resource_prompt_variable_decision_table_is_closed_and_provider_relative() {
        // Cause/effect graph:
        // C1 path view={sandbox absolute, Workdir relative};
        // C2 separator={POSIX, Windows}; C3 template={known, unknown, malformed}.
        // Effects: E1 known variables render from the same logical mount;
        // E2 Workdir output contains no provider-private absolute prefix;
        // E3 unknown/malformed Awaken variables fail before realization.
        // Constraints: Windows separator affects only Workdir-relative paths;
        // sandbox paths are provider-neutral POSIX paths. Decision rules:
        // R1 absolute+known=>E1; R2 relative+POSIX+known=>E1+E2;
        // R3 relative+Windows+known=>E1+E2; R4/R5 invalid=>E3.
        let projection = ResourcePromptProjection::try_from_input(&file(Some(
            "binding={{awaken.resource.binding}} path={{awaken.resource.path}} dir={{awaken.resource.directory}} workspace={{awaken.workspace}} outputs={{awaken.outputs}} logical={{awaken.resource.logical_path}} access={{awaken.resource.access}}",
        )))
        .expect("known template");
        let absolute = projection.render(
            crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::sandbox_absolute(),
        );
        assert!(
            absolute.contains("path=/mnt/session/uploads/brief.md"),
            "R1"
        );
        assert!(absolute.contains("workspace=/workspace"), "R1");
        assert!(absolute.contains("outputs=/mnt/session/outputs"), "R1");

        let posix_path = AgentPathView::workdir_relative("/workspace")
            .render_with_separator("/mnt/session/uploads/brief.md", '/');
        let windows_path = AgentPathView::workdir_relative("/workspace")
            .render_with_separator("/mnt/session/uploads/brief.md", '\\');
        assert_eq!(posix_path, "../mnt/session/uploads/brief.md", "R2");
        assert_eq!(windows_path, "..\\mnt\\session\\uploads\\brief.md", "R3");
        assert!(!posix_path.starts_with('/'), "R2 E2");
        assert!(!windows_path.contains(":"), "R3 E2");
        let relative = projection.render(
            crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::workdir_relative("/workspace"),
        );
        assert!(relative.contains("workspace=."), "R2 working directory");

        for (rule, instructions) in [
            ("R4", "{{awaken.resource.missing}}"),
            ("R5", "{{awaken.resource.path"),
        ] {
            assert!(
                ResourcePromptProjection::try_from_input(&file(Some(instructions))).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn workdir_prompt_paths_follow_the_effective_nested_cwd_cross_platform() {
        // Cause/effect graph: C1 cwd={workspace, workspace/repo/src};
        // C2 target={workspace, repository, File, Memory, outputs};
        // C3 separator={POSIX, Windows}. E1 every rendered path resolves from
        // the same logical cwd; E2 common prefixes are removed; E3 no host
        // drive/root enters model-visible text. Constraints: the authored cwd
        // is validated upstream and all targets are WorkspaceLayout-owned.
        // Decision rules: P1 workspace=>workspace-relative; P2 nested+ancestor=>`..`;
        // P3 nested+sibling-root=>common-prefix traversal; P4 Windows=>the same
        // components with `\\`; all rules require E3.
        let nested = AgentPathView::workdir_relative("/workspace/repo/src");
        for (target, posix, windows) in [
            ("/workspace", "../..", "..\\.."),
            ("/workspace/repo", "..", ".."),
            ("/workspace/repo/README.md", "../README.md", "..\\README.md"),
            (
                "/mnt/session/uploads/brief.md",
                "../../../mnt/session/uploads/brief.md",
                "..\\..\\..\\mnt\\session\\uploads\\brief.md",
            ),
            (
                "/mnt/memory/team",
                "../../../mnt/memory/team",
                "..\\..\\..\\mnt\\memory\\team",
            ),
            (
                "/mnt/session/outputs",
                "../../../mnt/session/outputs",
                "..\\..\\..\\mnt\\session\\outputs",
            ),
        ] {
            assert_eq!(nested.render_with_separator(target, '/'), posix, "P2/P3");
            let rendered = nested.render_with_separator(target, '\\');
            assert_eq!(rendered, windows, "P4");
            assert!(!rendered.contains(':') && !rendered.starts_with('\\'), "E3");
        }
        assert_eq!(nested.workspace(), "../..", "P2 workspace variable");
        let workspace = AgentPathView::workdir_relative("/workspace");
        assert_eq!(workspace.workspace(), ".", "P1 workspace variable");
        assert_eq!(
            workspace.render_with_separator("/mnt/memory/team", '/'),
            "../mnt/memory/team",
            "P1 sibling mount"
        );
    }

    #[test]
    fn memory_path_variables_require_filesystem_delivery() {
        // Cause/effect graph: C1 Memory instructions use a physical-path
        // variable; C2 uses binding/logical facts only. E1 C1 marks the prompt
        // as requiring filesystem delivery; E2 C2 remains valid for semantic
        // tools. The Session delivery decision owns the eventual rejection.
        // Decision table: M1 C1=>E1; M2 C2=>E2.
        let memory = |instructions: &str| ResolvedInput {
            binding_id: BindingId::from("memory"),
            source: ResolvedInputSource::MemoryStore {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                config: awaken_resource_contract::MemoryStoreConfigVersion {
                    memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                    version: awaken_resource_contract::ConfigVersion(1),
                    retention_policy: Default::default(),
                },
            },
            mount_path: "/mnt/memory".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some(instructions.into()),
        };
        assert!(
            ResourcePromptProjection::try_from_input(&memory("read {{awaken.resource.path}}"))
                .unwrap()
                .requires_filesystem_delivery(),
            "M1"
        );
        assert!(
            !ResourcePromptProjection::try_from_input(&memory(
                "use {{awaken.resource.binding}} at logical {{awaken.resource.logical_path}}"
            ))
            .unwrap()
            .requires_filesystem_delivery(),
            "M2"
        );
        for variable in [
            "{{awaken.workspace}}",
            "{{awaken.outputs}}",
            "{{awaken.resource.directory}}",
        ] {
            assert!(
                ResourcePromptProjection::try_from_input(&memory(variable))
                    .unwrap()
                    .requires_filesystem_delivery(),
                "M1 physical variable {variable}"
            );
        }
    }

    #[test]
    fn every_managed_resource_kind_uses_the_same_prompt_projection() {
        // Causes: C1 kind={file,memory,repository}; C2 Memory delivery={files,
        // semantic}; C3 access={read-only,read-write}. Effects: E1 file and repo
        // expose their exact provider-relative path; E2 filesystem Memory names
        // its mount; E3 semantic Memory names only binding-scoped tools; E4
        // read/write wording matches frozen access. The cases below cover every
        // renderer arm and both access effects; authored variable parsing is
        // owned by the preceding decision table.
        let memory = |access| ResolvedInput {
            binding_id: BindingId::from("memory"),
            source: ResolvedInputSource::MemoryStore {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                config: awaken_resource_contract::MemoryStoreConfigVersion {
                    memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    retention_policy: Default::default(),
                },
            },
            mount_path: "/mnt/memory".into(),
            access,
            instructions: None,
        };
        let repository = ResolvedInput {
            binding_id: BindingId::from("repo"),
            source: ResolvedInputSource::Repository {
                repository_id: awaken_resource_contract::RepositoryId::from("repository"),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: awaken_resource_contract::RepositoryId::from("repository"),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: "https://example.invalid/repo.git".into(),
                    credential_binding: None,
                    initial_branch: None,
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential: None,
            },
            mount_path: "/workspace/repo".into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        };
        let file = ResourcePromptProjection::try_from_input(&file(None))
            .unwrap()
            .render(
                crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
                AgentPathView::workdir_relative("/workspace"),
            );
        assert!(file.contains("mnt/session/uploads/brief.md"), "E1 file");
        let memory_files =
            ResourcePromptProjection::try_from_input(&memory(ResourceAccess::ReadOnly))
                .unwrap()
                .render(
                    crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
                    AgentPathView::sandbox_absolute(),
                );
        assert!(memory_files.contains("/mnt/memory"), "E2");
        assert!(memory_files.contains("read-only"), "E4");
        let memory_tools =
            ResourcePromptProjection::try_from_input(&memory(ResourceAccess::ReadWrite))
                .unwrap()
                .render(
                    crate::session_slot::ManagedContentDelivery::SemanticTools,
                    AgentPathView::sandbox_absolute(),
                );
        assert!(!memory_tools.contains("/mnt/memory"), "E3");
        assert!(memory_tools.contains("binding: \"memory\""), "E3");
        assert!(memory_tools.contains("read/write"), "E4");
        let repository = ResourcePromptProjection::try_from_input(&repository)
            .unwrap()
            .render(
                crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
                AgentPathView::sandbox_absolute(),
            );
        assert!(repository.contains("/workspace/repo"), "E1 repository");
        assert!(repository.contains("commit"), "E4");
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourcePromptKind {
    File,
    MemoryStore,
    Repository,
}

/// One process-local, rebuildable prompt projection for every managed input.
/// Durable authority remains `ResolvedInput`; this value only parses authored
/// instructions once and renders the delivery/path view selected by the one
/// Session Environment.
#[derive(Clone, Debug)]
pub(crate) struct ResourcePromptProjection {
    kind: ResourcePromptKind,
    binding: String,
    logical_path: String,
    access: awaken_resource_contract::ResourceAccess,
    instructions: Option<String>,
    instructions_require_filesystem: bool,
}

impl ResourcePromptProjection {
    pub(crate) fn try_from_input(
        input: &awaken_session_contract::ResolvedInput,
    ) -> Result<Self, awaken_session_contract::RunError> {
        use awaken_session_contract::ResolvedInputSource;

        let (kind, logical_path) = match &input.source {
            ResolvedInputSource::File { .. } => (
                ResourcePromptKind::File,
                managed_file_mount_path(&input.mount_path),
            ),
            ResolvedInputSource::MemoryStore { .. } => (
                ResourcePromptKind::MemoryStore,
                managed_resource_mount_path(&input.mount_path),
            ),
            ResolvedInputSource::Repository { .. } => {
                (ResourcePromptKind::Repository, input.mount_path.clone())
            }
        };
        let instructions_require_filesystem = input
            .instructions
            .as_deref()
            .map(validate_resource_prompt_template)
            .transpose()
            .map_err(awaken_session_contract::RunError::bad_request)?
            .unwrap_or(false);
        Ok(Self {
            kind,
            binding: input.binding_id.to_string(),
            logical_path,
            access: input.access,
            instructions: input.instructions.clone(),
            instructions_require_filesystem,
        })
    }

    pub(crate) fn requires_filesystem_delivery(&self) -> bool {
        self.kind == ResourcePromptKind::MemoryStore && self.instructions_require_filesystem
    }

    pub(crate) fn render(
        &self,
        delivery: crate::session_slot::ManagedContentDelivery,
        path_view: AgentPathView,
    ) -> String {
        use awaken_resource_contract::ResourceAccess;

        let access = match self.access {
            ResourceAccess::ReadOnly => "read-only",
            ResourceAccess::ReadWrite => "read/write",
        };
        let path = path_view.render(&self.logical_path);
        let base = match (self.kind, delivery) {
            (ResourcePromptKind::File, _) => {
                format!("A file is mounted read-only at `{path}`.")
            }
            (ResourcePromptKind::Repository, _) => format!(
                "A git repository is checked out at `{path}` ({access}); use git there to read, edit, commit, and export a reviewable patch. Remote publication requires an explicit operator workflow."
            ),
            (
                ResourcePromptKind::MemoryStore,
                crate::session_slot::ManagedContentDelivery::ManagedFilesystem,
            ) => {
                let usage = match self.access {
                    ResourceAccess::ReadOnly => "Use standard file tools to read it.",
                    ResourceAccess::ReadWrite => "Use standard file tools to read and maintain it.",
                };
                format!(
                    "Persistent memory store `{}` is mounted {access} at `{path}`. {usage}",
                    self.binding
                )
            }
            (
                ResourcePromptKind::MemoryStore,
                crate::session_slot::ManagedContentDelivery::SemanticTools,
            ) => {
                let usage = match self.access {
                    ResourceAccess::ReadOnly => {
                        "Use `list_memories` and `read_memory`; mutation tools will reject this binding."
                    }
                    ResourceAccess::ReadWrite => {
                        "Use `list_memories` and `read_memory` before a conditional write or delete."
                    }
                };
                format!(
                    "Persistent memory store `{}` is available {access} through the memory tools. Pass `binding: \"{}\"` on every memory operation. {usage}",
                    self.binding, self.binding
                )
            }
        };
        let Some(instructions) = self
            .instructions
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            return base;
        };
        let directory = resource_parent(&path);
        let rendered = instructions
            .replace("{{awaken.workspace}}", &path_view.workspace())
            .replace(
                "{{awaken.outputs}}",
                &path_view.render(awaken_provisioning_contract::WorkspaceLayout::OUTPUTS_ROOT),
            )
            .replace("{{awaken.resource.binding}}", &self.binding)
            .replace("{{awaken.resource.path}}", &path)
            .replace("{{awaken.resource.directory}}", &directory)
            .replace("{{awaken.resource.logical_path}}", &self.logical_path)
            .replace("{{awaken.resource.access}}", access);
        format!("{base}\n{rendered}")
    }
}

fn resource_parent(path: &str) -> String {
    let separators = ['/', '\\'];
    path.rfind(separators)
        .map(|index| {
            if index == 0 {
                path[..=index].to_string()
            } else {
                path[..index].to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

/// Validate only the namespaced Awaken grammar. Ordinary braces in authored
/// prose/code remain literal for backward compatibility; an attempted Awaken
/// variable must be closed and belong to the finite allowlist.
fn validate_resource_prompt_template(template: &str) -> Result<bool, String> {
    let mut remainder = template;
    let mut requires_filesystem = false;
    while let Some(start) = remainder.find(RESOURCE_PROMPT_VARIABLE_PREFIX) {
        let variable = &remainder[start..];
        let Some(end) = variable.find("}}") else {
            return Err("resource instructions contain an unterminated Awaken variable".into());
        };
        let variable = &variable[..end + 2];
        if !RESOURCE_PROMPT_VARIABLES.contains(&variable) {
            return Err(format!("unsupported resource prompt variable `{variable}`"));
        }
        requires_filesystem |= matches!(
            variable,
            "{{awaken.workspace}}"
                | "{{awaken.outputs}}"
                | "{{awaken.resource.path}}"
                | "{{awaken.resource.directory}}"
        );
        remainder = &remainder[start + end + 2..];
    }
    Ok(requires_filesystem)
}
