//! The sandbox spec (what to realize), the declared environment config, and the
//! process launch input.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sandbox::IsolationClass;
use crate::vocab::{EnvVar, MountRequirement, NetworkPolicy, ResourceLimits};

/// How a process's standard streams are wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stdio {
    /// Inherit the host's streams (dev/CLI).
    Inherit,
    /// Pipe streams back to the caller (a protocol bridge, e.g. ACP over stdio).
    /// The concrete stream handles are exposed by the provider's `ProcessHandle`
    /// type, not by the neutral trait (which would have to bind an IO abstraction).
    Piped,
    /// Discard.
    Null,
}

/// A process to launch **inside** the sandbox. Isolation is enforced by the OS
/// regardless of what the process does — this is the tool-transparent primitive
/// (`["claude","--acp"]`, `["bash","-lc",…]`, `["python","x.py"]` all work the
/// same way).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub argv: Vec<String>,
    /// Sandbox-absolute working directory. Empty = the environment default
    /// (typically `/workspace`).
    #[serde(default)]
    pub cwd: String,
    /// Extra env merged onto the environment's base env for this process.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    pub stdio: Stdio,
}

impl Command {
    /// A command with default cwd, no extra env, inherited stdio.
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: String::new(),
            env: Vec::new(),
            stdio: Stdio::Inherit,
        }
    }
}

/// The request to realize a sandbox environment. `extra` is forward-compatible,
/// provider-specific data (image tag, seccomp profile, …) opaque to this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Session/thread scope — the isolation boundary and the artifact key.
    pub scope: String,
    /// Minimum isolation the caller requires; the provider must meet or exceed it.
    pub isolation: IsolationClass,
    #[serde(default)]
    pub mounts: Vec<MountRequirement>,
    /// Base env applied to every process launched in the sandbox.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    pub network: NetworkPolicy,
    /// Sandbox-absolute directory the agent writes artifacts to (e.g.
    /// `/mnt/session/outputs`).
    pub outputs_path: String,
    #[serde(default)]
    pub limits: ResourceLimits,
    /// Optional dead-man's-switch: the owner must `renew_lease` within this window
    /// or the sandbox self-reaps. `None` = no lease (a local child dies with its
    /// parent anyway); set it for remote sandboxes that outlive the owning host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

/// The **declared** control-plane environment config (persisted + admitted),
/// distinct from a realized live environment. Mirrors the shapes both source
/// repos converged on; the realizer maps a `kind` to an [`IsolationClass`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// Soft boundary (HOME/TMPDIR redirect); no OS isolation.
    Scope,
    /// OS-namespace isolation (bwrap on Linux, sandbox-exec on macOS).
    Sandbox,
    /// Private root filesystem.
    IsolatedRoot {
        base: RootfsSource,
        /// When true the base is writable, which forces single-active use.
        writable_base: bool,
    },
    /// Container image (OCI reference).
    Image { reference: String },
    /// Host directory override — no isolation, cwd selection only.
    LocalDir { path_template: String },
}

/// Where an [`EnvironmentKind::IsolatedRoot`] base comes from — a reference, never
/// a resolved host path (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum RootfsSource {
    Dir { path_template: String },
    Tarball { reference: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip `value` and assert its serialized form carries `tag` verbatim.
    /// Pins the exact wire tag (the config→worker contract) alongside reparse-equality.
    fn round_trips_with_tag<T>(value: &T, tag: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let wire = serde_json::to_string(value).unwrap();
        assert!(wire.contains(tag), "missing tag {tag} in {wire}");
        assert_eq!(&serde_json::from_str::<T>(&wire).unwrap(), value, "{wire}");
    }

    #[test]
    fn every_environment_kind_round_trips_with_its_exact_kind_tag() {
        // EnvironmentKind crosses config→worker as the declared environment shape; its
        // `kind` discriminant is the contract both source repos converged on. Untested
        // until now despite being the realizer's dispatch key — pin every tag verbatim.
        round_trips_with_tag(&EnvironmentKind::Scope, "\"kind\":\"scope\"");
        round_trips_with_tag(&EnvironmentKind::Sandbox, "\"kind\":\"sandbox\"");
        round_trips_with_tag(
            &EnvironmentKind::IsolatedRoot {
                base: RootfsSource::Dir {
                    path_template: "/rootfs/{scope}".into(),
                },
                writable_base: true,
            },
            "\"kind\":\"isolated_root\"",
        );
        round_trips_with_tag(
            &EnvironmentKind::Image {
                reference: "registry.io/img:1".into(),
            },
            "\"kind\":\"image\"",
        );
        round_trips_with_tag(
            &EnvironmentKind::LocalDir {
                path_template: "/home/{user}/work".into(),
            },
            "\"kind\":\"local_dir\"",
        );
    }

    #[test]
    fn isolated_root_nests_and_preserves_its_rootfs_source_and_flag() {
        // The one composite kind: its `base` is a nested tagged `RootfsSource` and
        // `writable_base` must survive the round-trip (it forces single-active use).
        let kind = EnvironmentKind::IsolatedRoot {
            base: RootfsSource::Tarball {
                reference: "blob://base.tar".into(),
            },
            writable_base: false,
        };
        let wire = serde_json::to_string(&kind).unwrap();
        assert!(wire.contains("\"source\":\"tarball\""), "{wire}");
        assert!(wire.contains("\"writable_base\":false"), "{wire}");
        assert_eq!(
            serde_json::from_str::<EnvironmentKind>(&wire).unwrap(),
            kind
        );
    }

    #[test]
    fn every_rootfs_source_round_trips_with_its_exact_source_tag() {
        // RootfsSource is a *reference* (G3: never a resolved host path); its `source`
        // tag is the discriminant the realizer maps to a base image/dir.
        round_trips_with_tag(
            &RootfsSource::Dir {
                path_template: "/var/lib/rootfs/{scope}".into(),
            },
            "\"source\":\"dir\"",
        );
        round_trips_with_tag(
            &RootfsSource::Tarball {
                reference: "oci://base:latest".into(),
            },
            "\"source\":\"tarball\"",
        );
    }
}
