//! The sandbox spec (what to realize), the declared environment config, and the
//! process launch input.

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::sandbox::IsolationClass;
use crate::sandbox::{SandboxError, SecretBroker};
use crate::vocab::{
    EnvValue, EnvVar, EnvVisibility, MountRequirement, NetworkPolicy, PackageRequirements,
    ResourceLimits, ResourceRequests,
};

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

/// One environment value after the final process boundary has resolved every
/// broker reference. Unlike [`EnvValue`], this value is deliberately not
/// serializable: plaintext can reach only the OS/container launch adapter.
#[derive(Clone)]
pub enum MaterializedEnvValue {
    Inline(String),
    Secret(awaken_agent_contract::RedactedString),
}

impl MaterializedEnvValue {
    /// Expose the value only to the concrete process-launch adapter.
    #[must_use]
    pub fn expose(&self) -> &str {
        match self {
            Self::Inline(value) => value,
            Self::Secret(value) => value.expose_secret(),
        }
    }

    #[must_use]
    pub fn is_secret(&self) -> bool {
        matches!(self, Self::Secret(_))
    }
}

impl std::fmt::Debug for MaterializedEnvValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(value) => formatter.debug_tuple("Inline").field(value).finish(),
            Self::Secret(_) => formatter.write_str("Secret(***)"),
        }
    }
}

/// One process environment entry after last-mile materialization.
#[derive(Debug, Clone)]
pub struct MaterializedEnvVar {
    pub name: String,
    pub value: MaterializedEnvValue,
}

/// A launch-ready command. It has no Serde implementation, and secret values use
/// the repository-wide redacted/zeroizing wrapper. Providers may hand it to an OS
/// API but cannot accidentally put it back on a planning wire.
#[derive(Debug, Clone)]
pub struct MaterializedCommand {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<MaterializedEnvVar>,
    pub stdio: Stdio,
}

impl MaterializedCommand {
    /// Construct an already-materialized command with no environment. Intended
    /// for concrete adapter tests and non-secret infrastructure commands; normal
    /// callers use [`materialize_process_command`].
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: String::new(),
            env: Vec::new(),
            stdio: Stdio::Inherit,
        }
    }
}

/// Resolve the final effective process environment through the one neutral
/// broker seam. Command entries override base entries by name before resolution,
/// so an overridden secret is never unnecessarily opened.
pub async fn materialize_process_command(
    base_env: &[EnvVar],
    command: Command,
    broker: Option<&Arc<dyn SecretBroker>>,
) -> Result<MaterializedCommand, SandboxError> {
    let mut effective = BTreeMap::<String, EnvVar>::new();
    for var in base_env.iter().chain(&command.env) {
        effective.insert(var.name.clone(), var.clone());
    }

    let mut env = Vec::with_capacity(effective.len());
    for (_, var) in effective {
        let value = match var.value {
            EnvValue::Inline { value } => MaterializedEnvValue::Inline(value),
            EnvValue::Secret { .. } if var.visibility == EnvVisibility::EgressOnly => {
                return Err(SandboxError::new(format!(
                    "egress-only secret `{}` cannot be materialized into a process",
                    var.name
                )));
            }
            EnvValue::Secret { reference } => {
                let broker = broker.ok_or_else(|| {
                    SandboxError::new(format!(
                        "process secret `{}` has no credential broker",
                        var.name
                    ))
                })?;
                let bytes = broker.materialize_process(&reference).await?;
                let value = String::from_utf8(bytes).map_err(|_| {
                    SandboxError::new(format!("process secret `{}` is not valid UTF-8", var.name))
                })?;
                MaterializedEnvValue::Secret(awaken_agent_contract::RedactedString::new(value))
            }
        };
        env.push(MaterializedEnvVar {
            name: var.name,
            value,
        });
    }

    Ok(MaterializedCommand {
        argv: command.argv,
        cwd: command.cwd,
        env,
        stdio: command.stdio,
    })
}

