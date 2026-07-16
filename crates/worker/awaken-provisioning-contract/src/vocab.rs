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
    /// A **Cache Volume** (ADR-0056): a caller-supplied, node-local, ReadWriteOnce
    /// cache — a warm directory (checkout, build cache) whose bytes have **no truth
    /// authority**. The provider mounts the opaque `host_path` **in place** and NEVER
    /// harvests it back: losing it costs a cold rebuild, never data loss, so nothing
    /// whose only copy matters may live here. Warmth and GC are the caller's (product
    /// plane, ADR-0056 §2); awaken only mounts. `key` is the caller's reuse key, opaque
    /// to awaken. Distinct from `MemoryStore` (harvested, authoritative) — this is the
    /// only source that is reused in place and never carried back.
    CacheVolume {
        /// The caller-owned persistent path to mount in place. Opaque to awaken — it
        /// neither keeps it warm nor reclaims it.
        host_path: String,
        /// The caller's reuse key, opaque to awaken (never interpreted here).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key: String,
    },
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
    /// **Inline ephemeral content** (ADR-0057): small, **non-secret**, per-run *derived*
    /// bytes carried in the spec itself — e.g. a launched CLI's `config.toml` projected
    /// from `plugin_config`. Distinct from `File`/`Resource` (durable, content-addressed,
    /// store-resolved): this is a **projection**, so it is
    /// - **one-way**: written in at realize, **NEVER harvested/written back** (like
    ///   `CacheVolume`'s never-harvest, but for derived config) — harvesting a projection
    ///   would let a stale render impersonate the authoritative source;
    /// - **regenerable**: losing it costs a re-derive, never data loss;
    /// - **non-secret**: a credential stays a broker reference (`Secret`/`EnvValue::Secret`),
    ///   never inline bytes (G3).
    ///
    /// Self-contained (no store lookup), so it is portable to a remote worker. Emitted
    /// paired with `MountAccess::ReadOnly` + `MountLifetime::PerRun`; the never-harvest
    /// property is intrinsic — it is neither `Secret` (writeback) nor `MemoryStore`
    /// (harvested), so no realizer copies it back.
    Inline { contents: String },
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

    /// Whether this policy restricts egress at all — anything other than
    /// `Unrestricted`. A neutral fact about the policy; how an enforcer realizes it
    /// (an on/off sandbox vs an allowlist-capable gateway) is the caller's concern.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
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

impl ResourceLimits {
    /// Whether any cap is requested. A backend with `resource_limits = false` must
    /// fail closed on a spec whose limits are set, never silently ignore them.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.cpu_millis.is_some()
            || self.memory_bytes.is_some()
            || self.pids.is_some()
            || self.disk_bytes.is_some()
    }
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
    fn is_restricted_is_true_for_all_but_unrestricted() {
        assert!(!NetworkPolicy::Unrestricted.is_restricted());
        assert!(
            NetworkPolicy::Allowlist {
                hosts: vec!["api.example.com".into()],
            }
            .is_restricted()
        );
        assert!(NetworkPolicy::None.is_restricted());
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
    fn cache_volume_round_trips_and_omits_an_empty_key() {
        // ADR-0056 §3: the Cache Volume is a first-class mount kind on the wire
        // (`kind: cache_volume`), carrying the caller-owned host path; an empty reuse
        // key is omitted (skip_serializing_if) so an unkeyed cache volume is compact.
        let keyed = MountSource::CacheVolume {
            host_path: "/var/cache/awaken/proj-42".into(),
            key: "proj-42".into(),
        };
        let wire = serde_json::to_string(&keyed).unwrap();
        assert!(wire.contains("\"kind\":\"cache_volume\""), "{wire}");
        assert!(wire.contains("/var/cache/awaken/proj-42"));
        assert!(wire.contains("proj-42"));
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), keyed);

        let unkeyed = MountSource::CacheVolume {
            host_path: "/tmp/warm".into(),
            key: String::new(),
        };
        let wire = serde_json::to_string(&unkeyed).unwrap();
        assert!(!wire.contains("\"key\""), "an empty key is omitted: {wire}");
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), unkeyed);
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

    #[test]
    fn network_policy_rank_orders_unrestricted_below_allowlist_below_none() {
        // The ranked restrictiveness a provider admits against (satisfies uses it).
        assert_eq!(NetworkPolicy::Unrestricted.rank(), 0);
        assert_eq!(
            NetworkPolicy::Allowlist {
                hosts: vec!["h".into()]
            }
            .rank(),
            1
        );
        assert_eq!(NetworkPolicy::None.rank(), 2);
        assert!(
            NetworkPolicy::Unrestricted.rank() < NetworkPolicy::Allowlist { hosts: vec![] }.rank()
        );
        assert!(NetworkPolicy::Allowlist { hosts: vec![] }.rank() < NetworkPolicy::None.rank());
    }

    #[test]
    fn every_mount_source_kind_round_trips_through_the_wire() {
        // Forward-compat: each variant serializes with its tag and reparses equal —
        // the seam a distributed provider relies on (Resource/MemoryStore/Other were
        // previously only covered for Secret/File).
        for src in [
            MountSource::File {
                file_id: "f".into(),
                content_hash: Some("h".into()),
            },
            MountSource::Resource {
                resource_id: "r".into(),
                content_hash: None,
            },
            MountSource::MemoryStore {
                store_id: "s".into(),
            },
            MountSource::Secret {
                reference: "broker://k".into(),
                content_hash: None,
            },
        ] {
            let wire = serde_json::to_string(&src).unwrap();
            assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), src);
        }
    }

    #[test]
    fn a_session_lifetime_secret_is_not_a_writeback() {
        // Only Durable+ReadWrite is a writeback; Session lifetime (between PerRun and
        // Durable) is not — the boundary the other writeback tests didn't cover.
        let m = req(
            MountSource::Secret {
                reference: "r".into(),
                content_hash: None,
            },
            MountAccess::ReadWrite,
            MountLifetime::Session,
        );
        assert!(!m.is_secret_writeback());
    }

    #[test]
    fn resource_limits_is_set_flips_on_any_single_field() {
        assert!(!ResourceLimits::default().is_set());
        for limits in [
            ResourceLimits {
                cpu_millis: Some(1),
                ..Default::default()
            },
            ResourceLimits {
                memory_bytes: Some(1),
                ..Default::default()
            },
            ResourceLimits {
                pids: Some(1),
                ..Default::default()
            },
            ResourceLimits {
                disk_bytes: Some(1),
                ..Default::default()
            },
        ] {
            assert!(limits.is_set(), "any single cap set makes is_set() true");
        }
    }

    #[test]
    fn a_bind_realization_round_trips() {
        // `Realization::Bind` is produced by the namespace/container tiers but was
        // never constructed in a contract test; pin its wire form here.
        let m = RealizedMount {
            mount_id: "in".into(),
            mount_path: "/data/in".into(),
            access: MountAccess::ReadOnly,
            realization: Realization::Bind,
            content_hash: Some("h".into()),
        };
        let wire = serde_json::to_string(&m).unwrap();
        assert!(wire.contains("\"realization\":\"bind\""));
        assert_eq!(serde_json::from_str::<RealizedMount>(&wire).unwrap(), m);
    }

    #[test]
    fn reserved_env_keys_are_the_runtime_owned_set() {
        // The full guard set admission rejects (previously only PATH was exercised).
        for key in ["PATH", "HOME", "AWAKEN_PROJECT_DIR", "AWAKEN_OUTPUTS_DIR"] {
            assert!(RESERVED_ENV_KEYS.contains(&key), "{key} is runtime-owned");
        }
    }
}
