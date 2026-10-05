// @formal surface obligation=sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
// @formal surface obligation=session_resources.external_io_requires_durable_prepared_state
// @formal reason: Pure mount-layout and memory write-consistency projection read by provisioning and resource projection.
//! Pure projection from frozen Managed resource inputs to Agent-visible mount
//! layout, validation mounts and prompt fragments. Provisioning and the Host's
//! resource projection both read it; it reads neither.

mod resource_prompt;

pub(crate) use resource_prompt::{AgentPathView, ResourcePromptProjection};

pub(crate) fn managed_file_mount_path(requested: &str) -> String {
    let logical = requested.trim_start_matches('/');
    if logical.starts_with("mnt/session/uploads/") {
        format!("/{logical}")
    } else {
        format!("/mnt/session/uploads/{logical}")
    }
}

pub(crate) struct ManagedMountMetadata {
    mount_id: String,
    mount_path: String,
    access: awaken_provisioning_contract::MountAccess,
    validation_source: ManagedMountValidationSource,
}

pub(crate) enum ManagedMountValidationSource {
    File,
    MemoryStore(String),
}

impl ManagedMountMetadata {
    pub(crate) fn requirement(
        self,
        source: awaken_provisioning_contract::MountSource,
    ) -> awaken_provisioning_contract::MountRequirement {
        let Self {
            mount_id,
            mount_path,
            access,
            ..
        } = self;
        awaken_provisioning_contract::MountRequirement {
            mount_id,
            source,
            mount_path,
            access,
            lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
            required: true,
        }
    }

    pub(crate) fn validation_requirement(
        self,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> awaken_provisioning_contract::MountRequirement {
        let source = match &self.validation_source {
            ManagedMountValidationSource::File => {
                awaken_provisioning_contract::MountSource::InlineBytes {
                    contents: Vec::new(),
                    content_hash: None,
                }
            }
            ManagedMountValidationSource::MemoryStore(store_id) => {
                awaken_provisioning_contract::MountSource::MemoryStore {
                    store_id: store_id.clone(),
                    materialization_reference: None,
                    write_consistency: projected_memory_write_consistency(
                        self.access,
                        environment.map(|projection| &projection.idle_retention),
                    ),
                }
            }
        };
        self.requirement(source)
    }
}

pub(crate) enum ManagedLayoutProjection {
    Mount(ManagedMountMetadata),
    Repository {
        mount_path: String,
        access: awaken_provisioning_contract::MountAccess,
    },
}

/// Authority available while validating one frozen Managed Sandbox layout.
///
/// A Coordinator dispatching a Worker-owned Session can validate only the
/// provider-neutral frozen structure. The claimed Worker owns the exact
/// provider projection and repeats the same structural kernel against that
/// provider's effective spec before any physical effect.
#[derive(Clone, Copy)]
pub(crate) enum ManagedLayoutValidationScope<'a> {
    Structural,
    Exact(&'a crate::session_environment::SessionEnvironmentProvider),
}

impl ManagedLayoutValidationScope<'_> {
    pub(crate) fn network_isolation(self) -> bool {
        match self {
            Self::Structural => true,
            Self::Exact(provider) => provider.realizer().capabilities().network_isolation,
        }
    }
}

impl ManagedLayoutProjection {
    pub(crate) fn mount_path(&self) -> &str {
        match self {
            Self::Mount(mount) => &mount.mount_path,
            Self::Repository { mount_path, .. } => mount_path,
        }
    }
}

pub(crate) enum ManagedInputKind<'a> {
    File(&'a str),
    Memory(&'a str),
    Repository,
}

pub(crate) struct ManagedInputLayoutView<'a> {
    kind: ManagedInputKind<'a>,
    binding_id: &'a str,
    mount_path: &'a str,
    access: awaken_resource_contract::ResourceAccess,
}

pub(crate) fn managed_mount_access(
    access: awaken_resource_contract::ResourceAccess,
) -> awaken_provisioning_contract::MountAccess {
    match access {
        awaken_resource_contract::ResourceAccess::ReadOnly => {
            awaken_provisioning_contract::MountAccess::ReadOnly
        }
        awaken_resource_contract::ResourceAccess::ReadWrite => {
            awaken_provisioning_contract::MountAccess::ReadWrite
        }
    }
}

