//! Immutable Session baseline and its consumed creation intent (ADR-0066 D1).

use crate::env_registry::EnvironmentRevision;
use awaken_credential_contract::CredentialRealizationProfile;

/// Frozen network fact. This is Session state, not a provider request; the Host
/// projects it to the provisioning contract at realization time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionNetworkPolicy {
    Unrestricted,
    Allowlist { hosts: Vec<String> },
    None,
}

impl SessionNetworkPolicy {
    /// Whether the frozen Session policy restricts egress at all.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        !matches!(self, Self::Unrestricted)
    }

    fn canonicalized(&self) -> Self {
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
            (Self::Unrestricted, policy) | (policy, Self::Unrestricted) => policy.canonicalized(),
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
    pub config_fingerprint: EnvironmentFingerprint,
    /// Canonicalized, network-free sandbox requirement. `network` is the only
    /// reachability authority in this snapshot.
    pub sandbox: serde_json::Value,
    pub network: SessionNetworkPolicy,
    pub credential_realization: CredentialRealizationProfile,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMcpAuthoringContext {
    pub ordered_vault_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ControlSessionCreationInputs {
    pub environment: EnvironmentSnapshot,
    pub agent_id: String,
    pub model: String,
    pub runtime: Option<String>,
    pub mcp_authoring: SessionMcpAuthoringContext,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
    pub resources: crate::ResolvedSessionResources,
    #[serde(default)]
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionCreationIntent {
    pub control: ControlSessionCreationInputs,
    pub application: ApplicationContributionState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSessionCreation {
    pub baseline: SessionBaseline,
    pub initial_resources: crate::ResolvedSessionResources,
    pub initial_mcp: Vec<crate::McpAttachmentDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionCreationFinalizeError {
    #[error("application contribution is still required")]
    ApplicationRequired,
    #[error("application MCP drafts were supplied when no application was registered")]
    UnexpectedApplicationMcp,
    #[error("application MCP input and normalized draft counts differ")]
    ApplicationMcpCountMismatch,
    #[error("normalized application MCP draft has a non-application origin")]
    InvalidApplicationMcpOrigin,
    #[error("MCP attachment definitions conflict: {0}")]
    McpConflict(String),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApplicationContributionState {
    Required,
    Absent,
    Committed {
        fingerprint: String,
        input: ApplicationSessionInput,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApplicationContributionOutcome {
    Committed,
    Replayed,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationContributionError {
    #[error("application contribution fingerprint is empty")]
    EmptyFingerprint,
    #[error("this Session does not accept an application contribution")]
    NotRequired,
    #[error("this Session already accepted a different application contribution")]
    Conflict,
}

/// Minimal durable evidence retained after the temporary contribution input is
/// consumed. It proves replay identity without retaining a second desired-state
/// copy beside the frozen baseline.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationContributionReceipt {
    pub plan_fingerprint: String,
    pub input_fingerprint: String,
}

impl ApplicationContributionReceipt {
    #[must_use]
    pub fn from_input(plan_fingerprint: String, input: &ApplicationSessionInput) -> Self {
        Self {
            plan_fingerprint,
            input_fingerprint: input.fingerprint(),
        }
    }

    pub fn verify_replay(
        &self,
        plan_fingerprint: &str,
        input: &ApplicationSessionInput,
    ) -> Result<ApplicationContributionOutcome, ApplicationContributionError> {
        if plan_fingerprint.trim().is_empty() {
            return Err(ApplicationContributionError::EmptyFingerprint);
        }
        if self.plan_fingerprint == plan_fingerprint
            && self.input_fingerprint == input.fingerprint()
        {
            Ok(ApplicationContributionOutcome::Replayed)
        } else {
            Err(ApplicationContributionError::Conflict)
        }
    }
}

/// Boundary command input. Values remain opaque until the Session application
/// compiler maps them to the owning Resource/provisioning/MCP contracts.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationSessionInput {
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub mcp_inputs: Vec<serde_json::Value>,
    pub network_restriction: Option<SessionNetworkPolicy>,
}

impl ApplicationSessionInput {
    #[must_use]
    pub fn fingerprint(&self) -> String {
        crate::stable_fingerprint(self)
    }
}

impl ApplicationContributionState {
    /// Accept one complete claim-fenced application input before baseline
    /// finalization. The repository root CAS supplies concurrency; this kernel
    /// supplies deterministic apply/replay/conflict semantics.
    pub fn accept(
        &mut self,
        fingerprint: String,
        input: ApplicationSessionInput,
    ) -> Result<ApplicationContributionOutcome, ApplicationContributionError> {
        if fingerprint.trim().is_empty() {
            return Err(ApplicationContributionError::EmptyFingerprint);
        }
        match self {
            Self::Required => {
                *self = Self::Committed { fingerprint, input };
                Ok(ApplicationContributionOutcome::Committed)
            }
            Self::Absent => Err(ApplicationContributionError::NotRequired),
            Self::Committed {
                fingerprint: current_fingerprint,
                input: current_input,
            } if current_fingerprint == &fingerprint && current_input == &input => {
                Ok(ApplicationContributionOutcome::Replayed)
            }
            Self::Committed { .. } => Err(ApplicationContributionError::Conflict),
        }
    }
}

impl SessionCreationIntent {
    /// Consume the temporary preparation intent into the one immutable baseline
    /// and generation-1 inputs. `application_mcp` must be the exact normalized
    /// projection of `ApplicationSessionInput::mcp_inputs` produced by the sole
    /// Managed anti-corruption compiler.
    pub fn finalize(
        self,
        application_mcp: Vec<crate::McpAttachmentDraft>,
    ) -> Result<CompiledSessionCreation, SessionCreationFinalizeError> {
        let (application, receipt) = match self.application {
            ApplicationContributionState::Required => {
                return Err(SessionCreationFinalizeError::ApplicationRequired);
            }
            ApplicationContributionState::Absent => {
                if !application_mcp.is_empty() {
                    return Err(SessionCreationFinalizeError::UnexpectedApplicationMcp);
                }
                (ApplicationSessionInput::default(), None)
            }
            ApplicationContributionState::Committed { fingerprint, input } => {
                if input.mcp_inputs.len() != application_mcp.len() {
                    return Err(SessionCreationFinalizeError::ApplicationMcpCountMismatch);
                }
                if application_mcp
                    .iter()
                    .any(|draft| draft.origin != crate::McpAttachmentOrigin::Application)
                {
                    return Err(SessionCreationFinalizeError::InvalidApplicationMcpOrigin);
                }
                let receipt = ApplicationContributionReceipt::from_input(fingerprint, &input);
                (input, Some(receipt))
            }
        };

        let ControlSessionCreationInputs {
            mut environment,
            agent_id,
            model,
            runtime,
            mcp_authoring,
            delegate_ids,
            mut mounts,
            mut env,
            mut prompts,
            resources,
            mut initial_mcp,
        } = self.control;
        if let Some(restriction) = application.network_restriction {
            environment.network = environment.network.safe_intersection(&restriction);
        }
        mounts.extend(application.mounts);
        env.extend(application.env);
        prompts.extend(application.prompts);
        initial_mcp.extend(application_mcp);
        let initial_mcp = crate::mcp_attachment::resolve_mcp_draft_precedence(initial_mcp)
            .map_err(|error| SessionCreationFinalizeError::McpConflict(error.to_string()))?;
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment,
            mcp_authoring,
            agent_id,
            model,
            runtime,
            application: receipt,
            delegate_ids,
            mounts,
            env,
            prompts,
        });
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
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    pub model: String,
    pub runtime: Option<String>,
    #[serde(default)]
    pub application: Option<ApplicationContributionReceipt>,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
    #[serde(default)]
    pub env: Vec<serde_json::Value>,
    #[serde(default)]
    pub prompts: Vec<String>,
}

pub struct SessionBaselineInputs {
    pub environment: EnvironmentSnapshot,
    pub mcp_authoring: SessionMcpAuthoringContext,
    pub agent_id: String,
    pub model: String,
    pub runtime: Option<String>,
    pub application: Option<ApplicationContributionReceipt>,
    pub delegate_ids: Vec<String>,
    pub mounts: Vec<serde_json::Value>,
    pub env: Vec<serde_json::Value>,
    pub prompts: Vec<String>,
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
        #[derive(serde::Serialize)]
        struct Facts<'a> {
            environment: &'a EnvironmentSnapshot,
            mcp_authoring: &'a SessionMcpAuthoringContext,
            agent_id: &'a str,
            model: &'a str,
            runtime: &'a Option<String>,
            application: &'a Option<ApplicationContributionReceipt>,
            delegate_ids: &'a [String],
            mounts: &'a [serde_json::Value],
            env: &'a [serde_json::Value],
            prompts: &'a [String],
        }
        let SessionBaselineInputs {
            environment,
            mcp_authoring,
            agent_id,
            model,
            runtime,
            application,
            delegate_ids,
            mounts,
            env,
            prompts,
        } = inputs;
        let fingerprint = SessionBaselineFingerprint(crate::stable_fingerprint(&Facts {
            environment: &environment,
            mcp_authoring: &mcp_authoring,
            agent_id: &agent_id,
            model: &model,
            runtime: &runtime,
            application: &application,
            delegate_ids: &delegate_ids,
            mounts: &mounts,
            env: &env,
            prompts: &prompts,
        }));
        Self {
            fingerprint,
            environment,
            mcp_authoring,
            agent_id,
            model,
            runtime,
            application,
            delegate_ids,
            mounts,
            env,
            prompts,
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
            config_fingerprint: EnvironmentFingerprint(format!("config-{revision}")),
            sandbox: serde_json::json!({}),
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

    fn baseline_inputs(environment: EnvironmentSnapshot) -> SessionBaselineInputs {
        SessionBaselineInputs {
            environment,
            mcp_authoring: SessionMcpAuthoringContext::default(),
            agent_id: "agent".into(),
            model: "model".into(),
            runtime: None,
            application: None,
            delegate_ids: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
        }
    }

    fn baseline(environment: EnvironmentSnapshot) -> SessionBaseline {
        SessionBaseline::compile(baseline_inputs(environment))
    }

    fn control_inputs(network: SessionNetworkPolicy) -> ControlSessionCreationInputs {
        ControlSessionCreationInputs {
            environment: environment(1, network),
            agent_id: "agent".into(),
            model: "model".into(),
            runtime: Some("native".into()),
            mcp_authoring: SessionMcpAuthoringContext::default(),
            delegate_ids: vec!["delegate".into()],
            mounts: vec![serde_json::json!({"source": "control"})],
            env: vec![serde_json::json!({"name": "CONTROL"})],
            prompts: vec!["control".into()],
            resources: crate::ResolvedSessionResources::default(),
            initial_mcp: Vec::new(),
        }
    }

    fn mcp_draft(
        name: &str,
        url: &str,
        origin: crate::McpAttachmentOrigin,
    ) -> crate::McpAttachmentDraft {
        crate::McpAttachmentDraft {
            name: name.into(),
            target: crate::McpTarget::parse_http(url).unwrap(),
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
        // Cause graph:
        // contribution state must be consumable -> application raw MCP count
        // must equal its normalized projection -> every projected draft must
        // have Application origin -> all sources resolve by precedence with
        // unique selected targets -> baseline and generation-1 inputs freeze.
        //
        // | Rule | App state | App drafts | Origin | Merge | Effect |
        // |---|---|---|---|---|---|
        // | Z1 | Required | none | - | - | ApplicationRequired |
        // | Z2 | Absent | none | - | valid | freeze without receipt |
        // | Z3 | Absent | one | Application | - | UnexpectedApplicationMcp |
        // | Z4 | Committed(1 raw) | none | - | - | count mismatch |
        // | Z5 | Committed(1 raw) | one | Agent | - | invalid origin |
        // | Z6 | Committed(1 raw) | one | Application | app beats Agent | freeze merged facts |
        // | Z7 | Committed(1 raw) | one | Application | target collision | MCP conflict |
        let required = SessionCreationIntent {
            control: control_inputs(SessionNetworkPolicy::Unrestricted),
            application: ApplicationContributionState::Required,
        };
        assert_eq!(
            required.finalize(Vec::new()),
            Err(SessionCreationFinalizeError::ApplicationRequired),
            "Z1"
        );

        let absent = SessionCreationIntent {
            control: control_inputs(SessionNetworkPolicy::Unrestricted),
            application: ApplicationContributionState::Absent,
        };
        let z2 = absent.clone().finalize(Vec::new()).expect("Z2");
        assert!(z2.baseline.application.is_none(), "Z2");
        assert!(z2.initial_mcp.is_empty(), "Z2");
        assert_eq!(
            absent.finalize(vec![mcp_draft(
                "app",
                "https://app.example",
                crate::McpAttachmentOrigin::Application,
            )]),
            Err(SessionCreationFinalizeError::UnexpectedApplicationMcp),
            "Z3"
        );

        let application_input = ApplicationSessionInput {
            mounts: vec![serde_json::json!({"source": "application"})],
            env: vec![serde_json::json!({"name": "APPLICATION"})],
            prompts: vec!["application".into()],
            mcp_inputs: vec![serde_json::json!({"name": "calc"})],
            network_restriction: Some(SessionNetworkPolicy::Allowlist {
                hosts: vec!["API.EXAMPLE".into(), "other.example".into()],
            }),
        };
        let committed = |control: ControlSessionCreationInputs| SessionCreationIntent {
            control,
            application: ApplicationContributionState::Committed {
                fingerprint: "plan-a".into(),
                input: application_input.clone(),
            },
        };
        assert_eq!(
            committed(control_inputs(SessionNetworkPolicy::Unrestricted)).finalize(Vec::new()),
            Err(SessionCreationFinalizeError::ApplicationMcpCountMismatch),
            "Z4"
        );
        assert_eq!(
            committed(control_inputs(SessionNetworkPolicy::Unrestricted)).finalize(vec![
                mcp_draft(
                    "calc",
                    "https://app.example",
                    crate::McpAttachmentOrigin::Agent
                ),
            ]),
            Err(SessionCreationFinalizeError::InvalidApplicationMcpOrigin),
            "Z5"
        );

        let mut z6_control = control_inputs(SessionNetworkPolicy::Allowlist {
            hosts: vec!["api.example".into(), "control.example".into()],
        });
        z6_control.initial_mcp.push(mcp_draft(
            "calc",
            "https://agent.example",
            crate::McpAttachmentOrigin::Agent,
        ));
        let z6 = committed(z6_control)
            .finalize(vec![mcp_draft(
                "calc",
                "https://app.example",
                crate::McpAttachmentOrigin::Application,
            )])
            .expect("Z6");
        assert_eq!(z6.initial_mcp.len(), 1, "Z6");
        assert_eq!(z6.initial_mcp[0].target.url, "https://app.example", "Z6");
        assert_eq!(z6.baseline.mounts.len(), 2, "Z6");
        assert_eq!(z6.baseline.env.len(), 2, "Z6");
        assert_eq!(z6.baseline.prompts, vec!["control", "application"], "Z6");
        assert_eq!(
            z6.baseline.environment.network,
            SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            },
            "Z6"
        );
        assert_eq!(
            z6.baseline
                .application
                .as_ref()
                .map(|receipt| receipt.plan_fingerprint.as_str()),
            Some("plan-a"),
            "Z6"
        );

        let mut z7_control = control_inputs(SessionNetworkPolicy::Unrestricted);
        z7_control.initial_mcp.push(mcp_draft(
            "control",
            "https://same.example",
            crate::McpAttachmentOrigin::Session,
        ));
        assert!(
            matches!(
                committed(z7_control).finalize(vec![mcp_draft(
                    "application",
                    "https://same.example",
                    crate::McpAttachmentOrigin::Application,
                )]),
                Err(SessionCreationFinalizeError::McpConflict(_))
            ),
            "Z7"
        );
    }

    #[test]
    fn baseline_fingerprint_decision_table() {
        // Cause graph: equal normalized facts -> equal fingerprint; changing the
        // Environment revision, normalized network fact, application receipt, or
        // mount projection -> different fingerprint.
        //
        // | Rule | Same revision | Same network | Effect |
        // |------|---------------|--------------|--------|
        // | B1   | T             | T            | equal  |
        // | B2   | F             | T            | differ |
        // | B3   | T             | F            | differ |
        // | B4   | T             | T + application | differ |
        // | B5   | T             | T + mount | differ |
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
        let mut with_application =
            baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        with_application.application = Some(ApplicationContributionReceipt {
            plan_fingerprint: "plan".into(),
            input_fingerprint: "input".into(),
        });
        assert_ne!(
            original.fingerprint,
            SessionBaseline::compile(with_application).fingerprint,
            "B4"
        );
        let mut with_mount = baseline_inputs(environment(1, SessionNetworkPolicy::Unrestricted));
        with_mount.mounts = vec![serde_json::json!({"mount_id": "application"})];
        assert_ne!(
            original.fingerprint,
            SessionBaseline::compile(with_mount).fingerprint,
            "B5"
        );
    }

    #[test]
    fn legacy_baseline_defaults_new_application_fields() {
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
        assert!(decoded.application.is_none());
        assert!(decoded.mounts.is_empty());
    }

    #[derive(Clone, Copy)]
    enum ContributionRule {
        Commit,
        Replay,
        SameFingerprintDifferentInput,
        DifferentFingerprint,
        NotRequired,
        EmptyFingerprint,
    }

    #[test]
    fn application_contribution_cases_follow_the_decision_table() {
        // Cause graph:
        // Required + non-empty fingerprint -> commit complete input;
        // exact committed fingerprint + exact payload -> replay;
        // either committed identity or payload differs -> conflict;
        // Absent -> not required; empty identity -> invalid before state checks.
        //
        // | Rule | State | Fingerprint | Payload | Effect |
        // |---|---|---|---|---|
        // | A1 | Required | non-empty | new | Committed |
        // | A2 | Committed | same | same | Replayed |
        // | A3 | Committed | same | different | Conflict |
        // | A4 | Committed | different | any | Conflict |
        // | A5 | Absent | non-empty | any | NotRequired |
        // | A6 | any | empty | any | EmptyFingerprint |
        let original = ApplicationSessionInput {
            prompts: vec!["application prompt".into()],
            ..Default::default()
        };
        for rule in [
            ContributionRule::Commit,
            ContributionRule::Replay,
            ContributionRule::SameFingerprintDifferentInput,
            ContributionRule::DifferentFingerprint,
            ContributionRule::NotRequired,
            ContributionRule::EmptyFingerprint,
        ] {
            let mut state = match rule {
                ContributionRule::Commit | ContributionRule::EmptyFingerprint => {
                    ApplicationContributionState::Required
                }
                ContributionRule::NotRequired => ApplicationContributionState::Absent,
                ContributionRule::Replay
                | ContributionRule::SameFingerprintDifferentInput
                | ContributionRule::DifferentFingerprint => {
                    ApplicationContributionState::Committed {
                        fingerprint: "plan-a".into(),
                        input: original.clone(),
                    }
                }
            };
            let fingerprint = match rule {
                ContributionRule::DifferentFingerprint => "plan-b",
                ContributionRule::EmptyFingerprint => " ",
                _ => "plan-a",
            };
            let input = if matches!(rule, ContributionRule::SameFingerprintDifferentInput) {
                ApplicationSessionInput {
                    prompts: vec!["different".into()],
                    ..Default::default()
                }
            } else {
                original.clone()
            };
            let result = state.accept(fingerprint.into(), input);
            match rule {
                ContributionRule::Commit => {
                    assert_eq!(result, Ok(ApplicationContributionOutcome::Committed), "A1");
                    assert!(matches!(
                        state,
                        ApplicationContributionState::Committed { .. }
                    ));
                }
                ContributionRule::Replay => {
                    assert_eq!(result, Ok(ApplicationContributionOutcome::Replayed), "A2");
                }
                ContributionRule::SameFingerprintDifferentInput => {
                    assert_eq!(result, Err(ApplicationContributionError::Conflict), "A3");
                }
                ContributionRule::DifferentFingerprint => {
                    assert_eq!(result, Err(ApplicationContributionError::Conflict), "A4");
                }
                ContributionRule::NotRequired => {
                    assert_eq!(result, Err(ApplicationContributionError::NotRequired), "A5");
                }
                ContributionRule::EmptyFingerprint => {
                    assert_eq!(
                        result,
                        Err(ApplicationContributionError::EmptyFingerprint),
                        "A6"
                    );
                }
            }
        }
    }

    #[test]
    fn frozen_application_receipt_cases_follow_the_decision_table() {
        // Cause graph: after input consumption, exact plan+input fingerprints
        // replay; changing either cause conflicts; empty identity is invalid.
        //
        // | Rule | Plan fingerprint | Input fingerprint | Effect |
        // |---|---|---|---|
        // | F1 | same | same | Replayed |
        // | F2 | same | different | Conflict |
        // | F3 | different | any | Conflict |
        // | F4 | empty | any | EmptyFingerprint |
        let input = ApplicationSessionInput {
            prompts: vec!["frozen".into()],
            ..Default::default()
        };
        let receipt = ApplicationContributionReceipt::from_input("plan-a".into(), &input);
        assert_eq!(
            receipt.verify_replay("plan-a", &input),
            Ok(ApplicationContributionOutcome::Replayed),
            "F1"
        );
        assert_eq!(
            receipt.verify_replay("plan-a", &ApplicationSessionInput::default()),
            Err(ApplicationContributionError::Conflict),
            "F2"
        );
        assert_eq!(
            receipt.verify_replay("plan-b", &input),
            Err(ApplicationContributionError::Conflict),
            "F3"
        );
        assert_eq!(
            receipt.verify_replay(" ", &input),
            Err(ApplicationContributionError::EmptyFingerprint),
            "F4"
        );
    }
}
