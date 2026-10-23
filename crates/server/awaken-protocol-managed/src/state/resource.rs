// @formal surface obligation=session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal surface obligation=session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal reason: Managed resource input projection: wire resources to Session input requests (with the
// @formal reason: Managed default mount paths) and frozen inputs back to wire resources. Admission,
// @formal reason: derived paths and replacement rules live in the Session application.
//! Managed wire parsing and projection for typed Session inputs.

use crate::types::resource::{RepositoryCheckout, ResourceAccess, ResourceInput, SessionResource};
use awaken_session_application::{SessionInputRequest, SessionInputTarget};

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

impl ResourceInput {
    /// Lower one wire resource into the protocol-neutral Session input
    /// request, filling the Managed default mount paths.
    pub(crate) fn to_input_request(&self) -> SessionInputRequest {
        match self {
            ResourceInput::File {
                file_id,
                mount_path,
            } => SessionInputRequest {
                target: SessionInputTarget::File(file_id.clone().into()),
                mount_path: mount_path
                    .clone()
                    .unwrap_or_else(|| format!("/mnt/session/uploads/{file_id}")),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
                derived_memory_mount: false,
            },
            ResourceInput::MemoryStore {
                memory_store_id,
                instructions,
                access,
            } => SessionInputRequest::derived_memory_store(
                memory_store_id.clone().into(),
                access.unwrap_or(ResourceAccess::ReadWrite).into(),
                instructions.clone(),
            ),
            ResourceInput::GithubRepository {
                url,
                authorization_token,
                mount_path,
                checkout,
            } => SessionInputRequest {
                mount_path: mount_path.clone().unwrap_or_else(|| {
                    awaken_provisioning_contract::WorkspaceLayout::child(&repo_name(url))
                }),
                target: SessionInputTarget::Repository {
                    remote_url: url.clone(),
                    credential_material: authorization_token
                        .clone()
                        .map(|token| token.into_credential_material()),
                    initial_branch: match checkout {
                        Some(RepositoryCheckout::Branch { name }) => Some(name.clone()),
                        Some(RepositoryCheckout::Commit { .. }) | None => None,
                    },
                    initial_commit: match checkout {
                        Some(RepositoryCheckout::Commit { sha }) => Some(sha.clone()),
                        Some(RepositoryCheckout::Branch { .. }) | None => None,
                    },
                },
                // An exact commit is an immutable historical input. Giving it
                // ReadWrite authority would promise a terminal publication even
                // though detached HEAD has no unambiguous remote branch. Branch
                // and default checkouts retain the ordinary write-back contract.
                access: if matches!(checkout, Some(RepositoryCheckout::Commit { .. })) {
                    awaken_resource_contract::ResourceAccess::ReadOnly
                } else {
                    awaken_resource_contract::ResourceAccess::ReadWrite
                },
                instructions: None,
                derived_memory_mount: false,
            },
        }
    }
}

/// Project one frozen resolved input back to the Managed Session wire. Internal
/// config versions remain an awaken implementation detail; credentials are never
/// echoed.
pub(crate) fn resolved_resource_dto(
    session_id: &str,
    input: &crate::session_resource::ResolvedInput,
) -> SessionResource {
    use awaken_session_configuration_contract::resource::ResolvedInputSource;
    let id = || format!("{session_id}:resource:{}", input.binding_id.as_str());
    match &input.source {
        ResolvedInputSource::File { file_id } => SessionResource::File {
            id: id(),
            created_at: super::constants::persisted_time(input.created_at_unix_ms),
            file_id: file_id.as_str().to_string(),
            mount_path: input.mount_path.clone(),
            updated_at: super::constants::persisted_time(input.updated_at_unix_ms),
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
            created_at: super::constants::persisted_time(input.created_at_unix_ms),
            mount_path: input.mount_path.clone(),
            updated_at: super::constants::persisted_time(input.updated_at_unix_ms),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(checkout: Option<RepositoryCheckout>) -> ResourceInput {
        ResourceInput::GithubRepository {
            url: "https://example.invalid/repository.git".into(),
            authorization_token: None,
            mount_path: None,
            checkout,
        }
    }

    #[test]
    fn exact_commit_is_read_only_while_branch_and_default_remain_writable() {
        use awaken_resource_contract::ResourceAccess::{ReadOnly, ReadWrite};

        assert_eq!(
            repository(Some(RepositoryCheckout::Commit { sha: "abc".into() }))
                .to_input_request()
                .access,
            ReadOnly,
        );
        assert_eq!(
            repository(Some(RepositoryCheckout::Branch {
                name: "main".into()
            }))
            .to_input_request()
            .access,
            ReadWrite,
        );
        assert_eq!(repository(None).to_input_request().access, ReadWrite);
    }
}
