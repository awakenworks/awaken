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
        authorization_token: Option<crate::types::resource::RepositoryAuthorizationToken>,
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
    /// Only Managed MemoryStore inputs have a server-derived mount path. Keep
    /// that fact until the Resource Registry definition is available so the
    /// Session freezes a display-name-derived path rather than a shared literal.
    pub implicit_memory_mount: bool,
}

pub(crate) const MAX_SESSION_FILE_RESOURCES: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) enum InputProjectionClass {
    File,
    MemoryStore,
    Repository,
}

pub(super) fn parsed_projection_class(target: &ParsedInputTarget) -> InputProjectionClass {
    match target {
        ParsedInputTarget::File(_) => InputProjectionClass::File,
        ParsedInputTarget::MemoryStore(_) => InputProjectionClass::MemoryStore,
        ParsedInputTarget::Repository { .. } => InputProjectionClass::Repository,
    }
}

fn binding_projection_class(
    target: &awaken_resource_contract::InputResourceId,
) -> InputProjectionClass {
    match target {
        awaken_resource_contract::InputResourceId::File(_) => InputProjectionClass::File,
        awaken_resource_contract::InputResourceId::MemoryStore(_) => {
            InputProjectionClass::MemoryStore
        }
        awaken_resource_contract::InputResourceId::Repository(_) => {
            InputProjectionClass::Repository
        }
    }
}

pub(super) fn resolved_projection_class(
    source: &awaken_session_contract::ResolvedInputSource,
) -> InputProjectionClass {
    match source {
        awaken_session_contract::ResolvedInputSource::File { .. } => InputProjectionClass::File,
        awaken_session_contract::ResolvedInputSource::MemoryStore { .. } => {
            InputProjectionClass::MemoryStore
        }
        awaken_session_contract::ResolvedInputSource::Repository { .. } => {
            InputProjectionClass::Repository
        }
    }
}

pub(super) fn preflight_repository_mount_paths(
    resources: &[ParsedSessionInput],
) -> Result<(), StateError> {
    let paths = resources
        .iter()
        .filter(|resource| matches!(&resource.target, ParsedInputTarget::Repository { .. }))
        .map(|resource| resource.mount_path.as_str())
        .collect::<Vec<_>>();
    awaken_provisioning_contract::validate_repository_mount_paths(&paths, &[])
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))
}

fn plan_session_input_attachments(
    session_id: &str,
    resources: &[ParsedSessionInput],
    agent_defaults: &[awaken_resource_contract::InputBinding],
) -> Vec<awaken_session_contract::SessionInputAttachment> {
    resources
        .iter()
        .enumerate()
        .map(|(index, resource)| {
            let repository_id = matches!(&resource.target, ParsedInputTarget::Repository { .. })
                .then(|| awaken_resource_contract::RepositoryId::from("preflight"));
            let mut binding = input_binding(
                format!("session:{session_id}:input:{index}"),
                resource,
                repository_id,
            );
            let normalized = binding.mount_path.trim_start_matches('/');
            let replaces = agent_defaults
                .iter()
                .find(|default| {
                    default.mount_path.trim_start_matches('/') == normalized
                        && binding_projection_class(&default.target)
                            == parsed_projection_class(&resource.target)
                })
                .map(|default| default.binding_id.clone());
            if let Some(replaced) = &replaces {
                binding.binding_id.clone_from(replaced);
            }
            awaken_session_contract::SessionInputAttachment { binding, replaces }
        })
        .collect()
}

pub(crate) struct PreparedSessionInputs {
    resources: Vec<ParsedSessionInput>,
    attachments: Vec<awaken_session_contract::SessionInputAttachment>,
    effective_bindings: Vec<awaken_resource_contract::InputBinding>,
}

impl PreparedSessionInputs {
    pub(crate) fn effective_bindings(&self) -> &[awaken_resource_contract::InputBinding] {
        &self.effective_bindings
    }
}

