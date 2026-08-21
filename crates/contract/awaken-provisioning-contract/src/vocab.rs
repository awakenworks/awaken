//! The provisioning value objects: mounts, env, network, limits, artifacts.
//!
//! All serializable (they cross the config→worker edge as data). Paths are
//! sandbox-absolute or logical references — never host paths (G3).

use serde::{Deserialize, Serialize};

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

/// Required durability semantics for a writable MemoryStore mount.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteConsistency {
    /// The provider may use its ordinary realization, including copy/harvest.
    #[default]
    ProviderDefault,
    /// Every successful filesystem mutation must reach the canonical repository
    /// during execution. Providers must fail admission rather than fall back to
    /// a copy that is harvested only at teardown.
    WriteThroughRequired,
}

/// The one physical backend carrying a rebuildable CacheVolume. A value cannot
/// name both a node path and a Kubernetes claim, so preparation and mounting
/// always address the same storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheVolumeLocation {
    HostPath { path: String },
    PersistentVolumeClaim { claim_name: String },
}

/// Where a mount's content comes from. Logical / content-addressed only — a raw
/// host directory bind is a provider-specific concern expressed via
/// [`super::EnvironmentKind`], not carried here (G3). The enum is deliberately
/// closed: an unknown source cannot cross a persistence or Worker boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
    MemoryStore {
        store_id: String,
        /// Process-local, claim-bound data-plane reference. `None` selects the
        /// embedded repository by logical `store_id`; it is never a Resource id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        materialization_reference: Option<String>,
        #[serde(default)]
        write_consistency: MemoryWriteConsistency,
    },
    /// A **Cache Volume** (ADR-0056): a caller-supplied, node-local, ReadWriteOnce
    /// cache — a warm directory (checkout, build cache) whose bytes have **no truth
    /// authority**. The provider mounts the opaque location **in place** and NEVER
    /// harvests it back: losing it costs a cold rebuild, never data loss, so nothing
    /// whose only copy matters may live here. Warmth and GC are the caller's (product
    /// plane, ADR-0056 §2); awaken only mounts. `key` is the caller's reuse key, opaque
    /// to awaken. Distinct from `MemoryStore` (harvested, authoritative) — this is the
    /// only source that is reused in place and never carried back.
    CacheVolume {
        location: CacheVolumeLocation,
        /// The caller's reuse key, opaque to awaken (never interpreted here).
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
    /// Binary-safe, non-secret per-run content carried to a worker. Unlike
    /// [`MountSource::Inline`], bytes are never coerced through UTF-8. The optional
    /// hash is verified by the realizer before Agent execution.
    InlineBytes {
        contents: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
}

/// A requested mount: source + where it appears + access + lifetime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    pub name: String,
    pub value: EnvValue,
    pub visibility: EnvVisibility,
}

/// The value of an env var. A secret is a **broker reference**, never the bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvValue {
    /// Non-secret literal (`TZ`, `NODE_ENV`, …).
    Inline { value: String },
    /// A secret resolved by a credential broker at realization/egress time.
    Secret { reference: String },
}

impl std::fmt::Debug for EnvValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline { value } => formatter
                .debug_struct("Inline")
                .field("value", value)
                .finish(),
            // A process-secret reference is an ephemeral capability. It is
            // secret-free but still must not be copied into logs/debug dumps.
            Self::Secret { .. } => formatter.write_str("Secret { reference: *** }"),
        }
    }
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

/// Exact package-manager inputs provisioned before any workload process starts.
/// Manager names remain open for provider extensibility; protocol adapters own
/// their closed public enums and providers reject unsupported managers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRequirements {
    #[serde(default)]
    pub managers: std::collections::BTreeMap<String, Vec<String>>,
    /// Frozen Environment identity used only when at least one requirement is
    /// unpinned. This gives "latest" a precise lifecycle: it resolves once for
    /// one Environment snapshot, is cached across that snapshot's Sessions, and
    /// is resolved again after the Environment revision changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_id: Option<String>,
}

impl PackageRequirements {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.managers.values().all(Vec::is_empty)
    }
}

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

// ── Resource requests and limits ──────────────────────────────────────────────

/// Provider-neutral resources reserved for one sandbox by an infrastructure
/// scheduler. Requests are placement demand, not enforcement caps or billing
/// units; [`ResourceLimits`] remains the independent runtime ceiling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequests {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

