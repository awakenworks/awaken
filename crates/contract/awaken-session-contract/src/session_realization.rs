//! Control-owned Session realization phase protocol (ADR-0066 D5).
//!
//! The Session aggregate owns durable desired and lifecycle state. A Runtime
//! projection owns only staged/live effects. These commands let local and remote
//! topology adapters drive the same root-CAS transitions without giving a Worker
//! repository access or creating a second MCP desired-state registry.

use async_trait::async_trait;

use crate::{
    McpAttachmentRealizer, McpGenerationRef, McpRealizationReceipt, RunError,
    SessionRealizationLease, StageMcpAttachment,
};

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
        lease: crate::SessionRealizationLease,
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
            Self::Realization { lease, .. } => Some(lease),
        }
    }
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
}

/// Project every Session-owned Agent override onto one immutable publication.
///
/// This is the only lowering rule for Session-local model, inference, and
/// system-prompt selection. Coordinator publication, co-located execution, and
/// cold Worker recovery must reuse the returned snapshot; Runtime adapters may
/// validate its frozen coordinates but must not reconstruct it field by field.
pub fn project_effective_agent_publication(
    model_override: Option<&crate::SessionModelOverride>,
    system_prompt: &crate::SessionSystemPromptSelection,
    workspace_id: &str,
    mut snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, RunError> {
    if let Some(model_override) = model_override {
        if let Some(publication) = &model_override.publication {
            publication
                .validate_for_workspace(workspace_id)
                .map_err(|error| {
                    RunError::internal(format!(
                        "frozen Session model override publication is invalid: {error}"
                    ))
                })?;
            snapshot.resolved_spec.model_binding = publication.primary.clone();
            snapshot.resolved_spec.model_candidates = publication.candidates.clone();
        }
        snapshot.resolved_spec.plugin_config.inference = model_override.inference.clone();
    }
    if !system_prompt.is_inherit() {
        snapshot.resolved_spec.instructions = system_prompt
            .resolve(Some(snapshot.resolved_spec.instructions.clone()))
            .unwrap_or_default();
    }
    if model_override.is_some() || !system_prompt.is_inherit() {
        snapshot.recompute_fingerprint().map_err(|error| {
            RunError::internal(format!(
                "Session-local Agent publication is invalid: {error}"
            ))
        })?;
    }
    Ok(snapshot)
}

/// Total decision for binding an immutable Agent publication to one frozen
/// Session baseline. A pinned Worker projection is never allowed to continue
/// without the exact publication; local compatibility projections may omit it,
/// but any publication that is present must match every frozen coordinate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenAgentPublicationDecision {
    Unpinned,
    OptionalMissing,
    Exact,
    MissingRequired,
    Mismatch,
}

#[must_use]
const fn frozen_agent_publication_facts(
    has_frozen_revision: bool,
    worker_placement: bool,
    publication_present: bool,
    agent_id_matches: bool,
    source_revision_matches: bool,
    runtime_matches: bool,
) -> FrozenAgentPublicationDecision {
    if !has_frozen_revision {
        FrozenAgentPublicationDecision::Unpinned
    } else if !publication_present {
        if worker_placement {
            FrozenAgentPublicationDecision::MissingRequired
        } else {
            FrozenAgentPublicationDecision::OptionalMissing
        }
    } else if agent_id_matches && source_revision_matches && runtime_matches {
        FrozenAgentPublicationDecision::Exact
    } else {
        FrozenAgentPublicationDecision::Mismatch
    }
}

/// Compare a delivered executable publication with the immutable coordinates
/// in the Session baseline. Both Coordinator and Worker call this kernel, so a
/// transport cannot weaken the validation performed before dispatch.
#[must_use]
pub fn frozen_agent_publication_decision(
    baseline: &crate::SessionBaseline,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    let Some(source_revision) = baseline.agent_revision else {
        return FrozenAgentPublicationDecision::Unpinned;
    };
    let Some(publication) = publication else {
        return frozen_agent_publication_facts(
            true,
            baseline.runtime_placement == crate::SessionRuntimePlacement::Worker,
            false,
            false,
            false,
            false,
        );
    };
    let expected_backend = baseline
        .model_override
        .as_ref()
        .and_then(|model_override| model_override.publication.as_ref())
        .map(|publication| &publication.primary.binding().backend_ref)
        .unwrap_or(&publication.resolved_spec.model_binding.backend_ref);
    frozen_agent_publication_facts(
        true,
        baseline.runtime_placement == crate::SessionRuntimePlacement::Worker,
        true,
        publication.root_agent_id.0 == baseline.agent_id,
        publication.metadata.source.revision == source_revision,
        baseline
            .runtime
            .as_ref()
            .is_none_or(|runtime| expected_backend == runtime),
    )
}

