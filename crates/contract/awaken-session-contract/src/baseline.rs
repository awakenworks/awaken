//! Immutable Session baseline and its consumed creation intent (ADR-0066 D1).

use awaken_credential_contract::CredentialRealizationProfile;
use awaken_environment_contract::{EnvironmentPackages, EnvironmentRevision};

/// Exact Environment snapshot admission shared by Session creation and Kani.
/// Every resolved snapshot has a positive revision; an immutable published pin
/// additionally requires equality with the requested revision.
#[must_use]
pub const fn resolved_environment_snapshot_is_exact(
    identity_matches: bool,
    actual_revision: u64,
    required_revision: Option<u64>,
) -> bool {
    identity_matches
        && actual_revision > 0
        && match required_revision {
            Some(required) => required > 0 && actual_revision == required,
            None => true,
        }
}

#[cfg(kani)]
mod kani_proofs {
    use super::resolved_environment_snapshot_is_exact;

    #[kani::proof]
    fn resolved_environment_snapshot_accepts_only_exact_positive_identity_and_revision() {
        let identity_matches: bool = kani::any();
        let actual_revision: u64 = kani::any();
        let has_required_revision: bool = kani::any();
        let required_revision: u64 = kani::any();
        let required = has_required_revision.then_some(required_revision);

        assert_eq!(
            resolved_environment_snapshot_is_exact(identity_matches, actual_revision, required),
            identity_matches
                && actual_revision > 0
                && (!has_required_revision
                    || (required_revision > 0 && actual_revision == required_revision))
        );
    }
}

/// Frozen network fact. This is Session state, not a provider request; the Host
/// projects it to the provisioning contract at realization time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionNetworkPolicy {
    Unrestricted,
    Allowlist { hosts: Vec<String> },
    None,
}

/// When a Session materializes the sandbox selected by its frozen Environment.
///
/// This is Session configuration truth. Providers consume the decision but do
/// not own or reinterpret it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProvisioning {
    #[default]
    Eager,
    OnToolUse,
}

/// Frozen owner class for the Session Runtime projection. Environment
/// WorkQueue selection is an independent Environment concern and must not be
/// reinterpreted as this deployment placement decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimePlacement {
    /// Retained rows written before Runtime placement became a frozen Session
    /// fact. Only the Session application may resolve this upgrade state from
    /// the process role; newly compiled baselines never emit it.
    #[default]
    LegacyUnspecified,
    Local,
    Worker,
}

impl SessionNetworkPolicy {
    /// Whether the frozen Session policy restricts egress at all.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }

    /// Canonical form used at every persistence/realization boundary. Host names
    /// are trimmed, lower-cased and deduplicated; an empty allowlist is exactly
    /// `None`, not a second spelling of closed networking.
    #[must_use]
    pub fn normalized(&self) -> Self {
        match self {
            Self::Unrestricted => Self::Unrestricted,
            Self::None => Self::None,
            Self::Allowlist { hosts } => {
                let hosts = hosts
                    .iter()
                    .map(|host| host.trim().to_ascii_lowercase())
                    .filter(|host| !host.is_empty())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if hosts.is_empty() {
                    Self::None
                } else {
                    Self::Allowlist { hosts }
                }
            }
        }
    }