impl ResourceRequests {
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.cpu_millis.is_some() || self.memory_bytes.is_some() || self.disk_bytes.is_some()
    }

    /// Whether one per-sandbox allocatable ceiling can satisfy this request.
    /// A missing ceiling is unknown and therefore cannot satisfy a set request.
    #[must_use]
    pub fn fits_within(&self, capacity: &ResourceLimits) -> bool {
        self.cpu_millis
            .is_none_or(|required| capacity.cpu_millis.is_some_and(|value| value >= required))
            && self
                .memory_bytes
                .is_none_or(|required| capacity.memory_bytes.is_some_and(|value| value >= required))
            && self
                .disk_bytes
                .is_none_or(|required| capacity.disk_bytes.is_some_and(|value| value >= required))
    }

    /// Return the first request whose explicit limit is smaller.
    #[must_use]
    pub fn first_limit_violation(&self, limits: &ResourceLimits) -> Option<&'static str> {
        if self
            .cpu_millis
            .zip(limits.cpu_millis)
            .is_some_and(|(request, limit)| request > limit)
        {
            return Some("cpu");
        }
        if self
            .memory_bytes
            .zip(limits.memory_bytes)
            .is_some_and(|(request, limit)| request > limit)
        {
            return Some("memory");
        }
        self.disk_bytes
            .zip(limits.disk_bytes)
            .is_some_and(|(request, limit)| request > limit)
            .then_some("disk")
    }
}