#[cfg(kani)]
#[kani::proof]
fn frozen_worker_agent_publication_is_exact_or_fails_closed() {
    let has_frozen_revision: bool = kani::any();
    let worker_placement: bool = kani::any();
    let publication_present: bool = kani::any();
    let agent_id_matches: bool = kani::any();
    let source_revision_matches: bool = kani::any();
    let runtime_matches: bool = kani::any();
    let decision = frozen_agent_publication_facts(
        has_frozen_revision,
        worker_placement,
        publication_present,
        agent_id_matches,
        source_revision_matches,
        runtime_matches,
    );

    if has_frozen_revision && worker_placement {
        assert_eq!(
            decision == FrozenAgentPublicationDecision::Exact,
            publication_present && agent_id_matches && source_revision_matches && runtime_matches
        );
        assert_ne!(decision, FrozenAgentPublicationDecision::OptionalMissing);
    }
    if decision == FrozenAgentPublicationDecision::Exact {
        assert!(has_frozen_revision);
        assert!(publication_present);
        assert!(agent_id_matches && source_revision_matches && runtime_matches);
    }
    if decision == FrozenAgentPublicationDecision::MissingRequired {
        assert!(has_frozen_revision && worker_placement && !publication_present);
    }
}

#[cfg(test)]
mod frozen_agent_publication_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn worker_publication_decision_table_is_total_and_fail_closed() {
        for has_revision in [false, true] {
            for worker in [false, true] {
                for present in [false, true] {
                    for id_matches in [false, true] {
                        for revision_matches in [false, true] {
                            for runtime_matches in [false, true] {
                                let decision = frozen_agent_publication_facts(
                                    has_revision,
                                    worker,
                                    present,
                                    id_matches,
                                    revision_matches,
                                    runtime_matches,
                                );
                                let exact =
                                    present && id_matches && revision_matches && runtime_matches;
                                if has_revision && worker {
                                    assert_eq!(
                                        decision == FrozenAgentPublicationDecision::Exact,
                                        exact
                                    );
                                }
                                assert_eq!(
                                    decision == FrozenAgentPublicationDecision::MissingRequired,
                                    has_revision && worker && !present
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn effective_publication_system_prompt_decision_table_is_exact() {
        // Cause/effect graph: C1 selection is Inherit, Clear, or Replace; C2 an
        // Agent publication carries original instructions. Effects: E1 retain
        // them byte-for-byte; E2 emit an empty system instruction; E3 emit only
        // the Session replacement; E4 changed executable semantics recompute a
        // self-consistent fingerprint. Decision rules are the three rows below.
        let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "provider", "model", "genai",
            ))
            .instructions("agent instructions")
            .build();
        for (selection, expected, changed) in [
            (
                crate::SessionSystemPromptSelection::Inherit,
                "agent instructions",
                false,
            ),
            (crate::SessionSystemPromptSelection::Clear, "", true),
            (
                crate::SessionSystemPromptSelection::Replace("session instructions".into()),
                "session instructions",
                true,
            ),
        ] {
            let projected =
                project_effective_agent_publication(None, &selection, "workspace", base.clone())
                    .expect("valid effective publication");
            assert_eq!(projected.resolved_spec.instructions, expected);
            assert_eq!(projected.fingerprint != base.fingerprint, changed);
            assert_eq!(
                projected.fingerprint,
                projected.resolved_spec.catalog_fingerprint
            );
        }
    }

    proptest! {
        #[test]
        fn effective_publication_projection_is_idempotent(prompt in ".{0,128}") {
            // Property design: cause C1 is any valid replacement text, including
            // empty and Unicode; effect E1 is exact replacement and E2 is a
            // second projection producing the identical snapshot/fingerprint.
            // This covers the replay rule that Coordinator and Worker may both
            // observe the same already-effective immutable publication.
            let selection = crate::SessionSystemPromptSelection::Replace(prompt.clone());
            let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "genai",
                ))
                .instructions("agent instructions")
                .build();
            let once = project_effective_agent_publication(
                None,
                &selection,
                "workspace",
                base,
            ).expect("valid first projection");
            let twice = project_effective_agent_publication(
                None,
                &selection,
                "workspace",
                once.clone(),
            ).expect("valid replay projection");
            prop_assert_eq!(&once.resolved_spec.instructions, &prompt);
            prop_assert_eq!(twice, once);
        }
    }
}

/// Canonical Session-realization lease boundary. A lease is half-open: it is
/// live strictly before its expiry and stale at the exact expiry millisecond.
#[must_use]
pub const fn realization_lease_is_live_at(expires_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    expires_at_unix_ms > now_unix_ms
}

#[must_use]
const fn realization_lease_facts_authorize(
    same_owner: bool,
    same_runtime_incarnation: bool,
    same_epoch: bool,
    current_expires_at_unix_ms: u64,
    asserted_expires_at_unix_ms: u64,
    now_unix_ms: u64,
) -> bool {
    same_owner
        && same_runtime_incarnation
        && same_epoch
        && current_expires_at_unix_ms >= asserted_expires_at_unix_ms
        && realization_lease_is_live_at(current_expires_at_unix_ms, now_unix_ms)
}

/// Whether an asserted realization remains authorized by the aggregate's
/// current lease. A same-epoch renewal is a monotonic extension of one owner
/// incarnation, so work admitted under the shorter lease may finish while the
/// renewed current lease is live. Owner, incarnation, or epoch replacement
/// still fences the assertion.
#[must_use]
pub fn realization_lease_authorizes(
    current: &SessionRealizationLease,
    asserted: &SessionRealizationLease,
    now_unix_ms: u64,
) -> bool {
    realization_lease_facts_authorize(
        current.owner == asserted.owner,
        current.runtime_incarnation == asserted.runtime_incarnation,
        current.epoch == asserted.epoch,
        current.expires_at_unix_ms,
        asserted.expires_at_unix_ms,
        now_unix_ms,
    )
}

#[cfg(kani)]
#[kani::proof]
fn realization_renewal_never_widens_owner_epoch_or_expiry_authority() {
    let same_owner = kani::any();
    let same_runtime_incarnation = kani::any();
    let same_epoch = kani::any();
    let current_expires_at_unix_ms = kani::any();
    let asserted_expires_at_unix_ms = kani::any();
    let now_unix_ms = kani::any();
    let authorized = realization_lease_facts_authorize(
        same_owner,
        same_runtime_incarnation,
        same_epoch,
        current_expires_at_unix_ms,
        asserted_expires_at_unix_ms,
        now_unix_ms,
    );
    assert_eq!(
        authorized,
        same_owner
            && same_runtime_incarnation
            && same_epoch
            && current_expires_at_unix_ms >= asserted_expires_at_unix_ms
            && current_expires_at_unix_ms > now_unix_ms
    );
}

/// Whether the current exact-generation fence is the asserted fence or a
/// monotonic lease extension of it. Logical generation, owner incarnation, and
/// epoch remain immutable; only expiry may advance. Control uses this relation
/// to ask an in-flight phase driver to catch up instead of treating its already
/// authorized predecessor receipt as a conflicting generation.
#[must_use]
pub fn realization_generation_authorizes(
    current: &McpGenerationRef,
    asserted: &McpGenerationRef,
) -> bool {
    current.session_id == asserted.session_id
        && current.attachment_id == asserted.attachment_id
        && current.generation == asserted.generation
        && current.runtime_incarnation == asserted.runtime_incarnation
        && current.lease_epoch == asserted.lease_epoch
        && current.lease_expires_at_unix_ms >= asserted.lease_expires_at_unix_ms
}

/// Opaque Runtime assignment selected outside the Session domain. Worker and
/// local-process identities are mapped to these strings at the authenticated
/// application edge; the aggregate never imports their protocol vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationTarget {
    pub owner: String,
    pub runtime_incarnation: String,
    pub lease_expires_at_unix_ms: u64,
    /// Explicitly extend an existing lease for the same owner/incarnation.
    /// Ordinary create/hot-update commands leave this false, so a later wall
    /// clock alone cannot turn unrelated realization work into a renewal.
    #[serde(default)]
    pub renew_existing_lease: bool,
    /// Explicitly replace a live lease owned by another logical Runtime.
    /// Only a topology edge holding the current execution claim may set this;
    /// ordinary application callers leave it false and therefore cannot steal
    /// a live Session projection. Renewal and reassignment are mutually
    /// exclusive operations.
    #[serde(default)]
    pub reassign_existing_lease: bool,
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
    pub lease: SessionRealizationLease,
    pub action: SessionRealizationAction,
}

/// One cold Worker assignment for an already-fenced terminal Session.
///
/// Cleanup commands deliberately do not travel in this assignment. The Worker
/// installs the frozen projection and then polls
/// [`SessionRealizationControl::terminal_cleanup_commands`] plus the root-only
/// [`SessionRealizationControl::terminal_repository_publication_command`]
/// projection, keeping [`crate::SessionCleanupOperation`] as the only durable
/// work queue and completion registry.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTerminalCleanupAssignment {
    pub session_id: String,
    pub projection: FrozenSessionProjection,
    pub lease: SessionRealizationLease,
}

/// One aggregate-derived terminal Repository publication together with the
/// Session row's immutable owning Workspace. Coordinator adapters project both
/// facts through the same Control read so an authenticated Worker cannot choose
/// a different tenant coordinate for the Repository transport hop.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRepositoryPublicationProjection {
    pub workspace_id: String,
    pub command: crate::SessionRepositoryPublicationCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeginSessionRealization {
    pub session_id: String,
    pub target: SessionRealizationTarget,
}

/// Persisted initial-realization retry budget state. Runtime leases fence who
/// may act; this value records how many distinct fenced assignments attempted
/// the initial Environment so retry policy survives process and Worker loss.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationProgress {
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The exact claimed Run whose realization effect produced `last_error`.
    /// `None` preserves create/recovery failures that have no Run owner and
    /// keeps historical rows readable. Public projections use this identity to
    /// prefer the more precise committed Run lifecycle over a second Session
    /// realization error for the same cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_source_run_id: Option<Box<awaken_agent_contract::agent::run::Id>>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActivateSessionRealization {
    pub session_id: String,
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
    pub session_id: String,
    pub lease: SessionRealizationLease,
    #[serde(default)]
    pub published: Vec<McpGenerationRef>,
    #[serde(default)]
    pub drained: Vec<McpGenerationRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailSessionRealization {
    pub session_id: String,
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SessionRealizationControlFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is not ready for this realization phase")]
    NotReady,
    #[error("Session realization retired after its driving work settled")]
    Retired,
    #[error("Session realization is terminal")]
    Terminal,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session changed concurrently")]
    Conflict,
    #[error("Session realization command is invalid: {0}")]
    Invalid(String),
    #[error("Session realization service is unavailable: {0}")]
    Unavailable(String),
}

/// Stable effect class for one Control failure at a Worker/Run boundary.
///
/// The Session contract owns this classification so HTTP, embedded, and remote
/// Workers cannot independently reinterpret the same durable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationControlDisposition {
    NotReady,
    Retryable,
    Terminal,
}