#[cfg(test)]
mod process_secret_tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    struct DecisionBroker {
        values: BTreeMap<String, Result<Vec<u8>, &'static str>>,
    }

    #[async_trait]
    impl SecretBroker for DecisionBroker {
        async fn materialize(&self, reference: &str) -> Result<Vec<u8>, SandboxError> {
            self.values
                .get(reference)
                .cloned()
                .unwrap_or(Err("missing"))
                .map_err(SandboxError::new)
        }

        async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, SandboxError> {
            self.materialize(reference).await
        }

        async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), SandboxError> {
            Err(SandboxError::new("not supported"))
        }
    }

    fn secret(visibility: EnvVisibility) -> Command {
        let mut command = Command::new(["agent"]);
        command.env.push(EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Secret {
                reference: "lease://exact".into(),
            },
            visibility,
        });
        command
    }

    /// Process-secret cause graph:
    ///
    /// C1 visibility is Process -> C2 broker installed -> C3 exact reference
    /// resolves -> C4 material is UTF-8 -> E1 non-serializable launch value.
    /// Any failed cause terminates before launch with E2 and no plaintext in Debug.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Result |
    /// |---|---|---|---|---|---|
    /// | P1 | T | T | T | T | materialized secret |
    /// | P2 | T | F | - | - | missing broker |
    /// | P3 | T | T | F | - | broker failure |
    /// | P4 | T | T | T | F | invalid UTF-8 |
    /// | P5 | F | - | - | - | egress-only rejected |
    #[tokio::test]
    async fn process_secret_decision_table_fails_closed_and_redacts_material() {
        struct Rule {
            id: &'static str,
            visibility: EnvVisibility,
            broker: Option<Result<Vec<u8>, &'static str>>,
            succeeds: bool,
        }
        for rule in [
            Rule {
                id: "P1",
                visibility: EnvVisibility::Process,
                broker: Some(Ok(b"decision-table-secret".to_vec())),
                succeeds: true,
            },
            Rule {
                id: "P2",
                visibility: EnvVisibility::Process,
                broker: None,
                succeeds: false,
            },
            Rule {
                id: "P3",
                visibility: EnvVisibility::Process,
                broker: Some(Err("planned rejection")),
                succeeds: false,
            },
            Rule {
                id: "P4",
                visibility: EnvVisibility::Process,
                broker: Some(Ok(vec![0xff])),
                succeeds: false,
            },
            Rule {
                id: "P5",
                visibility: EnvVisibility::EgressOnly,
                broker: Some(Ok(b"must-not-open".to_vec())),
                succeeds: false,
            },
        ] {
            let broker: Option<Arc<dyn SecretBroker>> = rule.broker.map(|result| {
                Arc::new(DecisionBroker {
                    values: [("lease://exact".into(), result)].into_iter().collect(),
                }) as Arc<dyn SecretBroker>
            });
            let result =
                materialize_process_command(&[], secret(rule.visibility), broker.as_ref()).await;
            assert_eq!(result.is_ok(), rule.succeeds, "{} verdict", rule.id);
            let debug = format!("{result:?}");
            assert!(
                !debug.contains("decision-table-secret") && !debug.contains("must-not-open"),
                "{} debug must be secret-free: {debug}",
                rule.id
            );
            if let Ok(command) = result {
                assert_eq!(command.env.len(), 1, "{} one env", rule.id);
                assert!(command.env[0].value.is_secret(), "{} typed secret", rule.id);
                assert_eq!(command.env[0].value.expose(), "decision-table-secret");
            }
        }
    }

    #[tokio::test]
    async fn command_override_prevents_opening_an_overridden_base_secret() {
        let base = [EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Secret {
                reference: "lease://unused".into(),
            },
            visibility: EnvVisibility::Process,
        }];
        let mut command = Command::new(["agent"]);
        command.env.push(EnvVar {
            name: "API_KEY".into(),
            value: EnvValue::Inline {
                value: "public-override".into(),
            },
            visibility: EnvVisibility::Process,
        });
        let broker: Arc<dyn SecretBroker> = Arc::new(DecisionBroker {
            values: BTreeMap::new(),
        });
        let materialized = materialize_process_command(&base, command, Some(&broker))
            .await
            .unwrap();
        assert_eq!(materialized.env[0].value.expose(), "public-override");
        assert!(!materialized.env[0].value.is_secret());
    }
}

