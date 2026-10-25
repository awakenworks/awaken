// @formal entrypoint sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
//! The pure sandbox projection of a Session: which Resource mounts are
//! create-time state, which executions need no Environment, the inputs one
//! physical layout projection reads, the bare spec of an ephemeral sub-run, and
//! the mount requirements a Session Environment receives. Nothing here reads
//! installed state or performs I/O.

use crate::environment_projection;
use awaken_provisioning_contract as pc;

/// One Session-wide delivery decision for resources that can be represented
/// either as files or as semantic tools. The frozen Resource/Skill identities
/// remain authoritative; this value selects only their runtime projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedContentDelivery {
    /// Anthropic Managed Agents compatible filesystem projection. Memory stores
    /// are mounted and Skills are discovered from their `SKILL.md` paths.
    #[default]
    ManagedFilesystem,
    /// Filesystem-free projection for Native agents whose published tool policy
    /// cannot invoke a filesystem tool. The same frozen bindings are exposed by
    /// bounded semantic tools instead.
    SemanticTools,
}

/// One physical-mount classifier shared by fresh-spec construction and live
/// transition validation. MemoryStore mounts are provider create-time state;
/// File mounts use the canonical late-attach port.
pub fn resource_mount_is_create_time(mount: &pc::MountRequirement) -> bool {
    matches!(mount.source, pc::MountSource::MemoryStore { .. })
}

/// The one pure create-time projection of an aggregate Resource manifest.
pub fn create_time_resource_mounts(
    resources: &crate::resource::ResolvedSessionResources,
    environment: Option<&environment_projection::FrozenEnvironmentInputs>,
) -> Vec<pc::MountRequirement> {
    resources
        .inputs()
        .iter()
        .filter_map(|input| {
            crate::managed_layout::resolved_input_validation_mount(input, environment)
        })
        .filter(resource_mount_is_create_time)
        .collect()
}

/// An inference-only execution can omit a physical Environment only when its complete immutable
/// execution surface is environment-free. Inference-only Native/A2A routes do
/// not need a Sandbox; ACP owns a CLI process and every executable integration
/// keeps the ordinary Session Environment path.
pub fn environment_free_inference(
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) -> bool {
    let agent = &snapshot.resolved_spec.plugin_config.agent;
    snapshot.resolved_spec.tool_descriptors.is_empty()
        && snapshot.resolved_spec.plugin_ids.is_empty()
        && agent.mcp_servers.is_empty()
        && agent.skills.is_empty()
        && agent.delegates.is_empty()
        && agent.advisor.is_none()
        && agent.toolsets.is_empty()
        && snapshot
            .resolved_spec
            .attempt_candidates(None)
            .into_iter()
            .all(|candidate| {
                matches!(
                    awaken_runtime_contract::resolved::Backend::from_ref(
                        candidate.binding().identity().2,
                    ),
                    awaken_runtime_contract::resolved::Backend::Native
                        | awaken_runtime_contract::resolved::Backend::Remote(_)
                )
            })
}

/// One ephemeral, complete input snapshot for projecting a Session's physical
/// Sandbox layout. It is assembled at the caller's single projection point and
/// consumed immediately; the frozen Session and process-local installed Session remain the
/// authorities for every field.
pub struct ProjectedSandboxLayout<'a> {
    pub resource_mounts: Vec<pc::MountRequirement>,
    pub has_repositories: bool,
    pub baseline_mounts: Option<&'a [pc::MountRequirement]>,
    pub baseline_env: Option<&'a [pc::EnvVar]>,
    pub content_delivery: Option<ManagedContentDelivery>,
    pub environment: Option<&'a environment_projection::FrozenEnvironmentInputs>,
    pub network_isolation: bool,
}

/// Complete frozen/installed inputs share this one spec projection. The caller
/// resolves any legacy installed-environment fallback before entering here.
pub fn project_sandbox_spec(
    thread: &str,
    projection: ProjectedSandboxLayout<'_>,
    env: Vec<pc::EnvVar>,
) -> pc::SandboxSpec {
    let mounts = project_session_mounts(
        projection.resource_mounts,
        projection.baseline_mounts,
        projection.content_delivery,
    );
    let spec = environment_projection::sandbox_spec_from_projection(
        thread,
        mounts,
        env,
        projection.environment,
        projection.network_isolation,
    );
    if projection.environment.is_some_and(|environment| {
        environment
            .sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.host_access == pc::HostAccessPolicy::TrustedHost)
    }) {
        spec
    } else {
        pc::session_sandbox_projection(&spec, projection.has_repositories, false).0
    }
}

