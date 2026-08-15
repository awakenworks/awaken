//! `prepare_environment` — the pure planning step between a [`SandboxSpec`] and a
//! provider's `create`. It validates the spec against a backend's
//! [`SandboxCapabilities`] and returns a normalized [`EnvironmentPlan`], failing
//! closed when the backend cannot honor a requested guarantee. Pure and
//! deterministic: no I/O, safe to run at admission time and to unit-test.

use crate::sandbox::SandboxCapabilities;
use crate::spec::SandboxSpec;
use crate::vocab::{
    EnvVar, EnvVisibility, MountAccess, MountRequirement, NetworkPolicy, PackageRequirements,
    RESERVED_ENV_KEYS, ResourceLimits, ResourceRequests,
};

/// A validated, normalized plan ready to hand to [`crate::SandboxProvider::create`].
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentPlan {
    pub scope: String,
    pub mounts: Vec<MountRequirement>,
    pub env: Vec<EnvVar>,
    pub packages: PackageRequirements,
    pub network: NetworkPolicy,
    pub outputs_path: String,
    /// Scheduler reservation carried unchanged to infrastructure adapters.
    pub requests: ResourceRequests,
    /// The resource caps to enforce — carried forward so a provider realizes exactly
    /// what was admitted (and never a silently-dropped cap).
    pub limits: ResourceLimits,
}

/// Why a spec cannot be prepared against a backend.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrepareError {
    #[error("backend isolation is weaker than requested")]
    InsufficientIsolation,
    #[error("read-only mount {0:?} requested but backend does not enforce read-only")]
    ReadOnlyUnsupported(String),
    #[error("egress-only secret {0:?} requested but backend cannot substitute at egress")]
    EgressSecretUnsupported(String),
    #[error("network policy requested but backend has no network isolation")]
    NetworkIsolationUnsupported,
    #[error("network allowlist requested but backend has no no-bypass allowlist enforcement")]
    NetworkAllowlistUnsupported,
    #[error("package requirements requested but backend cannot provision packages")]
    PackageProvisioningUnsupported,
    #[error("resource limits requested but backend cannot enforce them")]
    ResourceLimitsUnsupported,
    #[error("resource request exceeds its {0} limit")]
    ResourceRequestExceedsLimit(&'static str),
    #[error("env key {0:?} is reserved by the runtime")]
    ReservedEnvKey(String),
    #[error("outputs_path must be an absolute sandbox path")]
    OutputsPathNotAbsolute,
}

/// Validate only the guarantees carried by mount requirements.
///
/// Live Session resource updates cannot rerun full environment admission, but
/// they must use the same fail-closed rule as initial sandbox creation. Keeping
/// this check here makes mount admission one source of truth for both paths.
pub fn validate_mount_requirements(
    mounts: &[MountRequirement],
    caps: &SandboxCapabilities,
) -> Result<(), PrepareError> {
    if !caps.enforced_readonly
        && let Some(mount) = mounts
            .iter()
            .find(|mount| mount.access == MountAccess::ReadOnly)
    {
        return Err(PrepareError::ReadOnlyUnsupported(mount.mount_id.clone()));
    }
    Ok(())
}

