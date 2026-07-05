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
