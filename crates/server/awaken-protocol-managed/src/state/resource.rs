//! Managed wire parsing and projection for typed Session inputs.

use super::*;
use crate::types::resource::{RepositoryCheckout, ResourceAccess, ResourceInput, SessionResource};

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
        credential_binding: Option<String>,
        initial_branch: Option<String>,
        initial_commit: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedSessionInput {
    pub target: ParsedInputTarget,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    pub instructions: Option<String>,
}

pub(crate) const MAX_SESSION_FILE_RESOURCES: usize = 500;

impl ResourceInput {
    pub(crate) fn to_parsed_input(&self) -> ParsedSessionInput {
        match self {
            ResourceInput::File {
                file_id,
                mount_path,
            } => ParsedSessionInput {
                target: ParsedInputTarget::File(file_id.clone().into()),
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
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
                credential_binding,
                mount_path,
                checkout,
            } => ParsedSessionInput {
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| format!("/workspace/{}", repo_name(url))),
                target: ParsedInputTarget::Repository {
                    remote_url: url.clone(),
                    credential_binding: credential_binding.clone(),
                    initial_branch: match checkout {
                        Some(RepositoryCheckout::Branch { name }) => Some(name.clone()),
                        Some(RepositoryCheckout::Commit { .. }) | None => None,
                    },
                    initial_commit: match checkout {
                        Some(RepositoryCheckout::Commit { sha }) => Some(sha.clone()),
                        Some(RepositoryCheckout::Branch { .. }) | None => None,
                    },
                },
                access: awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions: None,
            },
        }
    }
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
) -> SessionResource {
    use awaken_session_contract::ResolvedInputSource;
    let id = || format!("{session_id}:resource:{}", input.binding_id.as_str());
    match &input.source {
        ResolvedInputSource::File { file_id } => SessionResource::File {
            id: id(),
            created_at: PROCESSED_AT,
            file_id: file_id.as_str().to_string(),
            mount_path: input.mount_path.clone(),
            updated_at: PROCESSED_AT,
        },
        ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } => SessionResource::MemoryStore {
            memory_store_id: memory_store_id.as_str().to_string(),
            access: Some(match input.access {
                awaken_resource_contract::ResourceAccess::ReadOnly => ResourceAccess::ReadOnly,
                awaken_resource_contract::ResourceAccess::ReadWrite => ResourceAccess::ReadWrite,
            }),
            instructions: input.instructions.clone(),
            mount_path: Some(input.mount_path.clone()),
        },
        ResolvedInputSource::Repository { config, .. } => SessionResource::GithubRepository {
            id: id(),
            created_at: PROCESSED_AT,
            mount_path: input.mount_path.clone(),
            updated_at: PROCESSED_AT,
            url: config.remote_url.clone(),
            checkout: config
                .initial_branch
                .as_ref()
                .map(|name| RepositoryCheckout::Branch { name: name.clone() })
                .or_else(|| {
                    config
                        .initial_commit
                        .as_ref()
                        .map(|sha| RepositoryCheckout::Commit { sha: sha.clone() })
                }),
        },
    }
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