impl SessionRealizationControlFailure {
    #[must_use]
    pub const fn disposition(&self) -> SessionRealizationControlDisposition {
        match self {
            Self::NotReady => SessionRealizationControlDisposition::NotReady,
            Self::StaleOwnership | Self::Conflict | Self::Unavailable(_) => {
                SessionRealizationControlDisposition::Retryable
            }
            Self::NotFound | Self::Retired | Self::Terminal | Self::Invalid(_) => {
                SessionRealizationControlDisposition::Terminal
            }
        }
    }

    /// Whether this reply conclusively denies the caller's exact current
    /// realization owner/fence. A retryable Run may be relinquished and
    /// executed by a newer owner, while the stale physical projection itself
    /// must stop immediately. NotReady, Conflict, and Unavailable do not prove
    /// that fact; their prior unexpired lease remains authoritative.
    #[must_use]
    pub const fn proves_current_realization_cannot_continue(&self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::Retired
                | Self::Terminal
                | Self::StaleOwnership
                | Self::Invalid(_)
        )
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_control_failure_disposition_is_total_exact_and_fail_closed() {
    let selector = kani::any::<u8>() % 8;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    // This oracle is deliberately derived from the symbolic variant selector,
    // not from `failure.disposition()`: changing the production table cannot
    // change the expected result at the same time.
    let expected = match selector {
        1 => SessionRealizationControlDisposition::NotReady,
        4 | 5 | 7 => SessionRealizationControlDisposition::Retryable,
        _ => SessionRealizationControlDisposition::Terminal,
    };

    assert_eq!(failure.disposition(), expected);
}

#[cfg(kani)]
#[kani::proof]
fn realization_ownership_loss_proof_is_total_and_exact() {
    let selector = kani::any::<u8>() % 8;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    assert_eq!(
        failure.proves_current_realization_cannot_continue(),
        matches!(selector, 0 | 2 | 3 | 4 | 6)
    );
}

/// Driving port for the Control-owned realization state machine. It performs no
/// Runtime I/O; topology adapters execute the returned action and submit exact
/// receipts/acknowledgements to the next phase.
#[async_trait]
pub trait SessionRealizationControl: Send + Sync {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure>;

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure>;

