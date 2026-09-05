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
#[path = "resource_prompt_tests.rs"]
mod resource_prompt_tests;

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
        // Walk the authored text once. A binding/path may itself contain text
        // resembling a variable; replacement values are data, never templates.
        let mut rendered = String::new();
        let mut remaining = instructions;
        while let Some(start) = remaining.find(RESOURCE_PROMPT_VARIABLE_PREFIX) {
            rendered.push_str(&remaining[..start]);
            let variable = &remaining[start..];
            let end = variable.find("}}").expect("template was validated") + 2;
            let value = match &variable[..end] {
                "{{awaken.workspace}}" => path_view.workspace(),
                "{{awaken.outputs}}" => {
                    path_view.render(awaken_provisioning_contract::WorkspaceLayout::OUTPUTS_ROOT)
                }
                "{{awaken.resource.binding}}" => self.binding.clone(),
                "{{awaken.resource.path}}" => path.clone(),
                "{{awaken.resource.directory}}" => directory.clone(),
                "{{awaken.resource.logical_path}}" => self.logical_path.clone(),
                "{{awaken.resource.access}}" => access.to_string(),
                _ => unreachable!("template was validated"),
            };
            rendered.push_str(&value);
            remaining = &variable[end..];
        }
        rendered.push_str(remaining);
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