/// Whether the writable filesystem must survive the process that realizes it.
///
/// Retention is fail-safe by default: an older serialized request that predates
/// this field must not silently lose Session state. Deliberately prompt-free
/// capability probes and other disposable housekeeping work opt into
/// [`Self::Ephemeral`] explicitly.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemContinuity {
    #[default]
    Retained,
    Ephemeral,
}

/// The request to realize a sandbox environment.
///
/// Every creation-time input is part of this closed contract. Provider-specific
/// free-form data is deliberately forbidden: it bypasses admission, produces
/// different interpretations across providers, and makes capacity fingerprints
/// unstable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    /// Session/thread scope — the isolation boundary and the artifact key.
    pub scope: String,
    /// Minimum isolation the caller requires; the provider must meet or exceed it.
    pub isolation: IsolationClass,
    /// Frozen root filesystem/environment selected by the control plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentKind>,
    /// Container main process when the provider realizes process-as-container.
    /// It is excluded from the reusable capacity shape because it starts only
    /// after checkout.
    pub command: Vec<String>,
    /// Whether Workdir-tier rooted tools must run without a network namespace.
    /// This is distinct from [`NetworkPolicy`], which describes whole-sandbox
    /// enforcement and participates in provider admission.
    pub deny_tool_egress: bool,
    #[serde(default)]
    pub mounts: Vec<MountRequirement>,
    /// Base env applied to every process launched in the sandbox.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Frozen package inputs realized by the provider before workload launch.
    #[serde(default)]
    pub packages: PackageRequirements,
    pub network: NetworkPolicy,
    /// Sandbox-absolute directory the agent writes artifacts to (e.g.
    /// `/mnt/session/outputs`).
    pub outputs_path: String,
    /// Infrastructure scheduling reservation. This is distinct from the
    /// enforceable runtime cap below.
    #[serde(default)]
    pub requests: ResourceRequests,
    #[serde(default)]
    pub limits: ResourceLimits,
    /// Typed lifecycle demand for the writable filesystem. Providers without a
    /// retention mechanism may ignore `Ephemeral`; a configured retention
    /// mechanism must not allocate durable storage for it.
    #[serde(default)]
    pub filesystem_continuity: FilesystemContinuity,
    /// Closed, provider-neutral control services this Sandbox must publish.
    /// Empty preserves every pre-existing Pod/argv/fingerprint shape.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub control_services: BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
    /// Optional dead-man's-switch: the owner must `renew_lease` within this window
    /// or the sandbox self-reaps. `None` = no lease (a local child dies with its
    /// parent anyway); set it for remote sandboxes that outlive the owning host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_ttl_secs: Option<u64>,
}

/// Canonical identity of one substitutable, never-used sandbox capacity shape.
///
/// This is the only shape algorithm used by proactive warmup, pool checkout,
/// Worker receipts, and Coordinator placement preference. It excludes only the
/// per-Session scope and the process command launched after environment creation.
/// Every other current and future [`SandboxSpec`] field participates by default.
/// Specs with creation-time mounts are deliberately not poolable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxCapacityShapeId(String);