    /// Safe meet for independently authored Session restrictions. The result
    /// never widens either input and canonicalizes an empty intersection to
    /// `None`.
    #[must_use]
    pub fn safe_intersection(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::None, _) | (_, Self::None) => Self::None,
            (Self::Unrestricted, policy) | (policy, Self::Unrestricted) => policy.normalized(),
            (Self::Allowlist { hosts: left }, Self::Allowlist { hosts: right }) => {
                let right = right
                    .iter()
                    .map(|host| host.trim().to_ascii_lowercase())
                    .collect::<std::collections::BTreeSet<_>>();
                let hosts = left
                    .iter()
                    .map(|host| host.trim().to_ascii_lowercase())
                    .filter(|host| right.contains(host))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if hosts.is_empty() {
                    Self::None
                } else {
                    Self::Allowlist { hosts }
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct EnvironmentFingerprint(pub String);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionBaselineFingerprint(pub String);

/// Exact normalized Environment facts frozen for one Session.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentSnapshot {
    pub environment_id: String,
    pub revision: EnvironmentRevision,
    /// Whether execution is delegated through the external Worker WorkQueue.
    /// This placement fact is frozen with the Environment revision so recovery
    /// never reopens today's mutable executable catalog.
    #[serde(default)]
    pub self_hosted: bool,
    pub config_fingerprint: EnvironmentFingerprint,
    /// Canonicalized, network-free sandbox requirement. `network` is the only
    /// reachability authority in this snapshot.
    pub sandbox: serde_json::Value,
    /// Frozen creation timing from the exact Environment-bound execution policy.
    /// Absence in older persisted rows preserves the historical eager behavior.
    #[serde(default)]
    pub sandbox_provisioning: SandboxProvisioning,
    /// Frozen whole-Environment idle continuation policy. Historical Sessions
    /// default to resident and therefore never acquire destructive new behavior.
    #[serde(default)]
    pub idle_retention: EnvironmentIdleRetentionPolicy,
    /// Exact package inputs frozen with this Environment revision. Providers
    /// provision them before workload launch or reject the spec fail-closed.
    #[serde(default)]
    pub packages: EnvironmentPackages,
    /// Immutable OCI reference prepared by Coordinator for these exact package
    /// inputs. Older snapshots omit it and retain build-at-realization behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_image: Option<String>,
    pub network: SessionNetworkPolicy,
    pub credential_realization: CredentialRealizationProfile,
}

/// Full-Environment behavior after the Session reaches a durable idle edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentIdleRetentionMode {
    #[default]
    Resident,
    CheckpointAndRelease,
}

/// Expiry never silently reinterprets a corrupt live checkpoint. It only
/// authorizes a new Sandbox from the already frozen Environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentCheckpointExpiryBehavior {
    #[default]
    FreshFromFrozenEnvironment,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentIdleRetentionPolicy {
    #[serde(default)]
    pub mode: EnvironmentIdleRetentionMode,
    #[serde(default)]
    pub checkpoint_after_secs: u64,
    #[serde(default)]
    pub retention_secs: u64,
    #[serde(default)]
    pub expiry_behavior: EnvironmentCheckpointExpiryBehavior,
    #[serde(default)]
    pub max_checkpoint_bytes: u64,
    #[serde(default)]
    pub max_checkpoint_duration_secs: u64,
    /// Exact portable format required at both provider and Worker admission.
    #[serde(default)]
    pub checkpoint_format: String,
}

impl Default for EnvironmentIdleRetentionPolicy {
    fn default() -> Self {
        Self {
            mode: EnvironmentIdleRetentionMode::Resident,
            checkpoint_after_secs: 0,
            retention_secs: 0,
            expiry_behavior: EnvironmentCheckpointExpiryBehavior::FreshFromFrozenEnvironment,
            max_checkpoint_bytes: 0,
            max_checkpoint_duration_secs: 0,
            checkpoint_format: String::new(),
        }
    }
}

impl EnvironmentIdleRetentionPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.mode == EnvironmentIdleRetentionMode::Resident {
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

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMcpAuthoringContext {
    pub ordered_vault_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ControlSessionCreationInputs {
    pub environment: EnvironmentSnapshot,
    #[serde(default)]
    pub runtime_placement: SessionRuntimePlacement,
    pub agent_id: String,
    /// Exact immutable Agent publication selected for this Session. Historical
    /// rows without a pin retain `None` and use their legacy recovery behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
    pub model: String,
    #[serde(default)]
    pub execution_model_ref: String,
    pub runtime: Option<String>,
    pub mcp_authoring: SessionMcpAuthoringContext,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    #[serde(default)]
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Optional immutable committed-history prefix projected into every model
    /// request for this Session. The referenced source Thread remains the sole
    /// transcript authority; no source message is copied into target truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
    pub resources: crate::ResolvedSessionResources,
    #[serde(default)]
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionCreationIntent {
    pub control: ControlSessionCreationInputs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSessionCreation {
    pub baseline: SessionBaseline,
    pub initial_resources: crate::ResolvedSessionResources,
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionCreationFinalizeError {
    #[error("MCP attachment definitions conflict: {0}")]
    McpConflict(String),
}

impl SessionCreationIntent {
    /// Compile the complete creation intent before any external realization.
    /// Every caller supplies its immutable inputs up front; Worker-local setup
    /// remains an execution concern and cannot mutate the Session baseline.
    pub fn finalize(self) -> Result<CompiledSessionCreation, SessionCreationFinalizeError> {
        let ControlSessionCreationInputs {
            environment,
            runtime_placement,
            agent_id,
            agent_revision,
            model,
            execution_model_ref,
            runtime,
            mcp_authoring,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
            resources,
            initial_mcp,
        } = self.control;
        let initial_mcp = crate::mcp_attachment::resolve_mcp_draft_precedence(initial_mcp)
            .map_err(|error| SessionCreationFinalizeError::McpConflict(error.to_string()))?;
        let baseline = SessionBaseline::compile_with_execution_model_ref(
            SessionBaselineInputs {
                environment,
                runtime_placement,
                mcp_authoring,
                agent_id,
                agent_revision,
                model,
                runtime,
                delegate_ids,
                toolsets,
                mounts,
                env,
                prompts,
                transcript_prefix,
            },
            execution_model_ref,
        );
        Ok(CompiledSessionCreation {
            baseline,
            initial_resources: resources,
            initial_mcp,
        })
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionBaseline {
    pub fingerprint: SessionBaselineFingerprint,
    pub environment: EnvironmentSnapshot,
    #[serde(default)]
    pub runtime_placement: SessionRuntimePlacement,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<u64>,
    pub model: String,
    /// Runtime coordinate resolved from `model` by the published Agent source.
    /// It is fingerprinted with the baseline and never inferred from public
    /// Managed syntax after Session admission.
    #[serde(default)]
    pub execution_model_ref: String,
    pub runtime: Option<String>,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    #[serde(default)]
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
}

pub struct SessionBaselineInputs {
    pub environment: EnvironmentSnapshot,
    pub runtime_placement: SessionRuntimePlacement,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub model: String,
    pub runtime: Option<String>,
    pub delegate_ids: Vec<String>,
    pub toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mounts: Vec<serde_json::Value>,
    pub env: Vec<serde_json::Value>,
    pub prompts: Vec<String>,
    pub transcript_prefix:
        Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionBaselineState {
    Preparing(SessionCreationIntent),
    Frozen(SessionBaseline),
}

impl SessionBaseline {
    #[must_use]
    pub fn compile(inputs: SessionBaselineInputs) -> Self {
        let execution_model_ref = inputs.model.clone();
        Self::compile_with_execution_model_ref(inputs, execution_model_ref)
    }

    /// Compile a baseline whose public Managed model id differs from the exact
    /// model coordinate frozen into the executable publication.
    #[must_use]
    pub fn compile_with_execution_model_ref(
        inputs: SessionBaselineInputs,
        execution_model_ref: String,
    ) -> Self {
        #[derive(serde::Serialize)]
        struct Facts<'a> {
            environment: &'a EnvironmentSnapshot,
            runtime_placement: SessionRuntimePlacement,
            mcp_authoring: &'a SessionMcpAuthoringContext,
            agent_id: &'a str,
            agent_revision: Option<u64>,
            model: &'a str,
            execution_model_ref: &'a str,
            runtime: &'a Option<String>,
            delegate_ids: &'a [String],
            toolsets: &'a [awaken_agent_contract::ToolsetPolicy],
            mounts: &'a [serde_json::Value],
            env: &'a [serde_json::Value],
            prompts: &'a [String],
            transcript_prefix:
                &'a Option<awaken_agent_contract::thread::read::transcript::TranscriptSliceSpec>,
        }
        let SessionBaselineInputs {
            environment,
            runtime_placement,
            mcp_authoring,
            agent_id,
            agent_revision,
            model,
            runtime,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
        } = inputs;
        let fingerprint = SessionBaselineFingerprint(crate::stable_fingerprint(&Facts {
            environment: &environment,
            runtime_placement,
            mcp_authoring: &mcp_authoring,
            agent_id: &agent_id,
            agent_revision,
            model: &model,
            execution_model_ref: &execution_model_ref,
            runtime: &runtime,
            delegate_ids: &delegate_ids,
            toolsets: &toolsets,
            mounts: &mounts,
            env: &env,
            prompts: &prompts,
            transcript_prefix: &transcript_prefix,
        }));
        Self {
            fingerprint,
            environment,
            runtime_placement,
            mcp_authoring,
            agent_id,
            agent_revision,
            model,
            execution_model_ref,
            runtime,
            delegate_ids,
            toolsets,
            mounts,
            env,
            prompts,
            transcript_prefix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{PlaintextBoundary, PlaintextHolder};

    fn environment(revision: u64, network: SessionNetworkPolicy) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            environment_id: "env_a".into(),
            revision: EnvironmentRevision(revision),
            self_hosted: false,
            config_fingerprint: EnvironmentFingerprint(format!("config-{revision}")),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network,
            credential_realization: CredentialRealizationProfile {
                inference_holder: PlaintextHolder::new(
                    PlaintextBoundary::Workload,
                    "awaken.workload.acp",
                ),
                mcp_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
                resource_holder: PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
            },
        }
    }

    #[test]
    fn idle_retention_validation_follows_the_decision_table() {
        // Cause/effect graph: Resident ignores checkpoint-only values for legacy
        // compatibility. CheckpointAndRelease requires C1 positive idle delay,
        // C2 retention > idle delay, C3 positive size/time bounds and C4 an exact
        // format. R1 Resident => E1 valid; R2 all C1..C4 => E1 valid; R3..R7 each
        // violate one constraint => E2 reject before a Session can freeze it.
        let resident = EnvironmentIdleRetentionPolicy::default();
        assert_eq!(resident.validate(), Ok(()), "R1");

        let valid = EnvironmentIdleRetentionPolicy {
            mode: EnvironmentIdleRetentionMode::CheckpointAndRelease,
            checkpoint_after_secs: 60,
            retention_secs: 3_600,
            expiry_behavior: EnvironmentCheckpointExpiryBehavior::FreshFromFrozenEnvironment,
            max_checkpoint_bytes: 1_024,
            max_checkpoint_duration_secs: 30,
            checkpoint_format: "awaken-fs-tar-v1".into(),
        };
        assert_eq!(valid.validate(), Ok(()), "R2");
        for (rule, invalid) in [
            (
                "R3",
                EnvironmentIdleRetentionPolicy {
                    checkpoint_after_secs: 0,
                    ..valid.clone()
                },
            ),
            (
                "R4",
                EnvironmentIdleRetentionPolicy {
                    retention_secs: 60,
                    ..valid.clone()
                },
            ),
            (
                "R5",
                EnvironmentIdleRetentionPolicy {
                    max_checkpoint_bytes: 0,
                    ..valid.clone()
                },
            ),
            (
                "R6",
                EnvironmentIdleRetentionPolicy {
                    max_checkpoint_duration_secs: 0,
                    ..valid.clone()
                },
            ),
            (
                "R7",
                EnvironmentIdleRetentionPolicy {
                    checkpoint_format: "  ".into(),
                    ..valid.clone()
                },
            ),
        ] {
            assert!(invalid.validate().is_err(), "{rule}");
        }
    }

    #[test]
    fn legacy_environment_snapshot_defaults_to_local_placement() {
        // Cause/effect decision table for persisted compatibility:
        // P1 explicit self_hosted=true -> external Worker reconciliation;
        // P2 explicit false -> local execution; P3 legacy field absent -> false.
        // P3 must fail closed against manufacturing new external work for rows
        // written before placement became a frozen fact.
        let mut encoded =
            serde_json::to_value(environment(1, SessionNetworkPolicy::Unrestricted)).unwrap();
        encoded.as_object_mut().unwrap().remove("self_hosted");
        encoded.as_object_mut().unwrap().remove("idle_retention");
        let decoded: EnvironmentSnapshot = serde_json::from_value(encoded).unwrap();
        assert!(!decoded.self_hosted, "P3");
        assert_eq!(
            decoded.idle_retention,
            EnvironmentIdleRetentionPolicy::default(),
            "P3: retained rows remain Resident"
        );

        let mut explicit = environment(1, SessionNetworkPolicy::Unrestricted);
        explicit.self_hosted = true;
        let decoded: EnvironmentSnapshot =
            serde_json::from_value(serde_json::to_value(explicit).unwrap()).unwrap();
        assert!(decoded.self_hosted, "P1");
    }

    fn baseline_inputs(environment: EnvironmentSnapshot) -> SessionBaselineInputs {
        SessionBaselineInputs {
            environment,
            runtime_placement: SessionRuntimePlacement::Local,
            mcp_authoring: SessionMcpAuthoringContext::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        }
    }

    fn baseline(environment: EnvironmentSnapshot) -> SessionBaseline {
        SessionBaseline::compile(baseline_inputs(environment))
    }

    fn control_inputs(network: SessionNetworkPolicy) -> ControlSessionCreationInputs {
        ControlSessionCreationInputs {
            environment: environment(1, network),
            runtime_placement: SessionRuntimePlacement::Local,
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            execution_model_ref: "model".into(),
            runtime: Some("native".into()),
            mcp_authoring: SessionMcpAuthoringContext::default(),
            delegate_ids: vec!["delegate".into()],
            toolsets: Vec::new(),
            mounts: vec![serde_json::json!({"source": "control"})],
            env: vec![serde_json::json!({"name": "CONTROL"})],
            prompts: vec!["control".into()],
            transcript_prefix: None,
            resources: crate::ResolvedSessionResources::default(),
            initial_mcp: Vec::new(),
        }
    }

    #[test]
    fn public_model_id_and_execution_model_ref_are_frozen_as_distinct_facts() {
        // Causes: C1 public id may be provider/endpoint qualified; C2 the
        // publication resolves an exact upstream model coordinate. Effects: E1
        // both facts survive independently; E2 changing only the execution
        // coordinate changes the baseline fingerprint.
        //
        // | Rule | public id | execution ref | Effect |
        // | M1 | qualified | upstream-a | E1 |
        // | M2 | same | upstream-b | E2 |
        let mut inputs = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        inputs.model = "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat".into();
        let first =
            SessionBaseline::compile_with_execution_model_ref(inputs, "qwen/qwen3-235b".into());
        let mut inputs = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        inputs.model = first.model.clone();
        let second =
            SessionBaseline::compile_with_execution_model_ref(inputs, "upstream-alias".into());

        assert_eq!(
            first.model, "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat",
            "M1"
        );
        assert_eq!(first.execution_model_ref, "qwen/qwen3-235b", "M1");
        assert_ne!(first.fingerprint, second.fingerprint, "M2");
    }

    fn mcp_draft(
        name: &str,
        url: &str,
        origin: crate::McpAttachmentOrigin,
    ) -> crate::McpAttachmentDraft {
        crate::McpAttachmentDraft {
            name: name.into(),
            target: crate::McpTarget::parse_http(url).unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin,
        }
    }

    #[test]
    fn network_safe_intersection_cases_follow_the_decision_table() {
        // Cause graph:
        // either None -> None; otherwise either Unrestricted -> canonicalized
        // other side; otherwise intersect canonical host identities; an empty
        // intersection -> None. No branch may widen either input.
        //
        // | Rule | Left | Right | Shared hosts | Effect |
        // |---|---|---|---|---|
        // | N1 | Unrestricted | Unrestricted | - | Unrestricted |
        // | N2 | Unrestricted | Allowlist(B,a,A) | - | Allowlist(a,b) |
        // | N3 | Allowlist(B,a,A) | Unrestricted | - | Allowlist(a,b) |
        // | N4 | None | any | - | None |
        // | N5 | Allowlist(API,db) | Allowlist(api,cache) | api | Allowlist(api) |
        // | N6 | Allowlist(api) | Allowlist(db) | none | None |
        let unrestricted = SessionNetworkPolicy::Unrestricted;
        let mixed = SessionNetworkPolicy::Allowlist {
            hosts: vec!["B.example".into(), "a.example".into(), "A.EXAMPLE".into()],
        };
        let canonical = SessionNetworkPolicy::Allowlist {
            hosts: vec!["a.example".into(), "b.example".into()],
        };
        assert_eq!(
            unrestricted.safe_intersection(&unrestricted),
            SessionNetworkPolicy::Unrestricted,
            "N1"
        );
        assert_eq!(unrestricted.safe_intersection(&mixed), canonical, "N2");
        assert_eq!(mixed.safe_intersection(&unrestricted), canonical, "N3");
        assert_eq!(
            SessionNetworkPolicy::None.safe_intersection(&mixed),
            SessionNetworkPolicy::None,
            "N4"
        );
        assert_eq!(
            SessionNetworkPolicy::Allowlist {
                hosts: vec![" API.EXAMPLE ".into(), "db.example".into()],
            }
            .safe_intersection(&SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into(), "cache.example".into()],
            }),
            SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            },
            "N5"
        );
        assert_eq!(
            SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            }
            .safe_intersection(&SessionNetworkPolicy::Allowlist {
                hosts: vec!["db.example".into()],
            }),
            SessionNetworkPolicy::None,
            "N6"
        );
    }

    #[test]
    fn creation_finalization_cases_follow_the_decision_table() {
        // Cause/effect graph: complete immutable creation inputs are supplied by
        // Control -> MCP drafts resolve through the one Session-over-Agent
        // precedence rule -> baseline, resources, and generation-1 MCP freeze
        // together. Duplicate selected targets fail before persistence.
        //
        // | Rule | Up-front input | Same MCP name | Selected targets | Effect |
        // |---|---|---|---|---|
        // | Z1 | complete | none | unique | freeze every supplied fact |
        // | Z2 | complete | Session + Agent | unique | Session draft wins |
        // | Z3 | complete | none | duplicate | MCP conflict |
        let mut z1_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z1_control.resources.inputs.push(crate::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::new("input"),
            source: crate::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from("file"),
            },
            mount_path: "/inputs/file".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        });
        let z1 = SessionCreationIntent {
            control: z1_control,
        }
        .finalize()
        .expect("Z1");
        assert_eq!(z1.baseline.mounts.len(), 1, "Z1");
        assert_eq!(z1.baseline.env.len(), 1, "Z1");
        assert_eq!(z1.baseline.prompts, vec!["control"], "Z1");
        assert_eq!(z1.initial_resources.inputs.len(), 1, "Z1");

        let mut z2_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z2_control.initial_mcp.extend([
            mcp_draft(
                "calc",
                "https://agent.example",
                crate::McpAttachmentOrigin::Agent,
            ),
            mcp_draft(
                "calc",
                "https://session.example",
                crate::McpAttachmentOrigin::Session,
            ),
        ]);
        let z2 = SessionCreationIntent {
            control: z2_control,
        }
        .finalize()
        .expect("Z2");
        assert_eq!(z2.initial_mcp.len(), 1, "Z2");
        assert_eq!(
            z2.initial_mcp[0].target.http_url(),
            Some("https://session.example"),
            "Z2"
        );

        let mut z3_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z3_control.initial_mcp.extend([
            mcp_draft(
                "first",
                "https://same.example",
                crate::McpAttachmentOrigin::Agent,
            ),
            mcp_draft(
                "second",
                "https://same.example",
                crate::McpAttachmentOrigin::Session,
            ),
        ]);
        assert!(
            matches!(
                (SessionCreationIntent {
                    control: z3_control
                })
                .finalize(),
                Err(SessionCreationFinalizeError::McpConflict(_))
            ),
            "Z3"
        );
    }
    #[test]
    fn baseline_fingerprint_decision_table() {
        // Cause graph: equal normalized facts -> equal fingerprint; changing the
        // Environment revision, normalized network fact, or mount projection ->
        // different fingerprint.
        //
        // | Rule | Same revision | Same network | Effect |
        // |------|---------------|--------------|--------|
        // | B1   | T             | T            | equal  |
        // | B2   | F             | T            | differ |
        // | B3   | T             | F            | differ |
        // | B4   | T             | T + mount | differ |
        let original = baseline(environment(1, SessionNetworkPolicy::Unrestricted));
        assert_eq!(
            original.fingerprint,
            baseline(environment(1, SessionNetworkPolicy::Unrestricted)).fingerprint,
            "B1"
        );
        assert_ne!(
            original.fingerprint,
            baseline(environment(2, SessionNetworkPolicy::Unrestricted)).fingerprint,
            "B2"
        );
        assert_ne!(
            original.fingerprint,
            baseline(environment(1, SessionNetworkPolicy::None)).fingerprint,
            "B3"
        );
        let mut with_mount = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        with_mount.mounts = vec![serde_json::json!({"mount_id": "workspace"})];
        assert_ne!(
            original.fingerprint,
            SessionBaseline::compile(with_mount).fingerprint,
            "B4"
        );
    }

    #[test]
    fn legacy_baseline_defaults_runtime_placement_and_mounts() {
        // Cause/effect graph: C1 a retained baseline omits fields introduced
        // after its fingerprint was written; E1 mounts remain empty and E2
        // Runtime placement stays explicitly unresolved. An explicit value must
        // round-trip unchanged.
        //
        // | Rule | placement field | Effect |
        // |---|---|---|
        // | L1 | absent | LegacyUnspecified |
        // | L2 | local | Local |
        // | L3 | worker | Worker |
        let legacy = serde_json::json!({
            "fingerprint": "legacy",
            "environment": environment(1, SessionNetworkPolicy::Unrestricted),
            "mcp_authoring": {"ordered_vault_ids": []},
            "agent_id": "agent",
            "model": "model",
            "runtime": null,
            "delegate_ids": [],
            "skills": [],
            "env": [],
            "prompts": []
        });
        let decoded: SessionBaseline = serde_json::from_value(legacy).unwrap();
        assert!(decoded.mounts.is_empty());
        assert_eq!(
            decoded.runtime_placement,
            SessionRuntimePlacement::LegacyUnspecified,
            "L1"
        );
        for (rule, placement) in [
            ("L2", SessionRuntimePlacement::Local),
            ("L3", SessionRuntimePlacement::Worker),
        ] {
            let decoded: SessionBaseline = serde_json::from_value(
                serde_json::to_value(SessionBaseline::compile(SessionBaselineInputs {
                    runtime_placement: placement,
                    ..baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted))
                }))
                .expect("encode explicit placement"),
            )
            .expect("decode explicit placement");
            assert_eq!(decoded.runtime_placement, placement, "{rule}");
        }
    }
}
