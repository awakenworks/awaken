// @formal entrypoint session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal evidence session_realization.publication_excludes_staged_retired_generation
//! Immutable realization commands, projections and exact receipt vocabulary.
//! Control roles remain at the parent boundary; no state authority moves.

use super::*;
/// Exact durable Session projection consumed by local and remote realization.
/// A Worker may cache it only as rebuildable execution input; the Session
/// aggregate remains the sole authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrozenSessionProjection {
    pub workspace_id: String,
    pub revision: crate::SessionRevision,
    pub baseline: crate::SessionBaseline,
    /// Exact executable publication selected by the frozen baseline. This is a
    /// rebuildable Coordinator projection delivered to an authority-store-free
    /// Worker, never a second persisted Session or Agent authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_publication: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    #[serde(default)]
    pub environment: crate::SessionEnvironmentState,
    #[serde(default)]
    pub resource_revision: u64,
    pub resources: crate::ResolvedSessionResources,
    /// Prior active generation for an in-flight physical replacement. New
    /// Coordinators always include it; absence is accepted only for legacy
    /// same-generation projections and never authorizes guessing from a Worker
    /// cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_resource_manifest: Option<crate::SessionResourceManifest>,
    #[serde(default)]
    pub tools: crate::SessionToolConfiguration,
    pub mcp: Vec<crate::SessionMcpAttachment>,
    /// Materialized, rebuildable view of `baseline.transcript_prefix`. These
    /// messages remain request-only and are never persisted as Thread truth.
    #[serde(default)]
    pub request_context: Vec<awaken_agent_contract::agent::message::Message>,
}

/// The two valid installation intents for a complete frozen projection.
/// Invalid partial combinations are unrepresentable at the application port.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionProjectionInstallMode {
    /// Install dispatch facts before durable enqueue; no realization authority
    /// or physical Environment adoption is implied.
    Dispatch,
    /// Install the exact realization fence and optionally perform the Stage
    /// effects selected by the canonical realization driver.
    Realization {
        lease: ObservedSessionRealizationLease,
        prepare_session: bool,
    },
}

impl SessionProjectionInstallMode {
    /// Whether this installation materializes the Session execution context.
    #[must_use]
    pub const fn prepares_session(&self) -> bool {
        match self {
            Self::Dispatch => true,
            Self::Realization {
                prepare_session, ..
            } => *prepare_session,
        }
    }

    /// Whether a resident Environment binding must be adopted after prepare.
    #[must_use]
    pub const fn adopts_resident_environment(&self) -> bool {
        matches!(
            self,
            Self::Realization {
                prepare_session: true,
                ..
            }
        )
    }

    /// Exact realization fence carried by the mode, when one exists.
    #[must_use]
    pub const fn realization_lease(&self) -> Option<&crate::SessionRealizationLease> {
        match self {
            Self::Dispatch => None,
            Self::Realization { lease, .. } => Some(&lease.lease),
        }
    }
}

/// Whether decoding a frozen Resource transition can execute physical effects.
///
/// This is the sole compatibility decision for projections written before
/// `previous_resource_manifest` existed. New projections always carry both
/// aggregate-authored generations; callers that may perform Resource effects
/// must require them. Validation-only recovery may accept a bound legacy
/// projection as an already-installed no-op, but must not pass that transition
/// to a physical effect port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenResourceTransitionUse {
    ApplyEffects,
    ValidateInstalled,
}

impl FrozenSessionProjection {
    #[must_use]
    pub fn session_init(&self) -> crate::SessionInit {
        crate::SessionInit {
            workspace_id: self.workspace_id.clone(),
            agent_id: self.baseline.agent_id.clone(),
            delegate_ids: self.baseline.delegate_ids.clone(),
            tools: Some(self.tools.clone()),
            resource_revision: self.resource_revision,
            resources: self.resources.clone(),
            model: Some(self.baseline.execution_model_ref.clone()),
            runtime: self.baseline.runtime.clone(),
            environment: self.baseline.environment.clone(),
        }
    }

    /// Decode the one aggregate-authored Resource transition for this frozen
    /// projection. The typed use keeps legacy validation separate from any
    /// effect-capable path without exposing a boolean compatibility switch.
    pub fn resource_transition(
        &self,
        transition_use: FrozenResourceTransitionUse,
    ) -> Result<crate::SessionResourceTransition, crate::RunError> {
        let desired = crate::SessionResourceManifest::at_revision(
            self.workspace_id.clone(),
            self.resource_revision,
            self.resources.clone(),
        );
        let previous = match &self.previous_resource_manifest {
            Some(previous) => previous.clone(),
            None if self.environment.binding().is_none() => {
                // A legacy never-materialized projection has no physical prior
                // generation. Empty→desired is conservative and lets a newly
                // created/deferred Environment realize the complete manifest.
                crate::SessionResourceManifest::at_revision(
                    self.workspace_id.clone(),
                    0,
                    crate::ResolvedSessionResources::default(),
                )
            }
            None if transition_use == FrozenResourceTransitionUse::ValidateInstalled => {
                desired.clone()
            }
            None => {
                return Err(crate::RunError::unavailable_classified(
                    "session_resource_transition_missing",
                    "durable Session Environment recovery requires the prior Resource generation",
                ));
            }
        };
        crate::SessionResourceTransition::new(previous, desired)
            .map_err(|error| crate::RunError::bad_request(error.to_string()))
    }
}