/// A bare Workdir spec for an ephemeral sub-run sandbox (judge / delegate / compact /
/// skill fork): scoped to the thread, no staged resource mounts, host-shared network.
pub fn agent_run_sandbox_spec(thread: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: thread.to_string(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.to_string(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        control_services: Default::default(),
        lease_ttl_secs: None,
    }
}

/// The mount requirements the Session Environment receives: the staged Resource
/// mounts plus the frozen baseline mounts. Semantic-tool content delivery
/// reads Memory through tools, so its MemoryStore mounts are not projected.
pub fn project_session_mounts(
    mut resources: Vec<awaken_provisioning_contract::MountRequirement>,
    baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
    content_delivery: Option<ManagedContentDelivery>,
) -> Vec<awaken_provisioning_contract::MountRequirement> {
    if let Some(baseline_mounts) = baseline_mounts {
        resources.extend_from_slice(baseline_mounts);
    }
    if content_delivery == Some(ManagedContentDelivery::SemanticTools) {
        resources.retain(|mount| {
            !matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::MemoryStore { .. }
            )
        });
    }
    resources
}

/// Validate the provider-neutral structure of one prospective Managed
/// Sandbox layout: its Resource inputs mounted over the frozen baseline mounts
/// and environment, with network isolation assumed. A Coordinator that
/// dispatches a Worker-placed Session can validate only this structure; the
/// claimed Worker repeats the same projection against its provider's
/// effective spec before any physical effect.
///
/// Decision table (cause → effect):
/// | Rule | Repository paths | Effect |
/// |---|---|---|
/// | V1 | none | accepted: there is no Repository to adopt |
/// | V2 | each outside every mount and the reserved roots | accepted |
/// | V3 | one nested in a mount, a reserved root or another Repository | the provisioning refusal |
pub fn validate_structural_sandbox_layout(
    thread: &str,
    inputs: impl IntoIterator<Item = crate::managed_layout::ManagedLayoutProjection>,
    baseline_mounts: &[pc::MountRequirement],
    baseline_env: &[pc::EnvVar],
    environment: Option<&environment_projection::FrozenEnvironmentInputs>,
) -> Result<(), pc::SandboxError> {
    let mut mounts = Vec::new();
    let mut repository_paths = Vec::new();
    for input in inputs {
        match input {
            crate::managed_layout::ManagedLayoutProjection::Mount(mount) => {
                mounts.push(mount.validation_requirement(environment));
            }
            crate::managed_layout::ManagedLayoutProjection::Repository { mount_path, .. } => {
                repository_paths.push(mount_path);
            }
        }
    }
    let spec = project_sandbox_spec(
        thread,
        ProjectedSandboxLayout {
            resource_mounts: mounts,
            has_repositories: !repository_paths.is_empty(),
            baseline_mounts: Some(baseline_mounts),
            baseline_env: Some(baseline_env),
            content_delivery: None,
            environment,
            network_isolation: true,
        },
        baseline_env.to_vec(),
    );
    let repository_paths = repository_paths
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    pc::validate_repository_sandbox_adoption_layout(&repository_paths, &spec, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{
        BindingId, InputBinding, InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
    };

    fn binding(id: &str, target: InputResourceId, mount_path: &str) -> InputBinding {
        InputBinding {
            binding_id: BindingId::from(id),
            target,
            mount_path: mount_path.into(),
            access: ResourceAccess::ReadWrite,
            instructions: None,
        }
    }

    fn validate(bindings: &[InputBinding]) -> Result<(), pc::SandboxError> {
        validate_structural_sandbox_layout(
            "structural-session",
            bindings
                .iter()
                .map(crate::managed_layout::binding_layout_projection),
            &[],
            &[],
            None,
        )
    }

    #[test]
    fn structural_layout_validation_follows_its_decision_table() {
        // V1: a Memory mount and no Repository is accepted.
        let memory = binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("store")),
            "/mnt/memory/notes",
        );
        validate(std::slice::from_ref(&memory)).expect("V1");
        // V2: a Repository beside the Memory mount is accepted.
        let repository = binding(
            "repository",
            InputResourceId::Repository(RepositoryId::from("repo")),
            "/workspace/repo",
        );
        validate(&[memory.clone(), repository]).expect("V2");
        // V3: a Repository nested in the Memory mount is refused.
        let nested = binding(
            "nested",
            InputResourceId::Repository(RepositoryId::from("repo")),
            "/mnt/memory/notes/repo",
        );
        assert!(validate(&[memory, nested]).is_err(), "V3");
    }
}