/// Validate `spec` against `caps` and produce a plan. Fail-closed: any guarantee
/// the backend cannot enforce is an error, never a silent downgrade.
pub fn prepare_environment(
    spec: &SandboxSpec,
    caps: &SandboxCapabilities,
) -> Result<EnvironmentPlan, PrepareError> {
    // Isolation: backend must meet or exceed the requested class.
    if caps.isolation < spec.isolation {
        return Err(PrepareError::InsufficientIsolation);
    }

    // Outputs must be a sandbox-absolute path (G3: sandbox-absolute, not host).
    if !spec.outputs_path.starts_with('/') {
        return Err(PrepareError::OutputsPathNotAbsolute);
    }

    validate_mount_requirements(&spec.mounts, caps)?;

    // Env: no reserved keys; egress-only secrets need substitution support.
    for var in &spec.env {
        if RESERVED_ENV_KEYS.contains(&var.name.as_str()) {
            return Err(PrepareError::ReservedEnvKey(var.name.clone()));
        }
        if var.visibility == EnvVisibility::EgressOnly && !caps.secret_egress_substitution {
            return Err(PrepareError::EgressSecretUnsupported(var.name.clone()));
        }
    }

    // A non-`Unrestricted` policy needs a backend that can isolate egress.
    if spec.network.rank() > NetworkPolicy::Unrestricted.rank() && !caps.network_isolation {
        return Err(PrepareError::NetworkIsolationUnsupported);
    }
    if matches!(spec.network, NetworkPolicy::Allowlist { .. }) && !caps.enforced_network_allowlist {
        return Err(PrepareError::NetworkAllowlistUnsupported);
    }

    if !spec.packages.is_empty() && !caps.package_provisioning {
        return Err(PrepareError::PackageProvisioningUnsupported);
    }

    // Resource caps must be enforceable — never silently ignored on a tier that
    // can't cgroup them (bwrap reports `resource_limits = false`).
    if spec.limits.is_set() && !caps.resource_limits {
        return Err(PrepareError::ResourceLimitsUnsupported);
    }
    if let Some(resource) = spec.requests.first_limit_violation(&spec.limits) {
        return Err(PrepareError::ResourceRequestExceedsLimit(resource));
    }

    Ok(EnvironmentPlan {
        scope: spec.scope.clone(),
        mounts: spec.mounts.clone(),
        env: spec.env.clone(),
        packages: spec.packages.clone(),
        network: spec.network.clone(),
        outputs_path: spec.outputs_path.clone(),
        requests: spec.requests.clone(),
        limits: spec.limits.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::IsolationClass;
    use crate::vocab::{EnvValue, EnvVar, EnvVisibility, MountLifetime, MountSource};

    fn caps(isolation: IsolationClass) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation,
            tool_transparent: isolation >= IsolationClass::Namespace,
            path_fidelity: isolation >= IsolationClass::Namespace,
            enforced_readonly: isolation >= IsolationClass::Namespace,
            network_isolation: isolation >= IsolationClass::Namespace,
            enforced_network_allowlist: isolation >= IsolationClass::Namespace,
            secret_egress_substitution: isolation == IsolationClass::Container,
            resource_limits: isolation >= IsolationClass::Namespace,
            custom_rootfs: isolation == IsolationClass::Container,
            package_provisioning: false,
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "thread-1".into(),
            isolation: IsolationClass::Namespace,
            mounts: vec![MountRequirement {
                mount_id: "in".into(),
                source: MountSource::File {
                    file_id: "file_x".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/data.csv".into(),
                access: MountAccess::ReadOnly,
                lifetime: MountLifetime::PerRun,
                required: true,
            }],
            env: vec![EnvVar {
                name: "TZ".into(),
                value: EnvValue::Inline {
                    value: "UTC".into(),
                },
                visibility: EnvVisibility::Process,
            }],
            packages: Default::default(),
            network: NetworkPolicy::Allowlist {
                hosts: vec!["api.anthropic.com".into()],
            },
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: crate::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[test]
    fn prepares_against_a_capable_backend() {
        assert!(prepare_environment(&spec(), &caps(IsolationClass::Namespace)).is_ok());
    }

    /// Package admission cause graph / decision table:
    /// | Requirements | Provider capability | Result |
    /// |---|---|---|
    /// | empty | false | admit |
    /// | non-empty | false | reject before provider I/O |
    /// | non-empty | true | exact requirements in plan |
    #[test]
    fn package_requirements_are_capability_gated_and_lossless() {
        let empty = spec();
        assert!(
            prepare_environment(&empty, &caps(IsolationClass::Namespace)).is_ok(),
            "empty requirements need no capability"
        );
        let mut requested = spec();
        requested
            .packages
            .managers
            .insert("pip".into(), vec!["httpx==0.28.0".into()]);
        assert_eq!(
            prepare_environment(&requested, &caps(IsolationClass::Container)),
            Err(PrepareError::PackageProvisioningUnsupported),
            "unsupported provider fails before I/O"
        );
        let mut capable = caps(IsolationClass::Container);
        capable.package_provisioning = true;
        assert_eq!(
            prepare_environment(&requested, &capable)
                .expect("capable provider")
                .packages,
            requested.packages,
            "exact requirements cross the seam"
        );
    }

    #[test]
    fn rejects_weaker_isolation() {
        assert_eq!(
            prepare_environment(&spec(), &caps(IsolationClass::Workdir)),
            Err(PrepareError::InsufficientIsolation)
        );
    }

    #[test]
    fn rejects_readonly_on_a_lexical_backend() {
        // A Workdir backend that still claims Namespace isolation but no ro enforcement.
        let mut c = caps(IsolationClass::Namespace);
        c.enforced_readonly = false;
        assert_eq!(
            prepare_environment(&spec(), &c),
            Err(PrepareError::ReadOnlyUnsupported("in".into()))
        );
    }

    /// Mount admission cause/effect table shared by create and hot-plug paths:
    /// | RO mount present | Provider enforces RO | Effect |
    /// |---|---|---|
    /// | no | either | admit |
    /// | yes | yes | admit |
    /// | yes | no | reject with the first offending mount id |
    #[test]
    fn mount_only_admission_uses_the_same_readonly_rule() {
        let requested = spec().mounts;
        let mut incapable = caps(IsolationClass::Namespace);
        incapable.enforced_readonly = false;
        assert_eq!(
            validate_mount_requirements(&requested, &incapable),
            Err(PrepareError::ReadOnlyUnsupported("in".into()))
        );
        assert!(validate_mount_requirements(&[], &incapable).is_ok());
        assert!(validate_mount_requirements(&requested, &caps(IsolationClass::Namespace)).is_ok());
    }

    #[test]
    fn rejects_a_relative_outputs_path() {
        let mut s = spec();
        s.outputs_path = "relative/outputs".into();
        assert_eq!(
            prepare_environment(&s, &caps(IsolationClass::Namespace)),
            Err(PrepareError::OutputsPathNotAbsolute)
        );
    }

    #[test]
    fn an_egress_only_secret_requires_substitution_support() {
        let mut s = spec();
        s.env = vec![EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Secret {
                reference: "broker://k".into(),
            },
            visibility: EnvVisibility::EgressOnly,
        }];
        // The Namespace tier cannot substitute at egress → fail closed.
        assert_eq!(
            prepare_environment(&s, &caps(IsolationClass::Namespace)),
            Err(PrepareError::EgressSecretUnsupported("API_KEY".into()))
        );
        // A Container tier advertises the capability → accepted.
        assert!(prepare_environment(&s, &caps(IsolationClass::Container)).is_ok());
    }

    #[test]
    fn rejects_network_policy_without_isolation() {
        let mut c = caps(IsolationClass::Namespace);
        c.network_isolation = false;
        assert_eq!(
            prepare_environment(&spec(), &c),
            Err(PrepareError::NetworkIsolationUnsupported)
        );
    }

    /// Network-policy cause graph:
    ///
    /// C1 restricted policy -> C2 general isolation -> (C3 allowlist requested
    /// -> C4 no-bypass allowlist enforcement) -> E1 admit. Missing C2 yields
    /// E2 `NetworkIsolationUnsupported`; C3 with missing C4 yields E3
    /// `NetworkAllowlistUnsupported`.
    ///
    /// | Rule | Policy | C2 isolation | C4 allowlist | Effect |
    /// |---|---|---:|---:|---|
    /// | N1 | unrestricted | 0 | 0 | admit |
    /// | N2 | none | 0 | - | E2 |
    /// | N3 | none | 1 | - | admit |
    /// | N4 | allowlist | 0 | 0 | E2 |
    /// | N5 | allowlist | 1 | 0 | E3 |
    /// | N6 | allowlist | 1 | 1 | admit |
    #[test]
    fn network_policy_requires_its_exact_enforcement_capability() {
        let mut request = spec();
        request.mounts.clear();
        let mut provider = caps(IsolationClass::Namespace);

        request.network = NetworkPolicy::Unrestricted;
        provider.network_isolation = false;
        provider.enforced_network_allowlist = false;
        assert!(prepare_environment(&request, &provider).is_ok());

        request.network = NetworkPolicy::None;
        assert_eq!(
            prepare_environment(&request, &provider),
            Err(PrepareError::NetworkIsolationUnsupported)
        );
        provider.network_isolation = true;
        assert!(prepare_environment(&request, &provider).is_ok());

        request.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        provider.network_isolation = false;
        assert_eq!(
            prepare_environment(&request, &provider),
            Err(PrepareError::NetworkIsolationUnsupported)
        );
        provider.network_isolation = true;
        assert_eq!(
            prepare_environment(&request, &provider),
            Err(PrepareError::NetworkAllowlistUnsupported)
        );
        provider.enforced_network_allowlist = true;
        assert!(prepare_environment(&request, &provider).is_ok());
    }

    #[test]
    fn rejects_reserved_env_key() {
        let mut s = spec();
        s.env[0].name = "PATH".into();
        assert_eq!(
            prepare_environment(&s, &caps(IsolationClass::Namespace)),
            Err(PrepareError::ReservedEnvKey("PATH".into()))
        );
    }

    #[test]
    fn an_isolation_violation_masks_a_lower_precedence_reserved_key() {
        // Cause-effect masking: prepare_environment short-circuits in precedence order,
        // so a spec that violates isolation (checked first) AND also carries a reserved
        // env key reports InsufficientIsolation — the reserved-key fault stays masked,
        // never reached. Proves the ordering, not just the individual checks.
        let mut s = spec();
        s.isolation = IsolationClass::Container; // require the strongest tier
        s.env[0].name = "PATH".into(); // a reserved key that MUST stay masked
        assert_eq!(
            prepare_environment(&s, &caps(IsolationClass::Workdir)),
            Err(PrepareError::InsufficientIsolation)
        );
    }

    #[test]
    fn a_no_egress_policy_needs_isolation_and_is_admitted_with_it() {
        // The `None` egress class (most restrictive) — the other prepare egress tests
        // only used `Allowlist`. With isolation it prepares; without it fails closed.
        let mut s = spec();
        s.network = NetworkPolicy::None;
        assert!(prepare_environment(&s, &caps(IsolationClass::Namespace)).is_ok());
        let mut c = caps(IsolationClass::Namespace);
        c.network_isolation = false;
        assert_eq!(
            prepare_environment(&s, &c),
            Err(PrepareError::NetworkIsolationUnsupported)
        );
    }

    #[test]
    fn rejects_resource_limits_a_backend_cannot_enforce() {
        let mut s = spec();
        s.limits.memory_bytes = Some(512 * 1024 * 1024);
        let mut c = caps(IsolationClass::Namespace);
        c.resource_limits = false; // e.g. the bwrap tier
        assert_eq!(
            prepare_environment(&s, &c),
            Err(PrepareError::ResourceLimitsUnsupported)
        );
    }

    #[test]
    fn empty_limits_pass_a_non_enforcing_backend() {
        // Default (unset) limits must not trip the fail-closed check.
        let mut s = spec();
        s.limits = Default::default();
        let mut c = caps(IsolationClass::Namespace);
        c.resource_limits = false;
        assert!(prepare_environment(&s, &c).is_ok());
    }

    #[test]
    fn plan_carries_the_admitted_limits_forward() {
        let mut s = spec();
        s.limits.cpu_millis = Some(1500);
        s.limits.pids = Some(128);
        let plan = prepare_environment(&s, &caps(IsolationClass::Container)).unwrap();
        assert_eq!(plan.limits.cpu_millis, Some(1500));
        assert_eq!(plan.limits.pids, Some(128));
    }

    #[test]
    fn requests_are_preserved_but_cannot_exceed_matching_limits() {
        // Cause/effect table: C1 request set, C2 matching limit set, C3 request <=
        // limit. R1 C1+!C2 => preserve request; R2 C1+C2+C3 => preserve both;
        // R3 C1+C2+!C3 => ResourceRequestExceedsLimit. This distinguishes the
        // scheduler reservation from the independently enforced cap.
        let mut request_only = spec();
        request_only.requests.cpu_millis = Some(500);
        let plan = prepare_environment(&request_only, &caps(IsolationClass::Container)).unwrap();
        assert_eq!(plan.requests.cpu_millis, Some(500));
        assert_eq!(plan.limits.cpu_millis, None);

        let mut bounded = request_only;
        bounded.limits.cpu_millis = Some(1_000);
        assert!(prepare_environment(&bounded, &caps(IsolationClass::Container)).is_ok());

        bounded.requests.cpu_millis = Some(1_001);
        assert_eq!(
            prepare_environment(&bounded, &caps(IsolationClass::Container)),
            Err(PrepareError::ResourceRequestExceedsLimit("cpu"))
        );
    }
}