    /// Claim the next terminal Session whose physical realization was lost
    /// with a prior Worker process. The returned assignment contains only the
    /// facts needed to rebuild that process-local projection; callers must use
    /// [`Self::terminal_cleanup_commands`] for the immutable cleanup commands.
    /// Implementations that do not own external Worker placement return `None`.
    async fn claim_next_terminal_cleanup(
        &self,
        _target: SessionRealizationTarget,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        Ok(None)
    }

    /// Project the exact terminal-cleanup commands owned by this realization
    /// lease. `None` means the Session has no terminal fence; `Some([])` means
    /// terminal cleanup is fenced but not currently executable (or all receipts
    /// are already recorded). Remote Workers poll this existing Control channel
    /// so the durable [`crate::SessionCleanupOperation`] remains the only queue.
    async fn terminal_cleanup_commands(
        &self,
        _session_id: &str,
        _lease: &SessionRealizationLease,
    ) -> Result<Option<Vec<crate::SessionCleanupCommand>>, SessionRealizationControlFailure> {
        Ok(None)
    }

    /// Project the one root Repository publication command already frozen in
    /// the terminal cleanup operation together with the Session row's canonical
    /// Workspace. `None` means there is no publication intent, a child cleanup
    /// is still pending, or the exact receipt is already durable.
    /// Implementations must validate the same realization lease used by ordinary
    /// terminal cleanup commands.
    async fn terminal_repository_publication_command(
        &self,
        _session_id: &str,
        _lease: &SessionRealizationLease,
    ) -> Result<Option<SessionRepositoryPublicationProjection>, SessionRealizationControlFailure>
    {
        Ok(None)
    }

