//! The sandbox spec (what to realize), the declared environment config, and the
//! process launch input.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::sandbox::IsolationClass;
use crate::sandbox::{SandboxError, SecretBroker};
use crate::vocab::{
    EnvValue, EnvVar, EnvVisibility, MountRequirement, NetworkPolicy, ResourceLimits,
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

/// A typed policy projection onto a synthesized [`SandboxSpec`]. New snapshots
/// source it from an exact [`crate::SandboxExecutionPolicyRef`]; retained Sessions
/// may still contain the equivalent serialized value. It carries execution root,
/// `isolation`, legacy `network`, and `limits` — each optional so a partial value
/// overrides just what it sets and
/// leaves the rest at the host default. Content mounts are deliberately NOT here:
/// those are the ADR-0038
/// resource plane (files/memory/repos), realized as [`MountRequirement`]s from a
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
    pub limits: ResourceLimits,
}

impl SandboxOverride {
    /// Parse a frozen policy projection (or a retained legacy snapshot).
    /// The `network`/`limits`/`isolation` shapes deserialize straight onto the contract
    /// enums, so this is a lenient field-by-field lift — unknown keys (e.g. UI `mounts`)
    /// are ignored, and a field that fails to parse is simply left unset (never a hard
    /// error that would block a session on a malformed knob). Returns `None` when the
    /// blob contributes nothing.
    #[must_use]
    pub fn from_config_value(sandbox: &Value) -> Option<Self> {
        let get = |k: &str| sandbox.get(k).cloned();
        let over = SandboxOverride {
            environment: get("environment").and_then(|v| serde_json::from_value(v).ok()),
            isolation: get("isolation").and_then(|v| serde_json::from_value(v).ok()),
            network: get("network").and_then(|v| serde_json::from_value(v).ok()),
            limits: get("limits")
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default(),
        };
        (!over.is_empty()).then_some(over)
    }

    /// Whether this overlay changes anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.environment.is_none()
            && self.isolation.is_none()
            && self.network.is_none()
            && !self.limits.is_set()
    }

    /// Overlay onto a base spec: each set field wins; unset fields keep the base's
    /// synthesized value. Mounts/scope/outputs are the host's to decide, untouched.
    #[must_use]
    pub fn apply(&self, mut spec: SandboxSpec) -> SandboxSpec {
        if let Some(environment) = &self.environment {
            let extra = spec
                .extra
                .get_or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !extra.is_object() {
                *extra = Value::Object(serde_json::Map::new());
            }
            if let (Value::Object(fields), Ok(value)) = (extra, serde_json::to_value(environment)) {
                fields.insert("environment".to_string(), value);
            }
        }
        if let Some(isolation) = self.isolation {
            spec.isolation = isolation;
        }
        if let Some(network) = &self.network {
            spec.network = network.clone();
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
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[test]
    fn parses_the_console_blob_and_overlays_every_enforceable_field() {
        // Exactly the shape a retained frozen policy projection carries.
        let blob = serde_json::json!({
            "environment": { "kind": "image", "reference": "registry.example/agent:v2" },
            "isolation": "namespace",
            "mounts": [{ "mount_path": "/work", "access": "read_write" }], // ignored (no source)
            "network": { "mode": "allowlist", "hosts": ["api.github.com"] },
            "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
        });
        let over = SandboxOverride::from_config_value(&blob).expect("blob contributes");
        let spec = over.apply(base());
        assert_eq!(
            spec.extra,
            Some(serde_json::json!({
                "environment": { "kind": "image", "reference": "registry.example/agent:v2" }
            }))
        );
        assert_eq!(spec.isolation, IsolationClass::Namespace);
        assert_eq!(
            spec.network,
            NetworkPolicy::Allowlist {
                hosts: vec!["api.github.com".into()]
            }
        );
        assert_eq!(spec.limits.cpu_millis, Some(2000));
        assert_eq!(spec.limits.memory_bytes, Some(4_294_967_296));
        // Mounts are the resource plane's, never lifted from the UI blob.
        assert!(spec.mounts.is_empty());
    }

    #[test]
    fn a_partial_blob_overrides_only_what_it_sets() {
        let over = SandboxOverride::from_config_value(
            &serde_json::json!({ "network": { "mode": "none" } }),
        )
        .expect("contributes");
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
    fn an_empty_or_junk_blob_contributes_nothing() {
        assert!(SandboxOverride::from_config_value(&serde_json::json!({})).is_none());
        // A malformed knob is left unset, never a hard error; here nothing parses → None.
        assert!(
            SandboxOverride::from_config_value(&serde_json::json!({ "isolation": "bogus" }))
                .is_none()
        );
        assert!(
            SandboxOverride::from_config_value(&serde_json::json!({
                "environment": { "kind": "not-real" }
            }))
            .is_none()
        );
    }

    #[test]
    fn environment_overlay_preserves_other_provider_extra_fields() {
        let over = SandboxOverride::from_config_value(&serde_json::json!({
            "environment": { "kind": "sandbox" }
        }))
        .expect("environment contributes");
        let mut spec = base();
        spec.extra = Some(serde_json::json!({ "image": "fallback:v1" }));
        let spec = over.apply(spec);
        assert_eq!(
            spec.extra,
            Some(serde_json::json!({
                "image": "fallback:v1",
                "environment": { "kind": "sandbox" }
            }))
        );
    }
}