/// local-process identities are mapped to these strings at the authenticated
/// application edge; the aggregate never imports their protocol vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationTarget {
    pub owner: String,
    pub runtime_incarnation: String,
    pub lease_expires_at_unix_ms: u64,
    /// Explicitly replace a live lease owned by another logical Runtime.
    /// Only a topology edge holding the current execution claim may set this;
    /// ordinary application callers leave it false and therefore cannot steal
    /// a live Session projection. Lease extension is a separate exact-fence
    /// command and can never be smuggled through this assignment value.
    /// An explicit reassignment also advances the same owner's epoch: normal
    /// Work retirement may already have closed it in the same process.
    #[serde(default)]
    pub reassign_existing_lease: bool,
}

/// Assignment result for one atomically loaded Session realization fence.
/// The aggregate applies the returned epoch in the same root-CAS commit that
/// installs the owner; callers cannot mint or reinterpret an epoch themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationAssignment {
    Reuse,
    Assign { epoch: u64 },
    StaleOwnership,
    EpochExhausted,
}

/// Closed owner/incarnation/epoch arbitration used by the real Session root
/// mutation. `current_epoch` is `None` only when no fence has ever existed.
#[must_use]
pub const fn session_realization_assignment(
    current_epoch: Option<u64>,
    current_is_live: bool,
    same_owner: bool,
    same_runtime_incarnation: bool,
    reassign_existing_lease: bool,
) -> SessionRealizationAssignment {
    if current_is_live && !same_owner && !reassign_existing_lease {
        return SessionRealizationAssignment::StaleOwnership;
    }
    let needs_assignment = !current_is_live || !same_runtime_incarnation || reassign_existing_lease;
    if !needs_assignment {
        return SessionRealizationAssignment::Reuse;
    }
    match current_epoch {
        None => SessionRealizationAssignment::Assign { epoch: 1 },
        Some(epoch) => match epoch.checked_add(1) {
            Some(epoch) => SessionRealizationAssignment::Assign { epoch },
            None => SessionRealizationAssignment::EpochExhausted,
        },
    }
}