/// Enforceable resource caps. A backend that cannot enforce requested limits
/// reports `resource_limits = false` and fails admission rather than ignoring
/// them.
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
    fn cache_volume_location_is_a_closed_sum_type() {
        // Algebraic partition: V1 host path and V2 PVC each round-trip as one
        // location variant; V3 parallel legacy fields and V4 missing location
        // are outside the grammar. No precedence rule can select different bytes.
        let keyed = MountSource::CacheVolume {
            location: CacheVolumeLocation::PersistentVolumeClaim {
                claim_name: "proj-42-cache".into(),
            },
            key: "proj-42".into(),
        };
        let wire = serde_json::to_string(&keyed).unwrap();
        assert!(wire.contains("\"kind\":\"cache_volume\""), "{wire}");
        assert!(!wire.contains("host_path"), "V2 canonical XOR: {wire}");
        assert!(wire.contains("proj-42"));
        assert!(wire.contains("proj-42-cache"), "V2");
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), keyed);

        let unkeyed = MountSource::CacheVolume {
            location: CacheVolumeLocation::HostPath {
                path: "/tmp/warm".into(),
            },
            key: String::new(),
        };
        let wire = serde_json::to_string(&unkeyed).unwrap();
        assert!(!wire.contains("persistent_volume_claim"), "V1: {wire}");
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), unkeyed);

        let legacy_both = r#"{"kind":"cache_volume","host_path":"/legacy/local","persistent_volume_claim":"legacy-pvc","key":"v1"}"#;
        assert!(
            serde_json::from_str::<MountSource>(legacy_both).is_err(),
            "V3"
        );
        assert!(
            serde_json::from_str::<MountSource>(r#"{"kind":"cache_volume","key":"v1"}"#).is_err(),
            "V4"
        );
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
        // Closed-sum coverage: each supported variant serializes with its tag and
        // reparses equal across a distributed Worker boundary.
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
                materialization_reference: Some("claim-bound".into()),
                write_consistency: MemoryWriteConsistency::ProviderDefault,
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
    fn resource_requests_require_every_set_axis_to_fit_an_explicit_ceiling() {
        // Cause/effect decision table: C1 request axis is set; C2 matching ceiling
        // exists; C3 ceiling >= request. R1 !C1 => fit; R2 C1+C2+C3 => fit;
        // R3 C1+!C2 and R4 C1+C2+!C3 => reject. CPU, memory and disk share the
        // same conjunctive rule, exercised once each below.
        let requests = ResourceRequests {
            cpu_millis: Some(500),
            memory_bytes: Some(1024),
            disk_bytes: Some(2048),
        };
        assert!(requests.fits_within(&ResourceLimits {
            cpu_millis: Some(500),
            memory_bytes: Some(2048),
            disk_bytes: Some(4096),
            pids: None,
        }));
        assert!(!requests.fits_within(&ResourceLimits {
            cpu_millis: Some(499),
            memory_bytes: Some(2048),
            disk_bytes: Some(4096),
            pids: None,
        }));
        assert!(!requests.fits_within(&ResourceLimits {
            cpu_millis: Some(500),
            memory_bytes: None,
            disk_bytes: Some(4096),
            pids: None,
        }));
        assert!(!requests.fits_within(&ResourceLimits {
            cpu_millis: Some(500),
            memory_bytes: Some(2048),
            disk_bytes: Some(2047),
            pids: None,
        }));
        assert!(ResourceRequests::default().fits_within(&ResourceLimits::default()));
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

    #[test]
    fn an_inline_mount_source_round_trips_with_its_tag() {
        // ADR-0057 `Inline`: small non-secret per-run derived bytes carried in the spec.
        // The other-kind round-trip test skips it (and CacheVolume has its own), so its
        // `kind: inline` wire form was uncovered despite crossing to a remote worker.
        let src = MountSource::Inline {
            contents: "[plugin]\nname = \"x\"\n".into(),
        };
        let wire = serde_json::to_string(&src).unwrap();
        assert!(wire.contains("\"kind\":\"inline\""), "{wire}");
        assert!(
            wire.contains("[plugin]"),
            "the derived bytes ride the wire: {wire}"
        );
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), src);
    }

    #[test]
    fn inline_bytes_round_trip_without_utf8_coercion() {
        let src = MountSource::InlineBytes {
            contents: vec![0, 0xff, 0x80, b'\n'],
            content_hash: Some("hash".into()),
        };
        let wire = serde_json::to_string(&src).unwrap();
        assert!(wire.contains("\"kind\":\"inline_bytes\""), "{wire}");
        assert_eq!(serde_json::from_str::<MountSource>(&wire).unwrap(), src);
    }

    #[test]
    fn mount_source_grammar_is_closed_and_strict() {
        // Grammar partition: G1 known exact variant -> accept; G2 unknown kind,
        // G3 missing discriminant, G4 extra field -> reject at the contract edge.
        let known = MountSource::Inline {
            contents: "content".into(),
        };
        assert_eq!(
            serde_json::from_value::<MountSource>(serde_json::to_value(&known).unwrap()).unwrap(),
            known,
            "G1"
        );
        for (rule, value) in [
            ("G2", serde_json::json!({"kind": "future", "value": 1})),
            ("G3", serde_json::json!({"contents": "content"})),
            (
                "G4",
                serde_json::json!({"kind": "inline", "contents": "content", "extra": true}),
            ),
        ] {
            assert!(
                serde_json::from_value::<MountSource>(value).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_secret_env_value_round_trips_as_a_reference_only() {
        // G3 core: a secret env var is a BROKER REFERENCE on the wire, never the bytes.
        // Pins the `kind: secret` tag (the Inline vs Secret discriminant the realizer /
        // egress substitution keys on).
        let inline = EnvValue::Inline {
            value: "UTC".into(),
        };
        let iw = serde_json::to_string(&inline).unwrap();
        assert!(iw.contains("\"kind\":\"inline\""), "{iw}");
        assert_eq!(serde_json::from_str::<EnvValue>(&iw).unwrap(), inline);

        let secret = EnvValue::Secret {
            reference: "broker://anthropic/key".into(),
        };
        let sw = serde_json::to_string(&secret).unwrap();
        assert!(sw.contains("\"kind\":\"secret\""), "{sw}");
        assert!(
            sw.contains("broker://anthropic/key"),
            "the reference, not bytes: {sw}"
        );
        assert!(
            !sw.contains("\"value\""),
            "a secret carries no inline value: {sw}"
        );
        assert_eq!(serde_json::from_str::<EnvValue>(&sw).unwrap(), secret);
    }

    #[test]
    fn env_visibility_egress_only_round_trips_as_its_snake_case_tag() {
        // `EgressOnly` (the placeholder-until-egress-substitution mode) must survive the
        // wire so `prepare_environment` can gate it against `secret_egress_substitution`.
        let var = EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Secret {
                reference: "broker://k".into(),
            },
            visibility: EnvVisibility::EgressOnly,
        };
        let wire = serde_json::to_string(&var).unwrap();
        assert!(wire.contains("\"visibility\":\"egress_only\""), "{wire}");
        assert_eq!(serde_json::from_str::<EnvVar>(&wire).unwrap(), var);

        // The `Process` counterpart (the only guarantee a local backend gives).
        let proc = EnvVisibility::Process;
        let pw = serde_json::to_string(&proc).unwrap();
        assert_eq!(pw, "\"process\"");
        assert_eq!(serde_json::from_str::<EnvVisibility>(&pw).unwrap(), proc);
    }
}
