// @formal surface obligation=sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
// @formal surface obligation=session_resources.external_io_requires_durable_prepared_state
// @formal reason: Pure mount-layout and memory write-consistency projection read by provisioning and resource projection.
//! Pure projection from frozen Managed resource inputs to Agent-visible mount
//! layout, validation mounts and prompt fragments. Provisioning and the Host's
//! resource projection both read it; it reads neither.

use awaken_session_configuration_contract::environment_projection;
use awaken_session_configuration_contract::resource as session_resource;
mod resource_prompt;

pub use resource_prompt::{AgentPathView, ResourcePromptProjection};

pub fn managed_file_mount_path(requested: &str) -> String {
    let logical = requested.trim_start_matches('/');
    if logical.starts_with("mnt/session/uploads/") {
        format!("/{logical}")
    } else {
        format!("/mnt/session/uploads/{logical}")
    }
}

pub struct ManagedMountMetadata {
    mount_id: String,
    mount_path: String,
    access: awaken_provisioning_contract::MountAccess,
    validation_source: ManagedMountValidationSource,
}

pub enum ManagedMountValidationSource {
    File,
    MemoryStore(String),
}

impl ManagedMountMetadata {
    pub fn requirement(
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

    pub fn validation_requirement(
        self,
        environment: Option<&environment_projection::FrozenEnvironmentInputs>,
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
                    write_consistency:
                        awaken_provisioning_contract::projected_memory_write_consistency(
                            self.access,
                            environment.map(|projection| &projection.idle_retention),
                        ),
                }
            }
        };
        self.requirement(source)
    }
}

pub enum ManagedLayoutProjection {
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
pub enum ManagedLayoutValidationScope<'a> {
    Structural,
    Exact(&'a crate::session_environment::SessionEnvironmentProvider),
}

impl ManagedLayoutValidationScope<'_> {
    pub fn network_isolation(self) -> bool {
        match self {
            Self::Structural => true,
            Self::Exact(provider) => provider.realizer().capabilities().network_isolation,
        }
    }
}

impl ManagedLayoutProjection {
    pub fn mount_path(&self) -> &str {
        match self {
            Self::Mount(mount) => &mount.mount_path,
            Self::Repository { mount_path, .. } => mount_path,
        }
    }
}

pub enum ManagedInputKind<'a> {
    File(&'a str),
    Memory(&'a str),
    Repository,
}

pub struct ManagedInputLayoutView<'a> {
    kind: ManagedInputKind<'a>,
    binding_id: &'a str,
    mount_path: &'a str,
    access: awaken_resource_contract::ResourceAccess,
}

pub fn managed_mount_access(
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
pub fn project_managed_input_layout(view: ManagedInputLayoutView<'_>) -> ManagedLayoutProjection {
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

pub fn resolved_input_layout_projection(
    input: &session_resource::ResolvedInput,
) -> ManagedLayoutProjection {
    let kind = match &input.source {
        session_resource::ResolvedInputSource::File { file_id } => {
            ManagedInputKind::File(file_id.as_str())
        }
        session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id, ..
        } => ManagedInputKind::Memory(memory_store_id.as_str()),
        session_resource::ResolvedInputSource::Repository { .. } => ManagedInputKind::Repository,
    };
    project_managed_input_layout(ManagedInputLayoutView {
        kind,
        binding_id: input.binding_id.as_str(),
        mount_path: &input.mount_path,
        access: input.access,
    })
}

pub fn resolved_input_effect_path(input: &session_resource::ResolvedInput) -> String {
    resolved_input_layout_projection(input)
        .mount_path()
        .to_string()
}

pub fn resolved_input_validation_mount(
    input: &session_resource::ResolvedInput,
    environment: Option<&environment_projection::FrozenEnvironmentInputs>,
) -> Option<awaken_provisioning_contract::MountRequirement> {
    match resolved_input_layout_projection(input) {
        ManagedLayoutProjection::Mount(mount) => Some(mount.validation_requirement(environment)),
        ManagedLayoutProjection::Repository { .. } => None,
    }
}

pub fn binding_layout_projection(
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
