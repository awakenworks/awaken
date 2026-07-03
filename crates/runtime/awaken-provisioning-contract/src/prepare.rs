//! `prepare_environment` — the pure planning step between a [`SandboxSpec`] and a
//! provider's `create`. It validates the spec against a backend's
//! [`SandboxCapabilities`] and returns a normalized [`EnvironmentPlan`], failing
//! closed when the backend cannot honor a requested guarantee. Pure and
//! deterministic: no I/O, safe to run at admission time and to unit-test.

use crate::sandbox::SandboxCapabilities;
use crate::spec::SandboxSpec;
use crate::vocab::{
    EnvVar, EnvVisibility, MountAccess, MountRequirement, NetworkPolicy, RESERVED_ENV_KEYS,
};

/// A validated, normalized plan ready to hand to [`crate::SandboxProvider::create`].
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentPlan {
    pub scope: String,
    pub mounts: Vec<MountRequirement>,
    pub env: Vec<EnvVar>,
    pub network: NetworkPolicy,
    pub outputs_path: String,
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
    #[error("env key {0:?} is reserved by the runtime")]
    ReservedEnvKey(String),
    #[error("outputs_path must be an absolute sandbox path")]
    OutputsPathNotAbsolute,
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

    // Read-only mounts require OS-enforced read-only.
    if !caps.enforced_readonly {
        if let Some(m) = spec
            .mounts
            .iter()
            .find(|m| m.access == MountAccess::ReadOnly)
        {
            return Err(PrepareError::ReadOnlyUnsupported(m.mount_id.clone()));
        }
    }

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

    Ok(EnvironmentPlan {
        scope: spec.scope.clone(),
        mounts: spec.mounts.clone(),
        env: spec.env.clone(),
        network: spec.network.clone(),
        outputs_path: spec.outputs_path.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::IsolationClass;
    use crate::vocab::{EnvValue, MountLifetime, MountSource};

    fn caps(isolation: IsolationClass) -> SandboxCapabilities {
        SandboxCapabilities {
            isolation,
            tool_transparent: isolation >= IsolationClass::Namespace,
            path_fidelity: isolation >= IsolationClass::Namespace,
            enforced_readonly: isolation >= IsolationClass::Namespace,
            network_isolation: isolation >= IsolationClass::Namespace,
            secret_egress_substitution: isolation == IsolationClass::Container,
            resource_limits: isolation >= IsolationClass::Namespace,
            custom_rootfs: isolation == IsolationClass::Container,
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
            network: NetworkPolicy::Allowlist {
                hosts: vec!["api.anthropic.com".into()],
            },
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[test]
    fn prepares_against_a_capable_backend() {
        assert!(prepare_environment(&spec(), &caps(IsolationClass::Namespace)).is_ok());
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

    #[test]
    fn rejects_network_policy_without_isolation() {
        let mut c = caps(IsolationClass::Namespace);
        c.network_isolation = false;
        assert_eq!(
            prepare_environment(&spec(), &c),
            Err(PrepareError::NetworkIsolationUnsupported)
        );
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
}