impl SandboxCapacityShapeId {
    /// Derive the exact poolable capacity identity, or `None` when the spec
    /// contains Session-specific creation mounts.
    #[must_use]
    pub fn from_spec(spec: &SandboxSpec) -> Option<Self> {
        if !spec.mounts.is_empty() {
            return None;
        }
        let mut normalized = spec.clone();
        normalized.scope.clear();
        normalized.command.clear();
        Some(Self(awaken_agent_contract::stable_fingerprint(&normalized)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SandboxCapacityShapeId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SandboxCapacityShapeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for SandboxCapacityShapeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SandboxCapacityShapeId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
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

/// When a Session materializes the sandbox selected by its frozen Environment.
///
/// This is part of the provisioning policy's published language. Session owns
/// when the transition is requested; providers only consume the resulting
/// decision and never reinterpret it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProvisioning {
    #[default]
    Eager,
    OnToolUse,
}

/// Full-sandbox behavior after the owning Session reaches a durable idle edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxIdleRetentionMode {
    #[default]
    Resident,
    CheckpointAndRelease,
}

/// A stale checkpoint never silently becomes live state. Expiry only authorizes
/// a fresh sandbox from the already-frozen Environment policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxCheckpointExpiryBehavior {
    #[default]
    FreshFromFrozenEnvironment,
}

/// Immutable continuation policy published with one sandbox policy revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxIdleRetentionPolicy {
    #[serde(default)]
    pub mode: SandboxIdleRetentionMode,
    #[serde(default)]
    pub checkpoint_after_secs: u64,
    #[serde(default)]
    pub retention_secs: u64,
    #[serde(default)]
    pub expiry_behavior: SandboxCheckpointExpiryBehavior,
    #[serde(default)]
    pub max_checkpoint_bytes: u64,
    #[serde(default)]
    pub max_checkpoint_duration_secs: u64,
    #[serde(default)]
    pub checkpoint_format: String,
}

impl Default for SandboxIdleRetentionPolicy {
    fn default() -> Self {
        Self {
            mode: SandboxIdleRetentionMode::Resident,
            checkpoint_after_secs: 0,
            retention_secs: 0,
            expiry_behavior: SandboxCheckpointExpiryBehavior::FreshFromFrozenEnvironment,
            max_checkpoint_bytes: 0,
            max_checkpoint_duration_secs: 0,
            checkpoint_format: String::new(),
        }
    }
}