#[cfg(kani)]
/// @obligation session_realization.concurrent_drivers_preserve_one_owner_fence
#[kani::proof]
fn session_realization_assignment_never_shares_a_live_foreign_owner_or_reuses_an_epoch() {
    let has_current = kani::any::<bool>();
    let current_epoch = has_current.then(kani::any::<u64>);
    let current_is_live = kani::any::<bool>();
    let same_owner = kani::any::<bool>();
    let same_runtime_incarnation = kani::any::<bool>();
    let reassign = kani::any::<bool>();
    let decision = session_realization_assignment(
        current_epoch,
        current_is_live,
        same_owner,
        same_runtime_incarnation,
        reassign,
    );

    if current_is_live && !same_owner && !reassign {
        assert_eq!(decision, SessionRealizationAssignment::StaleOwnership);
    }
    if let SessionRealizationAssignment::Assign { epoch } = decision {
        match current_epoch {
            None => assert_eq!(epoch, 1),
            Some(previous) => {
                assert!(previous < u64::MAX);
                assert_eq!(epoch, previous + 1);
            }
        }
    }
    if decision == SessionRealizationAssignment::Reuse {
        assert!(current_is_live);
        assert!(same_owner);
        assert!(same_runtime_incarnation);
        assert!(!reassign);
    }
    // A claim-authenticated continuation cannot inherit a locally retired
    // generation even when both logical owner and process incarnation match.
    // This checks every u64 epoch, including exhaustion, in the production
    // assignment kernel; it does not model HTTP ordering or physical cleanup.
    if reassign {
        assert!(matches!(
            decision,
            SessionRealizationAssignment::Assign { .. }
                | SessionRealizationAssignment::EpochExhausted
        ));
    }
    // Once a failed nonterminal assignment is expired in the Session root,
    // even the same Host incarnation must advance its locally closed epoch.
    // Prove progress as well as no-reuse across the entire u64 epoch domain;
    // exhaustion is the sole terminal alternative to a successor assignment.
    if !current_is_live {
        assert_eq!(
            decision,
            match current_epoch {
                None => SessionRealizationAssignment::Assign { epoch: 1 },
                Some(u64::MAX) => SessionRealizationAssignment::EpochExhausted,
                Some(epoch) => SessionRealizationAssignment::Assign { epoch: epoch + 1 },
            }
        );
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionRealizationAction {
    /// Realize the frozen Environment/Resources and stage every exact MCP
    /// request. `prepare_session` is the exact execution-materialization bit;
    /// MCP-only hot updates must not recreate an already-live environment.
    Stage {
        prepare_session: bool,
        #[serde(default)]
        mcp_stages: Vec<StageMcpAttachment>,
    },
    /// Durable activation has committed. Publish and drain only these exact
    /// generations before acknowledging the effect back to Control.
    Publish {
        #[serde(default)]
        publish: Vec<McpGenerationRef>,
        #[serde(default)]
        drain: Vec<McpGenerationRef>,
    },
    /// Every required projection acknowledgement is durable.
    Complete,
}

/// Secret-free next action returned after every successful Control phase.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationDirective {
    pub projection: FrozenSessionProjection,
    #[serde(flatten)]
    pub authority: SessionRealizationLeaseReceipt,
    pub action: SessionRealizationAction,
}

impl SessionRealizationDirective {
    /// Relative time is observation metadata, not phase progress.
    #[must_use]
    pub fn same_progress_as(&self, previous: &Self) -> bool {
        self.projection == previous.projection
            && self.authority.lease == previous.authority.lease
            && self.action == previous.action
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeginSessionRealization {
    pub session_id: crate::SessionId,
    pub target: SessionRealizationTarget,
    /// Exact manifest decoded from a live guarded dispatch, never a browser
    /// hint. Unclaimed Control realization selects the current desired state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_resources: Option<crate::SessionResourceManifest>,
}

/// Extend one already-authoritative realization lease without replaying the
/// projection/phase protocol. The asserted lease is the caller's exact fence;
/// Control may return a monotonic same-epoch successor but can never change its
/// owner, Runtime incarnation, or epoch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenewSessionRealization {
    pub session_id: crate::SessionId,
    pub asserted_lease: SessionRealizationLease,
    pub requested_expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActivateSessionRealization {
    pub session_id: crate::SessionId,
    pub lease: SessionRealizationLease,
    /// Exact Resource generation prepared by the Stage effect. `None` means
    /// this was an MCP-only phase and must not settle an independently pending
    /// Resource transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_resource_revision: Option<u64>,
    #[serde(default)]
    pub mcp_receipts: Vec<McpRealizationReceipt>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AcknowledgeSessionRealization {
    pub session_id: crate::SessionId,
    pub lease: SessionRealizationLease,
    #[serde(default)]
    pub published: Vec<McpGenerationRef>,
    #[serde(default)]
    pub drained: Vec<McpGenerationRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailSessionRealization {
    pub session_id: crate::SessionId,
    pub lease: SessionRealizationLease,
    /// Exact Resource generation whose preparation failed, when this failure
    /// crossed the Resource effect boundary. MCP-only failures leave pending
    /// Resource work untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_resource_revision: Option<u64>,
    /// Retryable effects retain desired state and expire this lease. Permanent
    /// failures, or retryable failures after the persisted budget is exhausted,
    /// enter the existing terminal failure path.
    #[serde(default)]
    pub retryable: bool,
    /// Exact claimed Run that drove the failed effect, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<awaken_agent_contract::agent::run::Id>,
    pub reason: String,
}

/// Exact active or pending Resource target authorized by one Session realization lease.
/// The Workspace is intentionally absent: the Session root adds it only after
/// matching this untrusted transport value to its retained generation and target.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum SessionResourceMaterializationTarget {
    Input(Box<crate::ResolvedInput>),
    Skill(crate::ResolvedSkillBinding),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResourceMaterializationIntent {
    session_id: crate::SessionId,
    resource_revision: u64,
    target: SessionResourceMaterializationTarget,
    lease: SessionRealizationLease,
}

impl SessionResourceMaterializationIntent {
    pub fn try_new(
        session_id: impl Into<crate::SessionId>,
        resource_revision: u64,
        input: crate::ResolvedInput,
        lease: SessionRealizationLease,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        if session_id.as_str().trim().is_empty() || resource_revision == 0 {
            return Err(
                "Session Resource materialization intent requires identity and revision".into(),
            );
        }
        Ok(Self {
            session_id,
            resource_revision,
            target: SessionResourceMaterializationTarget::Input(Box::new(input)),
            lease,
        })
    }

    pub fn try_new_skill(
        session_id: impl Into<crate::SessionId>,
        resource_revision: u64,
        binding: crate::ResolvedSkillBinding,
        lease: SessionRealizationLease,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        if session_id.as_str().trim().is_empty() || resource_revision == 0 {
            return Err(
                "Session Resource materialization intent requires identity and revision".into(),
            );
        }
        Ok(Self {
            session_id,
            resource_revision,
            target: SessionResourceMaterializationTarget::Skill(binding),
            lease,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &crate::SessionId {
        &self.session_id
    }

    #[must_use]
    pub const fn resource_revision(&self) -> u64 {
        self.resource_revision
    }

    #[must_use]
    pub const fn target(&self) -> &SessionResourceMaterializationTarget {
        &self.target
    }

    #[must_use]
    pub const fn lease(&self) -> &SessionRealizationLease {
        &self.lease
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionRealizationDriveError {
    #[error(transparent)]
    Effect(#[from] RunError),
    #[error(transparent)]
    Control(#[from] SessionRealizationControlFailure),
    #[error("Session realization phase protocol did not converge")]
    DidNotConverge,
}