    /// Admit one exact root Repository publication receipt into the existing
    /// terminal cleanup operation. The explicit Session id is routing input,
    /// not duplicated receipt authority; implementations re-derive the command
    /// from that Session root before the root-CAS mutation.
    async fn record_terminal_repository_publication_receipt(
        &self,
        _session_id: &str,
        _lease: &SessionRealizationLease,
        _receipt: crate::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "remote Session Repository publication is unsupported".into(),
        ))
    }

    /// Admit one exact Runtime completion into the existing durable terminal
    /// cleanup operation. Implementations must verify the command identity and
    /// current realization generation before recording it.
    async fn record_terminal_cleanup_completion(
        &self,
        _lease: &SessionRealizationLease,
        _completion: crate::SessionCleanupCompletion,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "remote Session terminal cleanup is unsupported".into(),
        ))
    }
}

/// Topology-specific projection installation used by the one realization
/// driver. Local Managed execution and a remote Worker both install the same
/// complete frozen projection through `SessionRuntime`; this port selects only
/// topology-specific effects and owns no lifecycle transition or desired state.
#[async_trait]
pub trait SessionProjectionSynchronizer: Send + Sync {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &FrozenSessionProjection,
        lease: &SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError>;
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

/// Canonical Stage → Activate → Publish/Drain → Acknowledge driver.
///
/// Local and remote topology adapters share this algorithm. They may differ in
/// how a frozen projection is installed and how MCP effects are transported,
/// but cannot acquire a second ordering, cleanup, receipt, or failure path.
async fn publish_mcp_projection(
    mcp: &dyn McpAttachmentRealizer,
    generation: &crate::McpGenerationRef,
) -> Result<(), RunError> {
    let receipt = mcp
        .publish_mcp_generation_receipt(generation.clone())
        .await?;
    receipt
        .verify(generation, crate::McpProjectionEffectKind::Publish)
        .map_err(|_| {
            RunError::classified(
                "mcp_receipt_mismatch",
                "Runtime returned a receipt for another MCP publication",
            )
        })
}

async fn drain_mcp_projection(
    mcp: &dyn McpAttachmentRealizer,
    generation: &crate::McpGenerationRef,
) -> Result<(), RunError> {
    let receipt = mcp.drain_mcp_generation_receipt(generation.clone()).await?;
    receipt
        .verify(generation, crate::McpProjectionEffectKind::Drain)
        .map_err(|_| {
            RunError::classified(
                "mcp_receipt_mismatch",
                "Runtime returned a receipt for another MCP drain",
            )
        })
}

pub async fn drive_session_realization(
    session_id: &str,
    source_run_id: Option<awaken_agent_contract::agent::run::Id>,
    control: &dyn SessionRealizationControl,
    synchronizer: &dyn SessionProjectionSynchronizer,
    mcp: &dyn McpAttachmentRealizer,
    mut directive: SessionRealizationDirective,
) -> Result<(), SessionRealizationDriveError> {
    for _ in 0..4 {
        let prepare_session = match &directive.action {
            SessionRealizationAction::Stage {
                prepare_session, ..
            } => *prepare_session,
            SessionRealizationAction::Publish { .. } | SessionRealizationAction::Complete => false,
        };
        if let Err(error) = synchronizer
            .synchronize_session_projection(
                session_id,
                &directive.projection,
                &directive.lease,
                prepare_session,
            )
            .await
        {
            let _ = control
                .fail_session_realization(FailSessionRealization {
                    session_id: session_id.to_string(),
                    lease: directive.lease,
                    prepared_resource_revision: prepare_session
                        .then_some(directive.projection.resource_revision),
                    retryable: error.kind == crate::RunErrorKind::Unavailable,
                    source_run_id: source_run_id.clone(),
                    reason: error.to_string(),
                })
                .await;
            return Err(error.into());
        }
        match directive.action.clone() {
            SessionRealizationAction::Stage {
                prepare_session,
                mcp_stages,
            } => {
                let mut receipts = Vec::with_capacity(mcp_stages.len());
                let effect = async {
                    for request in mcp_stages {
                        let receipt = mcp.stage_mcp_attachment(request.clone()).await?;
                        receipt.verify(&request).map_err(|_| {
                            RunError::classified(
                                "mcp_receipt_mismatch",
                                "Runtime returned a receipt for another MCP realization",
                            )
                        })?;
                        receipts.push(receipt);
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    for receipt in &receipts {
                        let _ = drain_mcp_projection(mcp, &receipt.generation).await;
                    }
                    let _ = control
                        .fail_session_realization(FailSessionRealization {
                            session_id: session_id.to_string(),
                            lease: directive.lease,
                            prepared_resource_revision: prepare_session
                                .then_some(directive.projection.resource_revision),
                            retryable: error.kind == crate::RunErrorKind::Unavailable,
                            source_run_id: source_run_id.clone(),
                            reason: error.to_string(),
                        })
                        .await;
                    return Err(error.into());
                }
                directive = match control
                    .activate_session_realization(ActivateSessionRealization {
                        session_id: session_id.to_string(),
                        lease: directive.lease,
                        prepared_resource_revision: prepare_session
                            .then_some(directive.projection.resource_revision),
                        mcp_receipts: receipts.clone(),
                    })
                    .await
                {
                    Ok(next) => next,
                    Err(error) => {
                        for receipt in receipts {
                            let _ = drain_mcp_projection(mcp, &receipt.generation).await;
                        }
                        return Err(error.into());
                    }
                };
            }
            SessionRealizationAction::Publish { publish, drain } => {
                let effect = async {
                    for generation in &publish {
                        publish_mcp_projection(mcp, generation).await?;
                    }
                    for generation in &drain {
                        drain_mcp_projection(mcp, generation).await?;
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    let _ = control
                        .fail_session_realization(FailSessionRealization {
                            session_id: session_id.to_string(),
                            lease: directive.lease,
                            prepared_resource_revision: None,
                            retryable: error.kind == crate::RunErrorKind::Unavailable,
                            source_run_id: source_run_id.clone(),
                            reason: error.to_string(),
                        })
                        .await;
                    return Err(error.into());
                }
                directive = control
                    .acknowledge_session_realization(AcknowledgeSessionRealization {
                        session_id: session_id.to_string(),
                        lease: directive.lease,
                        published: publish,
                        drained: drain,
                    })
                    .await?;
            }
            SessionRealizationAction::Complete => return Ok(()),
        }
        // A Control transition may complete on the final permitted action. Do
        // not require a redundant fifth loop iteration merely to observe the
        // terminal directive after a same-epoch renewal catch-up.
        if matches!(directive.action, SessionRealizationAction::Complete) {
            return Ok(());
        }
    }
    Err(SessionRealizationDriveError::DidNotConverge)
}

#[cfg(test)]
mod tests {
    use super::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        FailSessionRealization, SessionProjectionInstallMode, SessionRealizationControl,
        SessionRealizationControlDisposition, SessionRealizationControlFailure,
        SessionRealizationDirective, realization_generation_authorizes,
        realization_lease_authorizes, realization_lease_is_live_at,
    };
    use crate::{McpAttachmentId, McpGeneration, McpGenerationRef, SessionRealizationLease};

    struct MinimalControl;

    #[test]
    fn projection_install_mode_effect_decision_table_is_closed() {
        // Cause/effect graph: C1 dispatch versus realization; C2 realization requests
        // preparation. Effects: E1 prepare execution context; E2 adopt a resident
        // Environment; E3 carry a realization lease. Decision rules:
        // D1 dispatch => E1,!E2,!E3; D2 realization+prepare => E1,E2,E3;
        // D3 realization+no-prepare => !E1,!E2,E3.
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: 2,
        };
        let rules = [
            (SessionProjectionInstallMode::Dispatch, true, false, false),
            (
                SessionProjectionInstallMode::Realization {
                    lease: lease.clone(),
                    prepare_session: true,
                },
                true,
                true,
                true,
            ),
            (
                SessionProjectionInstallMode::Realization {
                    lease,
                    prepare_session: false,
                },
                false,
                false,
                true,
            ),
        ];
        for (mode, prepares, adopts, has_lease) in rules {
            assert_eq!(mode.prepares_session(), prepares);
            assert_eq!(mode.adopts_resident_environment(), adopts);
            assert_eq!(mode.realization_lease().is_some(), has_lease);
        }
    }

    #[async_trait::async_trait]
    impl SessionRealizationControl for MinimalControl {
        async fn begin_session_realization(
            &self,
            _command: BeginSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn activate_session_realization(
            &self,
            _command: ActivateSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn acknowledge_session_realization(
            &self,
            _command: AcknowledgeSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }

        async fn fail_session_realization(
            &self,
            _command: FailSessionRealization,
        ) -> Result<(), SessionRealizationControlFailure> {
            unreachable!("not exercised by the terminal port-default tests")
        }
    }

    #[tokio::test]
    async fn terminal_repository_publication_control_defaults_do_not_invent_work_or_receipts() {
        // Control-default cause/effect table: C1 a topology has not implemented
        // remote Repository publication; C2 it polls for a command or submits a
        // receipt. Effects: E1 polling returns None (no parallel queue); E2
        // receipt admission fails Invalid (no synthetic root-CAS evidence).
        // Rules D1=C1+poll=>E1; D2=C1+record=>E2.
        let control = MinimalControl;
        let lease = SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: 2,
        };
        assert!(
            control
                .terminal_repository_publication_command("session", &lease)
                .await
                .unwrap()
                .is_none(),
            "D1/E1"
        );
        let receipt: crate::SessionRepositoryPublicationReceipt =
            serde_json::from_value(serde_json::json!({
                "command_fingerprint": "command",
                "effect_receipt": {
                    "repository_id": "repo",
                    "source_remote_url": "https://example.test/repo.git",
                    "branch": "awf/work",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                },
                "receipt_fingerprint": "receipt"
            }))
            .unwrap();
        assert!(
            matches!(
                control
                    .record_terminal_repository_publication_receipt("session", &lease, receipt,)
                    .await,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "D2/E2"
        );
    }

    /// Cause C1: expiry is strictly after observation time. Only C1 authorizes
    /// another effect; equality is already outside the half-open lease.
    ///
    /// | Rule | expiry vs now | live |
    /// |---|---|---|
    /// | L1 | before | false |
    /// | L2 | equal | false |
    /// | L3 | after | true |
    #[test]
    fn realization_lease_boundary_decision_table() {
        for (expiry, now, expected, rule) in [
            (9, 10, false, "L1"),
            (10, 10, false, "L2"),
            (11, 10, true, "L3"),
        ] {
            assert_eq!(
                realization_lease_is_live_at(expiry, now),
                expected,
                "{rule}"
            );
        }
    }

    /// Realization-authorization cause/effect graph: C1 owner/incarnation/epoch
    /// identity is unchanged; C2 current expiry is equal to or later than the
    /// asserted expiry; C3 current lease is live. E1 authorizes the in-flight
    /// phase; otherwise E2 fences it. An asserted lease may itself have elapsed
    /// after admission because a live same-epoch extension owns continuation.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | A1 | yes | yes | yes | E1 |
    /// | A2 | no | any | yes | E2 |
    /// | A3 | yes | no | yes | E2 |
    /// | A4 | yes | yes | no | E2 |
    #[test]
    fn realization_lease_authorization_decision_table() {
        let asserted = SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 3,
            expires_at_unix_ms: 10,
        };
        for (rule, current, now, expected) in [
            ("A1 exact", asserted.clone(), 9, true),
            (
                "A1 renewed after asserted expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                true,
            ),
            (
                "A2 replaced owner",
                SessionRealizationLease {
                    owner: "worker-b".into(),
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                11,
                false,
            ),
            (
                "A3 regressed expiry",
                SessionRealizationLease {
                    expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                8,
                false,
            ),
            (
                "A4 current expired",
                SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                20,
                false,
            ),
        ] {
            assert_eq!(
                realization_lease_authorizes(&current, &asserted, now),
                expected,
                "{rule}"
            );
        }
    }

    /// Generation-fence cause/effect graph: C1 logical generation identity,
    /// owner incarnation, and epoch match; C2 current expiry is equal/later.
    /// E1 permits a phase catch-up without committing the predecessor effect;
    /// E2 rejects replacement, regression, or cross-generation input.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | G1 exact | yes | equal | E1 |
    /// | G2 renewed | yes | later | E1 |
    /// | G3 regression | yes | earlier | E2 |
    /// | G4 replacement | no | any | E2 |
    #[test]
    fn realization_generation_authorization_decision_table() {
        let asserted = McpGenerationRef {
            session_id: "session-a".into(),
            attachment_id: McpAttachmentId("browser".into()),
            generation: McpGeneration(1),
            runtime_incarnation: "worker-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: 10,
        };
        for (rule, current, expected) in [
            ("G1", asserted.clone(), true),
            (
                "G2",
                McpGenerationRef {
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                true,
            ),
            (
                "G3",
                McpGenerationRef {
                    lease_expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                false,
            ),
            (
                "G4",
                McpGenerationRef {
                    lease_epoch: 4,
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                false,
            ),
        ] {
            assert_eq!(
                realization_generation_authorizes(&current, &asserted),
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn realization_control_failures_round_trip_without_semantic_loss() {
        // Cause/effect graph: C1 transient backpressure, C2 retryable authority
        // or dependency failure, and C3 an absent/terminal/invalid aggregate.
        // Effects: E1 defer without crash accounting, E2 relinquish for the
        // existing retry path, and E3 absorb the Run instead of hot-looping.
        // Every cause also round-trips over the existing typed transport.
        //
        // | Rule | cause | disposition | current owner disproved |
        // | T1 | not ready | NotReady | no |
        // | T2a | stale | Retryable | yes |
        // | T2b | conflict/unavailable | Retryable | no |
        // | T3 | not found/retired/terminal/invalid | Terminal | yes |
        for (rule, failure, disposition, owner_disproved) in [
            (
                "T3 not found",
                SessionRealizationControlFailure::NotFound,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T1 not ready",
                SessionRealizationControlFailure::NotReady,
                SessionRealizationControlDisposition::NotReady,
                false,
            ),
            (
                "T3 retired",
                SessionRealizationControlFailure::Retired,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T3 terminal",
                SessionRealizationControlFailure::Terminal,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2a stale",
                SessionRealizationControlFailure::StaleOwnership,
                SessionRealizationControlDisposition::Retryable,
                true,
            ),
            (
                "T2b conflict",
                SessionRealizationControlFailure::Conflict,
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
            (
                "T3 invalid",
                SessionRealizationControlFailure::Invalid("bad target".into()),
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2b unavailable",
                SessionRealizationControlFailure::Unavailable("control offline".into()),
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
        ] {
            let wire = serde_json::to_value(&failure).expect(rule);
            let decoded: SessionRealizationControlFailure =
                serde_json::from_value(wire).expect(rule);
            assert_eq!(decoded, failure, "{rule}");
            assert_eq!(decoded.disposition(), disposition, "{rule}");
            assert_eq!(
                decoded.proves_current_realization_cannot_continue(),
                owner_disproved,
                "{rule}"
            );
        }
    }
}
