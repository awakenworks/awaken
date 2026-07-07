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
    /// A file-materialized credential (ADR-0041 amendment). The provider writes the
    /// broker-resolved secret to `mount_path`, honoring the requirement's
    /// [`MountAccess`]/[`MountLifetime`]; a `ReadWrite` + `Durable` secret mount is
    /// written back to the broker after the run (CLI agents that refresh their own
    /// auth file). Only the broker `reference` crosses the seam — never the bytes (G3).
    Secret {
        reference: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
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

impl MountRequirement {
    /// A secret credential the provider must write back after the run: a durable,
    /// writable [`MountSource::Secret`] (an agent that refreshes its own auth file).
    #[must_use]
    pub fn is_secret_writeback(&self) -> bool {
        matches!(self.source, MountSource::Secret { .. })
            && self.access == MountAccess::ReadWrite
            && self.lifetime == MountLifetime::Durable
    }
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

    /// Whether a *binary* (on/off) egress enforcer — one that can only allow or
    /// deny all egress, e.g. bwrap `--unshare-net` — must deny to satisfy this
    /// policy. `Unrestricted` allows; `Allowlist` and `None` deny — an allowlist
    /// cannot be honored by an on/off enforcer, so it fails closed to no egress.
    #[must_use]
    pub fn denies_under_binary_enforcer(&self) -> bool {
        !matches!(self, NetworkPolicy::Unrestricted)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_enforcer_denies_all_but_unrestricted() {
        assert!(!NetworkPolicy::Unrestricted.denies_under_binary_enforcer());
        assert!(
            NetworkPolicy::Allowlist {
                hosts: vec!["api.example.com".into()],
            }
            .denies_under_binary_enforcer()
        );
        assert!(NetworkPolicy::None.denies_under_binary_enforcer());
    }

    fn req(source: MountSource, access: MountAccess, lifetime: MountLifetime) -> MountRequirement {
        MountRequirement {
            mount_id: "cred".into(),
            source,
            mount_path: "/home/agent/.config/auth.json".into(),
            access,
            lifetime,
            required: true,
        }
    }

    #[test]
    fn secret_source_round_trips_as_a_reference_only() {
        let src = MountSource::Secret {
            reference: "broker://anthropic/key".into(),
            content_hash: None,
        };
        let wire = serde_json::to_string(&src).unwrap();
        assert!(wire.contains("\"kind\":\"secret\""));
        assert!(wire.contains("broker://anthropic/key"));
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), src);
    }

    #[test]
    fn durable_writable_secret_is_a_writeback() {
        let m = req(
            MountSource::Secret {
                reference: "r".into(),
                content_hash: None,
            },
            MountAccess::ReadWrite,
            MountLifetime::Durable,
        );
        assert!(m.is_secret_writeback());
    }

    #[test]
    fn readonly_or_ephemeral_secret_is_not_a_writeback() {
        let ro = req(
            MountSource::Secret {
                reference: "r".into(),
                content_hash: None,
            },
            MountAccess::ReadOnly,
            MountLifetime::Durable,
        );
        let per_run = req(
            MountSource::Secret {
                reference: "r".into(),
                content_hash: None,
            },
            MountAccess::ReadWrite,
            MountLifetime::PerRun,
        );
        assert!(!ro.is_secret_writeback());
        assert!(!per_run.is_secret_writeback());
    }

    #[test]
    fn non_secret_source_is_never_a_writeback() {
        let m = req(
            MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            MountAccess::ReadWrite,
            MountLifetime::Durable,
        );
        assert!(!m.is_secret_writeback());
    }
}