impl SandboxIdleRetentionPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.mode == SandboxIdleRetentionMode::Resident {
            return Ok(());
        }
        if self.checkpoint_after_secs == 0 {
            return Err("checkpoint_after_secs must be positive");
        }
        if self.retention_secs <= self.checkpoint_after_secs {
            return Err("retention_secs must exceed checkpoint_after_secs");
        }
        if self.max_checkpoint_bytes == 0 || self.max_checkpoint_duration_secs == 0 {
            return Err("checkpoint bounds must be positive");
        }
        if self.checkpoint_format.trim().is_empty() {
            return Err("checkpoint_format must be non-empty");
        }
        Ok(())
    }
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

    fn capacity_spec() -> SandboxSpec {
        SandboxSpec {
            scope: "session-a".into(),
            isolation: IsolationClass::Container,
            environment: Some(EnvironmentKind::Image {
                reference: "agent:v1".into(),
            }),
            command: vec!["agent".into(), "--acp".into()],
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::None,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
        }
    }

    #[test]
    fn capacity_shape_is_the_single_complete_creation_identity() {
        // FMECA: F1 placement and pool use different field allowlists, so a new
        // creation field can advertise a false warm hit (S9,O4,D4,RPN144); F2
        // Session scope/attempt command fragments substitutable capacity
        // (S4,O6,D3,RPN72); F3 a mounted shape crosses Session bytes (S10,O3,D2,
        // RPN60). Cause graph: C1=only scope changes; C2=only command changes;
        // C3=any creation field changes; C4=any mount exists. Effects: E1=same
        // typed identity; E2=different identity; E3=not poolable.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | S1   | 1  | 0  | 0  | 0  | E1     |
        // | S2   | 0  | 1  | 0  | 0  | E1     |
        // | S3   | 0  | 0  | 1  | 0  | E2     |
        // | S4   | -  | -  | -  | 1  | E3     |
        let base = capacity_spec();
        let base_id = SandboxCapacityShapeId::from_spec(&base).expect("poolable");

        let mut scope = base.clone();
        scope.scope = "session-b".into();
        assert_eq!(
            SandboxCapacityShapeId::from_spec(&scope),
            Some(base_id.clone()),
            "S1"
        );

        let mut command = base.clone();
        command.command = vec!["other".into()];
        assert_eq!(
            SandboxCapacityShapeId::from_spec(&command),
            Some(base_id.clone()),
            "S2"
        );

        let mut variants = Vec::new();
        let mut isolation = base.clone();
        isolation.isolation = IsolationClass::Namespace;
        variants.push(isolation);
        let mut env = base.clone();
        env.env.push(EnvVar {
            name: "MODE".into(),
            value: EnvValue::Inline {
                value: "strict".into(),
            },
            visibility: EnvVisibility::Process,
        });
        variants.push(env);
        let mut packages = base.clone();
        packages.packages.resolution_id = Some("packages-v2".into());
        variants.push(packages);
        let mut network = base.clone();
        network.network = NetworkPolicy::Unrestricted;
        variants.push(network);
        let mut outputs = base.clone();
        outputs.outputs_path = "/other/outputs".into();
        variants.push(outputs);
        let mut limits = base.clone();
        limits.limits.memory_bytes = Some(1 << 20);
        variants.push(limits);
        let mut lease = base.clone();
        lease.lease_ttl_secs = Some(60);
        variants.push(lease);
        let mut rootfs = base.clone();
        rootfs.environment = Some(EnvironmentKind::Image {
            reference: "agent:v2".into(),
        });
        variants.push(rootfs);
        let mut tool_network = base.clone();
        tool_network.deny_tool_egress = true;
        variants.push(tool_network);
        for variant in variants {
            assert_ne!(
                SandboxCapacityShapeId::from_spec(&variant),
                Some(base_id.clone()),
                "S3"
            );
        }

        let mut mounted = base;
        mounted.mounts.push(MountRequirement {
            mount_id: "session-input".into(),
            source: crate::MountSource::Inline {
                contents: "x".into(),
            },
            mount_path: "/input".into(),
            access: crate::MountAccess::ReadOnly,
            lifetime: crate::MountLifetime::PerRun,
            required: true,
        });
        assert_eq!(SandboxCapacityShapeId::from_spec(&mounted), None, "S4");
    }

    #[test]
    fn legacy_specs_default_to_retained_filesystem_continuity() {
        /* Compatibility cause/effect rule: C1 serialized input predates the
         * continuity field; C2 input explicitly selects ephemeral. E1 C1 must
         * deserialize as Retained (fail-safe); E2 C2 must remain Ephemeral.
         */
        let mut legacy = serde_json::to_value(capacity_spec()).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("filesystem_continuity");
        let retained: SandboxSpec = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            retained.filesystem_continuity,
            FilesystemContinuity::Retained,
            "E1"
        );

        let mut ephemeral = capacity_spec();
        ephemeral.filesystem_continuity = FilesystemContinuity::Ephemeral;
        let round_trip: SandboxSpec =
            serde_json::from_slice(&serde_json::to_vec(&ephemeral).unwrap()).unwrap();
        assert_eq!(
            round_trip.filesystem_continuity,
            FilesystemContinuity::Ephemeral,
            "E2"
        );
    }
}

/// A typed policy projection onto a synthesized [`SandboxSpec`]. New snapshots
/// source it from an exact [`crate::SandboxExecutionPolicyRef`]; retained Sessions
/// may still contain the equivalent serialized value. It carries execution root,
/// `isolation`, legacy `network`, and `limits` — each optional so a partial value
/// overrides just what it sets and
/// leaves the rest at the host default. Content mounts are deliberately NOT here:
/// those are the ADR-0038
/// Resources context (files/memory/repos), realized as [`MountRequirement`]s from a
/// content source; a bare UI mount path has no source to realize.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<IsolationClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    pub requests: ResourceRequests,
    pub limits: ResourceLimits,
}

