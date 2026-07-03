//! The provisioning value objects: mounts, env, network, limits, artifacts.
//!
//! All serializable (they cross the config→worker edge as data). Paths are
//! sandbox-absolute or logical references — never host paths (G3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Mounts ──────────────────────────────────────────────────────────────────

/// Whether a mount is writable by processes in the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAccess {
    /// Read-only: input data. Writes fail at the OS level (e.g. `EROFS`).
    ReadOnly,
    /// Read-write: a workspace or an output area.
    ReadWrite,
}

/// How long a mount's realized content survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountLifetime {
    /// Dies with the sandbox instance.
    PerRun,
    /// Survives across runs of the same session.
    Session,
    /// Persistent workspace-scoped store.
    Durable,
}

/// Where a mount's content comes from. Logical / content-addressed only — a raw
/// host directory bind is a provider-specific concern expressed via
/// [`super::EnvironmentKind`], not carried here (G3). `Other` keeps the wire
/// forward-compatible so a distributed provider can add kinds without a break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MountSource {
    /// An immutable blob from the file store (ADR-0038 `File`).
    File {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    /// A provisioned resource file.
    Resource {
        resource_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    /// A persistent memory store, mounted for read/write (typically via FUSE).
    MemoryStore { store_id: String },
    /// Forward-compat escape: an unknown source a newer provider understands.
    Other(Value),
}

/// A requested mount: source + where it appears + access + lifetime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountRequirement {
    pub mount_id: String,
    pub source: MountSource,
    /// Sandbox-absolute path, e.g. `/workspace/data.csv`.
    pub mount_path: String,
    pub access: MountAccess,
    pub lifetime: MountLifetime,
    /// A required mount that fails to realize aborts the whole environment
    /// (all-or-nothing); an optional one is skipped.
    pub required: bool,
}

/// How a provider realized a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Realization {
    /// Bind-mounted (namespace/container).
    Bind,
    /// FUSE-backed (e.g. a write-through memory store).
    Fuse,
    /// A read-only fan-out copy on the sandbox filesystem.
    Copy,
}

/// A realized mount reference the host receives — logical path + content hash,
/// never a host path (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealizedMount {
    pub mount_id: String,
    pub mount_path: String,
    pub access: MountAccess,
    pub realization: Realization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

// ── Environment variables ─────────────────────────────────────────────────────

/// Env keys the runtime owns; a declaration may not set them (admission rejects).
pub const RESERVED_ENV_KEYS: &[&str] =
    &["PATH", "HOME", "AWAKEN_PROJECT_DIR", "AWAKEN_OUTPUTS_DIR"];

/// An environment variable injected into every process in the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: EnvValue,
    pub visibility: EnvVisibility,
}

/// The value of an env var. A secret is a **broker reference**, never the bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvValue {
    /// Non-secret literal (`TZ`, `NODE_ENV`, …).
    Inline { value: String },
    /// A secret resolved by a credential broker at realization/egress time.
    Secret { reference: String },
}

/// Where an injected value is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvVisibility {
    /// The process sees the real value (the only guarantee a local backend gives).
    Process,
    /// The sandbox sees a placeholder; the real value is substituted at network
    /// egress. Requires a provider with `secret_egress_substitution`.
    EgressOnly,
}

// ── Network ───────────────────────────────────────────────────────────────────

/// Egress policy for the sandbox. Ranked so a provider admits a request only when
/// it can enforce a policy at least as restrictive as the one asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Full egress (the agent still needs to reach the model endpoint).
    Unrestricted,
    /// Deny-by-default; only these hosts are reachable.
    Allowlist { hosts: Vec<String> },
    /// No egress.
    None,
}

impl NetworkPolicy {
    /// Restrictiveness rank: `Unrestricted` < `Allowlist` < `None`.
    #[must_use]
    pub fn rank(&self) -> u8 {
        match self {
            NetworkPolicy::Unrestricted => 0,
            NetworkPolicy::Allowlist { .. } => 1,
            NetworkPolicy::None => 2,
        }
    }
}

// ── Resource limits ───────────────────────────────────────────────────────────

/// Best-effort resource caps. A backend that cannot enforce a field ignores it
/// (and reports `resource_limits = false` in its capabilities).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

// ── Artifacts (sandbox → host) ────────────────────────────────────────────────

/// A file the agent wrote under the outputs path, retrievable by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Content-addressed id (stable, dedup-friendly).
    pub id: String,
    /// Sandbox-absolute path under the environment's `outputs_path`.
    pub path: String,
    pub size_bytes: u64,
    pub content_hash: String,
}