impl ManagedState {
    /// Resolve server-derived logical paths and compile the one effective typed
    /// binding plan before entering Skill/File/Vault/Repository effects. The
    /// returned value is consumed unchanged by effect lowering, so replacement
    /// semantics and layout validation cannot drift between two preflights.
    pub(crate) fn prepare_session_input_attachments(
        &self,
        session_id: &str,
        owner_scope: &str,
        resources: &[ParsedSessionInput],
        agent_defaults: &[awaken_resource_contract::InputBinding],
    ) -> Result<PreparedSessionInputs, StateError> {
        let mut used_mounts = agent_defaults
            .iter()
            .map(|binding| binding.mount_path.trim_start_matches('/').to_string())
            .chain(
                resources
                    .iter()
                    .filter(|resource| !resource.implicit_memory_mount)
                    .map(|resource| resource.mount_path.trim_start_matches('/').to_string()),
            )
            .collect::<std::collections::BTreeSet<_>>();
        // Resolve every definition-owned mount before entering either durable
        // Repository participant. A later malformed Memory/File input cannot
        // strand an earlier Registry/Vault participant.
        let mut normalized_resources = Vec::with_capacity(resources.len());
        for resource in resources {
            let mut resource = resource.clone();
            if resource.implicit_memory_mount {
                let ParsedInputTarget::MemoryStore(memory_store_id) = &resource.target else {
                    unreachable!("only MemoryStore inputs derive a Managed mount path")
                };
                let definition = self
                    .application
                    .session_memory_store(owner_scope, memory_store_id.as_str())
                    .map_err(StateError::Run)?;
                resource.mount_path = unique_memory_mount_path(
                    &definition.name,
                    memory_store_id.as_str(),
                    &used_mounts,
                );
            }
            used_mounts.insert(resource.mount_path.trim_start_matches('/').to_string());
            normalized_resources.push(resource);
        }

        // Build the complete logical binding set with a non-durable placeholder
        // Repository id. The shared resolver owns replacement/collision/tree
        // semantics, and this call remains pure; only after it succeeds may the
        // adapter enter Registry/Vault participants.
        let attachments =
            plan_session_input_attachments(session_id, &normalized_resources, agent_defaults);
        let effective_bindings = awaken_session_contract::SessionInputResolver::effective_bindings(
            agent_defaults,
            &attachments,
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;

        Ok(PreparedSessionInputs {
            resources: normalized_resources,
            attachments,
            effective_bindings,
        })
    }

    /// Configure the Repository/Vault participants for one already-validated
    /// binding plan. No logical path or replacement decision is recomputed here.
    pub(crate) async fn lower_prepared_session_input_attachments(
        &self,
        session_id: &str,
        owner_scope: &str,
        prepared: &PreparedSessionInputs,
    ) -> Result<
        (
            Vec<awaken_session_contract::SessionInputAttachment>,
            Vec<awaken_session_application::ConfiguredSessionRepository>,
        ),
        StateError,
    > {
        let mut attachments = Vec::with_capacity(prepared.resources.len());
        let mut repository_configurations = Vec::new();
        for (index, resource) in prepared.resources.iter().enumerate() {
            let repository_id = if let ParsedInputTarget::Repository {
                remote_url,
                authorization_token,
                initial_branch,
                initial_commit,
            } = &resource.target
            {
                let repository_id = format!("managed:{session_id}:repository:{index}");
                let configured = match self
                    .application
                    .configure_session_repository(
                        awaken_session_application::SessionRepositoryResourceInput {
                            id: repository_id,
                            workspace_id: owner_scope.to_string(),
                            name: format!("Session repository {index}"),
                            description: "Managed compatibility Session input".into(),
                            remote_url: remote_url.clone(),
                            credential_material: authorization_token
                                .clone()
                                .map(|token| token.into_credential_material()),
                            credential: None,
                            mount_path: resource.mount_path.clone(),
                            initial_branch: initial_branch.clone(),
                            initial_commit: initial_commit.clone(),
                        },
                    )
                    .await
                {
                    Ok(configured) => configured,
                    Err(first) => {
                        if !self
                            .application
                            .abort_unadopted_session_repositories(&repository_configurations)
                            .await
                        {
                            tracing::warn!(
                                session = %session_id,
                                "Managed Session Repository compensation remains pending after lowering failure"
                            );
                        }
                        return Err(StateError::Run(first));
                    }
                };
                let repository_id = configured.repository_id.clone();
                repository_configurations.push(configured);
                Some(repository_id)
            } else {
                None
            };
            let mut attachment = prepared.attachments[index].clone();
            if let Some(repository_id) = repository_id {
                attachment.binding.target =
                    awaken_resource_contract::InputResourceId::Repository(repository_id);
            }
            // A replacement changes the resource occupying one logical Agent
            // slot; it does not invent a new slot identity. This keeps published
            // extension configuration (such as memory.binding_id) stable.
            attachments.push(attachment);
        }
        Ok((attachments, repository_configurations))
    }
}

/// Anthropic-compatible MemoryStore mount component. It is deliberately local
/// to the Managed anti-corruption layer: neutral Session and Worker contracts
/// consume the exact frozen path and never reproduce wire naming rules.
fn memory_mount_component(label: &str) -> String {
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
                implicit_memory_mount: false,
            },
            ResourceInput::MemoryStore {
                memory_store_id,
                instructions,
                access,
            } => ParsedSessionInput {
                target: ParsedInputTarget::MemoryStore(memory_store_id.clone().into()),
                // This provisional spelling is never exposed or frozen. The
                // Managed preparation step always replaces it with the
                // definition-owned display-name projection.
                mount_path: format!("/mnt/memory/{}", memory_mount_component(memory_store_id)),
                access: access.unwrap_or(ResourceAccess::ReadWrite).into(),
                instructions: instructions.clone(),
                implicit_memory_mount: true,
            },
            ResourceInput::GithubRepository {
                url,
                authorization_token,
                mount_path,
                checkout,
            } => ParsedSessionInput {
                mount_path: mount_path.clone().unwrap_or_else(|| {
                    awaken_provisioning_contract::WorkspaceLayout::child(&repo_name(url))
                }),
                target: ParsedInputTarget::Repository {
                    remote_url: url.clone(),
                    authorization_token: authorization_token.clone(),
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
                implicit_memory_mount: false,
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
                .to_parsed_input()
                .access,
            ReadOnly,
        );
        assert_eq!(
            repository(Some(RepositoryCheckout::Branch {
                name: "main".into()
            }))
            .to_parsed_input()
            .access,
            ReadWrite,
        );
        assert_eq!(repository(None).to_parsed_input().access, ReadWrite);
    }
}
