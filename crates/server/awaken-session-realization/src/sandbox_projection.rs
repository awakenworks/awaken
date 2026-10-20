// @formal entrypoint sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
//! The pure sandbox projection of a Session: which Resource mounts are
//! create-time state, which executions need no Environment, the inputs one
//! physical layout projection reads, the bare spec of an ephemeral sub-run, and
//! the mount requirements a Session Environment receives. Nothing here reads
//! installed state or performs I/O.

use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::environment_projection;

/// One physical-mount classifier shared by fresh-spec construction and live
/// transition validation. MemoryStore mounts are provider create-time state;
/// File mounts use the canonical late-attach port.
pub fn resource_mount_is_create_time(mount: &pc::MountRequirement) -> bool {
    matches!(mount.source, pc::MountSource::MemoryStore { .. })
}

/// The one pure create-time projection of an aggregate Resource manifest.
pub fn create_time_resource_mounts(
    resources: &awaken_session_configuration_contract::resource::ResolvedSessionResources,
    environment: Option<&environment_projection::FrozenEnvironmentInputs>,
) -> Vec<pc::MountRequirement> {
    resources
        .inputs()
        .iter()
        .filter_map(|input| {
            crate::managed_input_layout::resolved_input_validation_mount(input, environment)
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
    pub content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
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
    content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
) -> Vec<awaken_provisioning_contract::MountRequirement> {
    if let Some(baseline_mounts) = baseline_mounts {
        resources.extend_from_slice(baseline_mounts);
    }
    if content_delivery == Some(crate::frozen_inputs::ManagedContentDelivery::SemanticTools) {
        resources.retain(|mount| {
            !matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::MemoryStore { .. }
            )
        });
    }
    resources
}

/// The Runtime Host is the sole adapter from aggregate checkpoint intent to
/// provider byte-custody input. Ordinary suspension and terminal takeover both
/// call this projection so metadata, expiry, and effect identity cannot drift.
/// Session activity/source/lease coordinates stay on the aggregate-side request;
/// this capture input neither grants authority nor replaces the separate fence.
pub fn provisioning_checkpoint_request(
    request: &awaken_session_state_contract::SandboxCheckpointRequest,
) -> awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
    awaken_provisioning_contract::SandboxCheckpointCaptureRequest {
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.to_string(),
        generation_id: request.generation.id().to_owned(),
        environment_fingerprint: request.generation.environment_fingerprint().to_owned(),
        base_image_fingerprint: request.generation.base_image_fingerprint().to_owned(),
        effect_id: request.operation.effect_id().to_owned(),
        format: request.format.clone(),
        created_at_unix_ms: request.created_at_unix_ms,
        expires_at_unix_ms: request.expires_at_unix_ms,
        max_bytes: request.max_bytes,
    }
}