impl SandboxOverride {
    /// Whether this overlay changes anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.environment.is_none()
            && self.isolation.is_none()
            && self.network.is_none()
            && !self.requests.is_set()
            && !self.limits.is_set()
    }

    /// Overlay onto a base spec: each set field wins; unset fields keep the base's
    /// synthesized value. Mounts/scope/outputs are the host's to decide, untouched.
    #[must_use]
    pub fn apply(&self, mut spec: SandboxSpec) -> SandboxSpec {
        if let Some(environment) = &self.environment {
            spec.environment = Some(environment.clone());
        }
        if let Some(isolation) = self.isolation {
            spec.isolation = isolation;
        }
        if let Some(network) = &self.network {
            spec.network = network.clone();
        }
        if self.requests.is_set() {
            spec.requests = self.requests.clone();
        }
        if self.limits.is_set() {
            spec.limits = self.limits.clone();
        }
        spec
    }
}

#[cfg(test)]
mod sandbox_override_tests {
    use super::*;
    use crate::vocab::NetworkPolicy;

    fn base() -> SandboxSpec {
        SandboxSpec {
            scope: "t".into(),
            isolation: IsolationClass::Workdir,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            requests: ResourceRequests::default(),
            limits: ResourceLimits::default(),
            filesystem_continuity: FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
        }
    }

    #[test]
    fn parses_the_console_blob_and_overlays_every_enforceable_field() {
        // Exactly the shape a retained frozen policy projection carries.
        let blob = serde_json::json!({
            "environment": { "kind": "image", "reference": "registry.example/agent:v2" },
            "isolation": "namespace",
            "network": { "mode": "allowlist", "hosts": ["api.github.com"] },
            "requests": { "cpu_millis": 750, "memory_bytes": 2147483648u64 },
            "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
        });
        let over: SandboxOverride = serde_json::from_value(blob).expect("valid typed policy");
        let spec = over.apply(base());
        assert_eq!(
            spec.environment,
            Some(EnvironmentKind::Image {
                reference: "registry.example/agent:v2".into()
            })
        );
        assert_eq!(spec.isolation, IsolationClass::Namespace);
        assert_eq!(
            spec.network,
            NetworkPolicy::Allowlist {
                hosts: vec!["api.github.com".into()]
            }
        );
        assert_eq!(spec.requests.cpu_millis, Some(750));
        assert_eq!(spec.requests.memory_bytes, Some(2_147_483_648));
        assert_eq!(spec.limits.cpu_millis, Some(2000));
        assert_eq!(spec.limits.memory_bytes, Some(4_294_967_296));
        assert!(spec.mounts.is_empty());
    }

    #[test]
    fn a_partial_blob_overrides_only_what_it_sets() {
        let over: SandboxOverride =
            serde_json::from_value(serde_json::json!({ "network": { "mode": "none" } }))
                .expect("valid typed policy");
        assert!(over.environment.is_none() && over.isolation.is_none() && !over.limits.is_set());
        let spec = over.apply(base());
        assert_eq!(spec.network, NetworkPolicy::None);
        assert_eq!(
            spec.isolation,
            IsolationClass::Workdir,
            "base isolation kept"
        );
    }

    #[test]
    fn malformed_or_unknown_policy_fields_fail_closed() {
        let empty: SandboxOverride = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.is_empty());
        assert!(
            serde_json::from_value::<SandboxOverride>(serde_json::json!({
                "isolation": "bogus"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SandboxOverride>(serde_json::json!({
                "environment": { "kind": "not-real" }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<SandboxOverride>(serde_json::json!({
                "mounts": []
            }))
            .is_err()
        );
    }

    #[test]
    fn sandbox_spec_rejects_provider_specific_free_form_fields() {
        let mut value = serde_json::to_value(base()).expect("encode canonical spec");
        value["provider_field"] = serde_json::json!("must-not-cross-contract");
        assert!(serde_json::from_value::<SandboxSpec>(value).is_err());
    }
}
