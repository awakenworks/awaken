//! Managed wire parsing and projection for typed Session inputs.

use super::*;
use crate::types::resource::{RepositoryCheckout, ResourceAccess, ResourceInput};

impl From<ResourceAccess> for awaken_resource_contract::ResourceAccess {
    fn from(value: ResourceAccess) -> Self {
        match value {
            ResourceAccess::ReadOnly => Self::ReadOnly,
            ResourceAccess::ReadWrite => Self::ReadWrite,
        }
    }
}

/// The repo name for a default mount path: the URL's last path segment, minus a
/// trailing `.git`. Falls back to `repo` when the URL has no usable segment.
fn repo_name(url: &str) -> String {
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .strip_suffix(".git")
        .or_else(|| Some(url.trim_end_matches('/').rsplit('/').next().unwrap_or("")))
        .unwrap_or("");
    if stem.is_empty() {
        "repo".to_string()
    } else {
        stem.to_string()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ParsedInputTarget {
    File(awaken_resource_contract::FileId),
    MemoryStore(awaken_resource_contract::MemoryStoreId),
    Repository {
        remote_url: String,
        initial_branch: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedSessionInput {
    pub target: ParsedInputTarget,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    pub instructions: Option<String>,
}

impl ResourceInput {
    pub(crate) fn to_parsed_input(&self) -> ParsedSessionInput {
        match self {
            ResourceInput::File {
                file_id,
                mount_path,
                instructions,
            } => ParsedSessionInput {
                target: ParsedInputTarget::File(file_id.clone().into()),
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: instructions.clone(),
            },
            ResourceInput::MemoryStore {
                memory_store_id,
                mount_path,
                instructions,
                access,
            } => ParsedSessionInput {
                target: ParsedInputTarget::MemoryStore(memory_store_id.clone().into()),
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| "/mnt/memory/store".into()),
                access: access.unwrap_or(ResourceAccess::ReadWrite).into(),
                instructions: instructions.clone(),
            },
            ResourceInput::GithubRepository {
                url,
                mount_path,
                instructions,
                checkout,
            } => ParsedSessionInput {
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| format!("/workspace/{}", repo_name(url))),
                target: ParsedInputTarget::Repository {
                    remote_url: url.clone(),
                    initial_branch: match checkout {
                        Some(RepositoryCheckout::Branch { name }) => Some(name.clone()),
                        Some(RepositoryCheckout::Commit { .. }) | None => None,
                    },
                },
                access: awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions: instructions.clone(),
            },
        }
    }
}

pub(crate) fn parse_session_input(v: &serde_json::Value) -> Option<ParsedSessionInput> {
    if v.get("authorization_token").is_some() {
        return None;
    }
    serde_json::from_value::<ResourceInput>(v.clone())
        .ok()
        .map(|resource| resource.to_parsed_input())
}

/// Lower a parsed Managed resource into the shared typed binding language. The
/// caller supplies the platform id for a compatibility Repository after it has
/// created that Session-scoped catalog definition.
pub(crate) fn input_binding(
    binding_id: String,
    input: &ParsedSessionInput,
    repository_id: Option<awaken_resource_contract::RepositoryId>,
) -> awaken_resource_contract::InputBinding {
    use awaken_resource_contract::{BindingId, InputBinding, InputResourceId};

    let target = match &input.target {
        ParsedInputTarget::File(file_id) => InputResourceId::File(file_id.clone()),
        ParsedInputTarget::MemoryStore(memory_store_id) => {
            InputResourceId::MemoryStore(memory_store_id.clone())
        }
        ParsedInputTarget::Repository { .. } => InputResourceId::Repository(
            repository_id.expect("Managed Repository lowering supplies a platform id"),
        ),
    };
    InputBinding {
        binding_id: BindingId::new(binding_id),
        target,
        mount_path: input.mount_path.clone(),
        access: input.access,
        instructions: input.instructions.clone(),
    }
}

/// Project one frozen resolved input back to the Managed Session wire. Internal
/// config versions remain an awaken implementation detail; credentials are never
/// echoed.
pub(crate) fn resolved_resource_dto(
    session_id: &str,
    input: &awaken_session_contract::ResolvedInput,
) -> serde_json::Value {
    use awaken_session_contract::ResolvedInputSource;
    use serde_json::json;

    let mut obj = serde_json::Map::new();
    obj.insert(
        "id".into(),
        json!(format!(
            "{session_id}:resource:{}",
            input.binding_id.as_str()
        )),
    );
    obj.insert("mount_path".into(), json!(input.mount_path));
    obj.insert("created_at".into(), json!(PROCESSED_AT));
    obj.insert("updated_at".into(), json!(PROCESSED_AT));
    match &input.source {
        ResolvedInputSource::File { file_id } => {
            obj.insert("type".into(), json!("file"));
            obj.insert("file_id".into(), json!(file_id.as_str()));
        }
        ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } => {
            obj.insert("type".into(), json!("memory_store"));
            obj.insert("memory_store_id".into(), json!(memory_store_id.as_str()));
            obj.insert(
                "access".into(),
                json!(match input.access {
                    awaken_resource_contract::ResourceAccess::ReadOnly => "read_only",
                    awaken_resource_contract::ResourceAccess::ReadWrite => "read_write",
                }),
            );
            if let Some(instructions) = &input.instructions {
                obj.insert("instructions".into(), json!(instructions));
            }
        }
        ResolvedInputSource::Repository { config, .. } => {
            obj.insert("type".into(), json!("github_repository"));
            obj.insert("url".into(), json!(config.remote_url));
            if let Some(branch) = &config.initial_branch {
                obj.insert(
                    "checkout".into(),
                    json!({ "type": "branch", "name": branch }),
                );
            }
        }
    }
    serde_json::Value::Object(obj)
}

/// Recover the typed binding identity from the Managed wire resource id.
/// Resource DTOs are projections only; callers use this value to locate the
/// authoritative input in `SessionResourceState`.
pub(crate) fn resource_binding_id(
    session_id: &str,
    resource_id: &str,
) -> Option<awaken_resource_contract::BindingId> {
    let binding_id = resource_id
        .strip_prefix(session_id)?
        .strip_prefix(":resource:")?;
    (!binding_id.is_empty()).then(|| awaken_resource_contract::BindingId::from(binding_id))
}
