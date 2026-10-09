// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Runtime Host owns the validated Agent-visible prompt projection of frozen ResolvedInput
// @formal reason: values. Session Environment selects the path and delivery view; rendering preserves
// @formal reason: binding and access without revealing provider-private roots or granting new authority.
// @formal reason: Adjacent resource_prompt_tests own executable decision-table evidence; this
// @formal reason: classification is not a direct formal-proof claim.
//! Agent-visible projection of managed Resource instructions and logical paths.
//!
//! Durable authority remains the frozen `ResolvedInput`; this module owns only
//! validated prompt-template rendering for the selected Session Environment.

use super::managed_file_mount_path;

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

/// The Agent's view of sandbox-logical paths; the Session environment
/// contract owns the value and its rendering.
pub(crate) use awaken_session_environment_contract::AgentPathView;

#[cfg(test)]
#[path = "resource_prompt_tests.rs"]
mod resource_prompt_tests;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResourcePromptKind {
    File(awaken_resource_contract::FileId),
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
        input: &awaken_session_resource_contract::ResolvedInput,
    ) -> Result<Self, awaken_session_types::RunError> {
        use awaken_session_resource_contract::ResolvedInputSource;

        let (kind, logical_path) = match &input.source {
            ResolvedInputSource::File { file_id } => (
                ResourcePromptKind::File(file_id.clone()),
                managed_file_mount_path(&input.mount_path),
            ),
            ResolvedInputSource::MemoryStore { .. } => (
                ResourcePromptKind::MemoryStore,
                awaken_provisioning_contract::memory_materialization_mount_path(&input.mount_path),
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
            .map_err(awaken_session_types::RunError::bad_request)?
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
        delivery: crate::frozen_inputs::ManagedContentDelivery,
        path_view: AgentPathView,
    ) -> String {
        use awaken_resource_contract::ResourceAccess;

        let access = match self.access {
            ResourceAccess::ReadOnly => "read-only",
            ResourceAccess::ReadWrite => "read/write",
        };
        let path = path_view.render(&self.logical_path);
        let base = match (&self.kind, delivery) {
            (ResourcePromptKind::File(file_id), _) => {
                // The frozen binding and the selected Environment jointly own
                // identity and usable path. A client mount request is not an
                // executable path, and equal filenames never select a version.
                format!(
                    "Read-only file reference (JSON data): {}",
                    serde_json::json!({
                        "file_id": file_id,
                        "binding_id": self.binding,
                        "path": path,
                    })
                )
            }
            (ResourcePromptKind::Repository, _) => format!(
                "A git repository is checked out at `{path}` ({access}); use git there to read, edit, commit, and export a reviewable patch. Remote publication requires an explicit operator workflow."
            ),
            (
                ResourcePromptKind::MemoryStore,
                crate::frozen_inputs::ManagedContentDelivery::ManagedFilesystem,
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
                crate::frozen_inputs::ManagedContentDelivery::SemanticTools,
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
