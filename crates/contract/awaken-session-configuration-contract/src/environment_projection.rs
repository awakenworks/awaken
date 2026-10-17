// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: The one projection of a frozen Environment into the sandbox spec, admission vector and
// @formal reason: capacity shape; placement and realization read the same network and isolation policy.
//! The frozen Session Environment projected into neutral provisioning
//! vocabulary. Worker placement, warm capacity and Session realization all read
//! these projections, so a Worker cannot claim requirements different from
//! those it materializes.

use awaken_provisioning_contract as pc;

/// The sole process-local projection of the frozen Environment snapshot.
/// Native and ACP provisioning both consume this value; no late network or
/// Sandbox override is stored beside it.
#[derive(Clone)]
pub struct FrozenEnvironmentInputs {
    pub fingerprint: crate::EnvironmentFingerprint,
    pub network: pc::NetworkPolicy,
    pub packages: awaken_environment_contract::PackageRequirements,
    pub sandbox: Option<pc::SandboxOverride>,
    pub provisioning: crate::SandboxProvisioning,
    pub idle_retention: crate::EnvironmentIdleRetentionPolicy,
}

/// Canonical projection from the frozen Session Environment into neutral
/// provisioning vocabulary. Admission and realization both consume this value,
/// so a Worker cannot claim requirements different from those it materializes.
pub fn project_environment(environment: &crate::EnvironmentSnapshot) -> FrozenEnvironmentInputs {
    let network = match &environment.network {
        crate::SessionNetworkPolicy::Unrestricted => pc::NetworkPolicy::Unrestricted,
        crate::SessionNetworkPolicy::Allowlist { hosts } => pc::NetworkPolicy::Allowlist {
            hosts: hosts.clone(),
        },
        crate::SessionNetworkPolicy::None => pc::NetworkPolicy::None,
    };
    let packages = environment.prepared_image.as_ref().map_or_else(
        || awaken_environment_contract::PackageRequirements {
            managers: environment
                .packages
                .manager_packages()
                .into_iter()
                .filter(|(_, packages)| !packages.is_empty())
                .map(|(manager, packages)| (manager.to_owned(), packages.to_vec()))
                .collect(),
            resolution_id: Some(format!(
                "{}:{}",
                environment.environment_id, environment.revision.0
            )),
        },
        |_| awaken_environment_contract::PackageRequirements::default(),
    );
    let mut sandbox = environment.sandbox.clone();
    // EnvironmentSnapshot.network is the sole reachability authority. The
    // authoring overlay cannot become a second, late widening decision.
    sandbox.network = None;
    if let Some(image) = &environment.prepared_image {
        sandbox.environment = Some(pc::EnvironmentKind::Image {
            reference: image.clone(),
        });
    }
    FrozenEnvironmentInputs {
        fingerprint: environment.config_fingerprint.clone(),
        network,
        packages,
        sandbox: (!sandbox.is_empty()).then_some(sandbox),
        provisioning: environment.sandbox_provisioning,
        idle_retention: environment.idle_retention.clone(),
    }
}

/// Exact admission vector derived from the same canonical projection used by
/// Session realization. Repository presence is the frozen typed resource fact;
/// `opaque_process` distinguishes projected ACP from cooperative Native
/// execution. All callers use this complete signature so no compatibility
/// overload can omit one Session path cause.
pub fn sandbox_requirements(
    environment: &crate::EnvironmentSnapshot,
    has_repository: bool,
    opaque_process: bool,
) -> pc::SandboxRequirements {
    let projection = project_environment(environment);
    let spec = sandbox_spec_from_projection(
        &environment.environment_id,
        Vec::new(),
        Vec::new(),
        Some(&projection),
        true,
    );
    pc::session_sandbox_projection(&spec, has_repository, opaque_process).1
}

/// Scheduling demand projected through the same Environment overlay used for
/// capability admission and eventual sandbox creation.
pub fn sandbox_resource_requests(environment: &crate::EnvironmentSnapshot) -> pc::ResourceRequests {
    let projection = project_environment(environment);
    sandbox_spec_from_projection(
        &environment.environment_id,
        Vec::new(),
        Vec::new(),
        Some(&projection),
        true,
    )
    .requests
}

/// Canonical mount/env-independent Environment projection used by both Session
/// creation and proactive capacity. Callers supply per-Session mounts and env;
/// an empty pair therefore yields the exact poolable shape, not a second warmup
/// approximation.
pub fn sandbox_spec_from_projection(
    scope: &str,
    mounts: Vec<pc::MountRequirement>,
    env: Vec<pc::EnvVar>,
    environment: Option<&FrozenEnvironmentInputs>,
    provider_enforces_network_isolation: bool,
) -> pc::SandboxSpec {
    let projected_network = environment
        .map(|projection| projection.network.clone())
        .unwrap_or(pc::NetworkPolicy::Unrestricted);
    // Workdir can enforce denial only for spawned tools via its wrapper. Stronger
    // providers retain the exact frozen policy for admission and enforcement.
    let network = if projected_network.is_restricted() && !provider_enforces_network_isolation {
        pc::NetworkPolicy::Unrestricted
    } else {
        projected_network
    };
    let deny_tool_egress = environment.is_some_and(|projection| projection.network.is_restricted());
    let base = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.to_owned(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress,
        mounts,
        env,
        packages: environment
            .map(|projection| projection.packages.clone())
            .unwrap_or_default(),
        network,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.to_owned(),
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        control_services: Default::default(),
        lease_ttl_secs: None,
    };
    let mut spec = environment
        .and_then(|projection| projection.sandbox.clone())
        .map_or(base.clone(), |sandbox| sandbox.apply(base));
    project_memory_write_consistency(&mut spec.mounts, environment);
    spec
}

/// Apply the canonical per-mount decision after the Environment Sandbox
/// overlay so authored mounts cannot bypass the same consistency policy.
fn project_memory_write_consistency(
    mounts: &mut [pc::MountRequirement],
    environment: Option<&FrozenEnvironmentInputs>,
) {
    for mount in mounts {
        if let pc::MountSource::MemoryStore {
            write_consistency, ..
        } = &mut mount.source
        {
            *write_consistency = pc::projected_memory_write_consistency(
                mount.access,
                environment.map(|projection| &projection.idle_retention),
            );
        }
    }
}

/// Canonical mount-less durable Session shape used by cold creation and the
/// capacity planner. It is intentionally Retained; a concrete warm-pool owner
/// will return zero because durable Session filesystems cannot be transferred
/// by rebinding an in-process scope.
pub fn default_session_sandbox_spec(scope: &str) -> pc::SandboxSpec {
    sandbox_spec_from_projection(scope, Vec::new(), Vec::new(), None, true)
}

pub struct EnvironmentCapacityProjection {
    pub spec: pc::SandboxSpec,
    pub shape_id: pc::SandboxCapacityShapeId,
}

/// Project one frozen Environment into both the creation request and its
/// canonical capacity identity. Keeping them in one value prevents placement,
/// heartbeat receipts, and pool checkout from recomputing parallel identities.
pub fn environment_capacity_projection(
    environment: &crate::EnvironmentSnapshot,
    provider_enforces_network_isolation: bool,
) -> EnvironmentCapacityProjection {
    let projection = project_environment(environment);
    let spec = sandbox_spec_from_projection(
        "environment-warmup",
        Vec::new(),
        Vec::new(),
        Some(&projection),
        provider_enforces_network_isolation,
    );
    let shape_id = pc::SandboxCapacityShapeId::from_spec(&spec)
        .expect("Environment capacity projection is mount-less");
    EnvironmentCapacityProjection { spec, shape_id }
}