/// One metadata projector shared by pure final-layout admission and effectful
/// staging. In particular, File mount identity is its immutable `file_id`, not
/// the replaceable binding id; duplicate content bindings therefore fail before
/// either durable root publication or File reads.
pub(crate) fn project_managed_input_layout(
    view: ManagedInputLayoutView<'_>,
) -> ManagedLayoutProjection {
    let access = managed_mount_access(view.access);
    match view.kind {
        ManagedInputKind::File(file_id) => ManagedLayoutProjection::Mount(ManagedMountMetadata {
            mount_id: file_id.to_string(),
            mount_path: managed_file_mount_path(view.mount_path),
            access: awaken_provisioning_contract::MountAccess::ReadOnly,
            validation_source: ManagedMountValidationSource::File,
        }),
        ManagedInputKind::Memory(store_id) => {
            ManagedLayoutProjection::Mount(ManagedMountMetadata {
                mount_id: view.binding_id.to_string(),
                mount_path: awaken_provisioning_contract::memory_materialization_mount_path(
                    view.mount_path,
                ),
                access,
                validation_source: ManagedMountValidationSource::MemoryStore(store_id.to_string()),
            })
        }
        ManagedInputKind::Repository => ManagedLayoutProjection::Repository {
            mount_path: view.mount_path.to_string(),
            access,
        },
    }
}

pub(crate) fn resolved_input_layout_projection(
    input: &awaken_session_resource_contract::ResolvedInput,
) -> ManagedLayoutProjection {
    let kind = match &input.source {
        awaken_session_resource_contract::ResolvedInputSource::File { file_id } => {
            ManagedInputKind::File(file_id.as_str())
        }
        awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id,
            ..
        } => ManagedInputKind::Memory(memory_store_id.as_str()),
        awaken_session_resource_contract::ResolvedInputSource::Repository { .. } => {
            ManagedInputKind::Repository
        }
    };
    project_managed_input_layout(ManagedInputLayoutView {
        kind,
        binding_id: input.binding_id.as_str(),
        mount_path: &input.mount_path,
        access: input.access,
    })
}

pub(crate) fn resolved_input_effect_path(
    input: &awaken_session_resource_contract::ResolvedInput,
) -> String {
    resolved_input_layout_projection(input)
        .mount_path()
        .to_string()
}

pub(crate) fn resolved_input_validation_mount(
    input: &awaken_session_resource_contract::ResolvedInput,
    environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
) -> Option<awaken_provisioning_contract::MountRequirement> {
    match resolved_input_layout_projection(input) {
        ManagedLayoutProjection::Mount(mount) => Some(mount.validation_requirement(environment)),
        ManagedLayoutProjection::Repository { .. } => None,
    }
}

pub(crate) fn binding_layout_projection(
    input: &awaken_resource_contract::InputBinding,
) -> ManagedLayoutProjection {
    let kind = match &input.target {
        awaken_resource_contract::InputResourceId::File(file_id) => {
            ManagedInputKind::File(file_id.as_str())
        }
        awaken_resource_contract::InputResourceId::MemoryStore(memory_store_id) => {
            ManagedInputKind::Memory(memory_store_id.as_str())
        }
        awaken_resource_contract::InputResourceId::Repository(_) => ManagedInputKind::Repository,
    };
    project_managed_input_layout(ManagedInputLayoutView {
        kind,
        binding_id: input.binding_id.as_str(),
        mount_path: &input.mount_path,
        access: input.access,
    })
}

/// The one pure Memory consistency decision shared by validation, effectful
/// staging, and final Sandbox assembly. Checkpoint-and-release may proceed only
/// when writable Memory is already write-through; read-only mounts and
/// Resident environments retain provider defaults.
pub(crate) fn projected_memory_write_consistency(
    access: awaken_provisioning_contract::MountAccess,
    idle_retention: Option<&awaken_provisioning_contract::SandboxIdleRetentionPolicy>,
) -> awaken_provisioning_contract::MemoryWriteConsistency {
    if access == awaken_provisioning_contract::MountAccess::ReadWrite
        && idle_retention.is_some_and(|policy| {
            policy.mode
                == awaken_provisioning_contract::SandboxIdleRetentionMode::CheckpointAndRelease
        })
    {
        awaken_provisioning_contract::MemoryWriteConsistency::WriteThroughRequired
    } else {
        awaken_provisioning_contract::MemoryWriteConsistency::ProviderDefault
    }
}
