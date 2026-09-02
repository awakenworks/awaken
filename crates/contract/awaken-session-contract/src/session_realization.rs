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

mod repository_publication_projection;
pub use repository_publication_projection::SessionRepositoryPublicationProjection;
mod terminal_cleanup_authorization;
pub use terminal_cleanup_authorization::{
    SessionTerminalCleanupAssignment, SessionTerminalCleanupPreparationAuthorization,
    SessionTerminalCleanupWork,
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
    frozen_runtime_is_resolved: bool,
    model_candidate_present: bool,
) -> FrozenAgentPublicationDecision {
    if publication_present {
        if agent_id_matches && source_revision_matches && frozen_runtime_is_resolved {
            FrozenAgentPublicationDecision::Exact
        } else {
            FrozenAgentPublicationDecision::Mismatch
        }
    } else if has_frozen_revision {
        if worker_placement {
            FrozenAgentPublicationDecision::MissingRequired
        } else if frozen_runtime_is_resolved {
            FrozenAgentPublicationDecision::OptionalMissing
        } else {
            FrozenAgentPublicationDecision::Mismatch
        }
    } else if !frozen_runtime_is_resolved {
        FrozenAgentPublicationDecision::Mismatch
    } else if model_candidate_present {
        // A complete Session-local override is a frozen model publication even
        // when an optional Agent snapshot is unavailable. It must not be
        // collapsed into the genuinely publication-free legacy Host case.
        FrozenAgentPublicationDecision::OptionalMissing
    } else {
        FrozenAgentPublicationDecision::Unpinned
    }
}

fn publication_decision_for_coordinates(
    agent_id: &str,
    agent_revision: Option<u64>,
    runtime_placement: crate::SessionRuntimePlacement,
    model_override: Option<&crate::SessionModelOverride>,
    runtime: Option<&str>,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    let model_candidate = model_override
        .and_then(|model_override| model_override.publication.as_ref())
        .map(|publication| &publication.primary)
        .or_else(|| publication.map(|publication| &publication.resolved_spec.model_binding));
    let frozen_runtime_is_resolved = runtime.is_none_or(|runtime| {
        model_candidate.is_some_and(|candidate| candidate.binding().backend_ref == runtime)
    });
    frozen_agent_publication_facts(
        agent_revision.is_some(),
        runtime_placement == crate::SessionRuntimePlacement::Worker,
        publication.is_some(),
        publication.is_some_and(|publication| publication.root_agent_id.0 == agent_id),
        publication.is_some_and(|publication| {
            agent_revision.is_none_or(|revision| publication.metadata.source.revision == revision)
        }),
        frozen_runtime_is_resolved,
        model_candidate.is_some(),
    )
}

/// Compare a delivered executable publication with the immutable coordinates
/// in the Session baseline. Both Coordinator and Worker call this kernel, so a
/// transport cannot weaken the validation performed before dispatch.
#[must_use]
pub fn frozen_agent_publication_decision(
    baseline: &crate::SessionBaseline,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        &baseline.agent_id,
        baseline.agent_revision,
        baseline.runtime_placement,
        baseline.model_override.as_ref(),
        baseline.runtime.as_deref(),
        publication,
    )
}

/// Prospective-layout adapter for [`frozen_agent_publication_decision`]. Both
/// current Session baselines and pre-root layouts consume the same fact kernel;
/// protocol/application callers never select a provider independently.
#[must_use]
pub fn frozen_sandbox_layout_publication_decision(
    layout: &crate::SessionSandboxLayout,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        &layout.agent_id,
        layout.agent_revision,
        layout.runtime_placement,
        layout.model_override.as_ref(),
        layout.runtime.as_deref(),
        publication,
    )
}

/// Compatibility adapter for a pre-baseline local Session. It deliberately has
/// no frozen runtime or model override, so only an exact current Agent
/// publication or the genuinely publication-free Host path is admissible.
#[must_use]
pub fn legacy_agent_publication_decision(
    agent_id: &str,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        agent_id,
        None,
        crate::SessionRuntimePlacement::Local,
        None,
        None,
        publication,
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
    let model_candidate_present: bool = kani::any();
    let decision = frozen_agent_publication_facts(
        has_frozen_revision,
        worker_placement,
        publication_present,
        agent_id_matches,
        source_revision_matches,
        runtime_matches,
        model_candidate_present,
    );

    if has_frozen_revision && worker_placement {
        assert_eq!(
            decision == FrozenAgentPublicationDecision::Exact,
            publication_present && agent_id_matches && source_revision_matches && runtime_matches
        );
        assert_ne!(decision, FrozenAgentPublicationDecision::OptionalMissing);
    }
    if decision == FrozenAgentPublicationDecision::Exact {
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
                                for candidate_present in [false, true] {
                                    let decision = frozen_agent_publication_facts(
                                        has_revision,
                                        worker,
                                        present,
                                        id_matches,
                                        revision_matches,
                                        runtime_matches,
                                        candidate_present,
                                    );
                                    let exact = present
                                        && id_matches
                                        && revision_matches
                                        && runtime_matches;
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
                                    assert_eq!(
                                        decision == FrozenAgentPublicationDecision::Unpinned,
                                        !has_revision
                                            && !present
                                            && runtime_matches
                                            && !candidate_present
                                    );
                                }
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

/// Whether an asserted terminal generation is still the aggregate's exact
/// owner/epoch. Terminal cleanup may intentionally outlive the ordinary lease
/// wall-clock expiry after Work retirement; authenticated topology and the
/// aggregate's monotonic generation, rather than a second timer rule, fence it.
#[must_use]
pub fn realization_lease_generation_authorizes(
    current: &SessionRealizationLease,
    asserted: &SessionRealizationLease,
) -> bool {
    current.owner == asserted.owner
        && current.runtime_incarnation == asserted.runtime_incarnation
        && current.epoch == asserted.epoch
        && current.expires_at_unix_ms >= asserted.expires_at_unix_ms
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
    /// Explicitly replace a live lease owned by another logical Runtime.
    /// Only a topology edge holding the current execution claim may set this;
    /// ordinary application callers leave it false and therefore cannot steal
    /// a live Session projection. Lease extension is a separate exact-fence
    /// command and can never be smuggled through this assignment value.
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
    let needs_assignment =
        !current_is_live || !same_runtime_incarnation || (reassign_existing_lease && !same_owner);
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
    }
}

#[cfg(test)]
mod realization_assignment_tests {
    use super::*;

    #[test]
    fn assignment_follows_the_owner_incarnation_epoch_decision_table() {
        // Cause/effect graph: C1 a current fence exists and is live; C2 owner
        // matches; C3 Runtime incarnation matches; C4 authenticated topology
        // explicitly permits cross-owner reassignment; C5 epoch can advance.
        // Effects: E1 reuse only the exact live owner/incarnation; E2 reject a
        // live foreign owner; E3 every new assignment mints exactly successor
        // epoch (or 1 from absence); E4 exhaustion fails closed.
        //
        // | Rule | Live | Owner | Incarnation | Reassign | Effect |
        // |---|---|---|---|---|---|
        // | R1 | yes | same | same | no | E1 reuse |
        // | R2 | yes | other | any | no | E2 stale |
        // | R3 | no | any | any | any | E3 successor |
        // | R4 | yes | same | other | no | E3 successor |
        // | R5 | yes | other | any | yes | E3 successor |
        // | R6 | assignment | any | any | any | E4 exhausted |
        assert_eq!(
            session_realization_assignment(Some(7), true, true, true, false),
            SessionRealizationAssignment::Reuse,
            "R1/E1"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, false, false, false),
            SessionRealizationAssignment::StaleOwnership,
            "R2/E2"
        );
        assert_eq!(
            session_realization_assignment(Some(7), false, false, false, false),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R3/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, true, false, false),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R4/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(7), true, false, false, true),
            SessionRealizationAssignment::Assign { epoch: 8 },
            "R5/E3"
        );
        assert_eq!(
            session_realization_assignment(Some(u64::MAX), false, true, true, false),
            SessionRealizationAssignment::EpochExhausted,
            "R6/E4"
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
    pub lease: SessionRealizationLease,
    pub action: SessionRealizationAction,
}

#[cfg(test)]
mod repository_publication_projection_tests {
    use super::{SessionRealizationLease, SessionRepositoryPublicationProjection};

    fn command() -> crate::SessionRepositoryPublicationCommand {
        crate::SessionRepositoryPublicationCommand {
            session_id: "publication-session".into(),
            effect_id: "publication-effect".into(),
            intent: serde_json::from_value(serde_json::json!({
                "input": {
                    "binding_id": "repository-binding",
                    "source": {
                        "kind": "repository",
                        "repository_id": "repository",
                        "config": {
                            "repository_id": "repository",
                            "version": 1,
                            "remote_url": "https://git.invalid/repository.git",
                            "initial_branch": "main"
                        }
                    },
                    "mount_path": "/workspace/repository",
                    "access": "read_write"
                },
                "expectation": {
                    "branch": "awf/publication",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                }
            }))
            .expect("valid publication intent"),
        }
    }

    #[test]
    fn repository_publication_projection_wire_is_closed_and_lease_canonical() {
        // Cause/effect graph: C1 the projection has an immutable Workspace and
        // command; C2 current_lease has a complete canonical generation and a
        // nonzero expiry; C3 either the outer projection or nested lease carries
        // an unknown/missing field. Effects: E1 exact values round-trip; E2
        // unknown or missing authority is rejected; E3 structurally unusable
        // lease identity/expiry is rejected before reaching a transport.
        //
        // | Rule | command/Workspace | current lease | wire | Effect |
        // |---|---|---|---|---|
        // | R1 | exact | canonical | closed | E1 |
        // | R2 | exact | canonical | outer/nested unknown | E2 |
        // | R3 | exact | missing | closed | E2 |
        // | R4 | exact | blank owner or zero expiry | closed | E3 |
        let projection = SessionRepositoryPublicationProjection::try_new(
            "workspace".into(),
            command(),
            SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch: 7,
                expires_at_unix_ms: 20,
            },
        )
        .expect("R1/E1 canonical projection");
        let encoded = serde_json::to_value(&projection).expect("R1/E1 encode");
        assert_eq!(
            serde_json::from_value::<SessionRepositoryPublicationProjection>(encoded.clone())
                .expect("R1/E1 decode"),
            projection,
            "R1/E1"
        );

        for (rule, mut invalid) in [
            ("R2 outer", encoded.clone()),
            ("R2 nested", encoded.clone()),
        ] {
            if rule == "R2 outer" {
                invalid["parallel_authority"] = serde_json::json!(true);
            } else {
                invalid["current_lease"]["parallel_owner"] = serde_json::json!("other");
            }
            assert!(
                serde_json::from_value::<SessionRepositoryPublicationProjection>(invalid).is_err(),
                "{rule}/E2"
            );
        }

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("R3 object")
            .remove("current_lease");
        assert!(
            serde_json::from_value::<SessionRepositoryPublicationProjection>(missing).is_err(),
            "R3/E2"
        );

        for (rule, field, value) in [
            ("R4 owner", "owner", serde_json::json!(" ")),
            ("R4 expiry", "expires_at_unix_ms", serde_json::json!(0)),
        ] {
            let mut invalid = encoded.clone();
            invalid["current_lease"][field] = value;
            assert!(
                serde_json::from_value::<SessionRepositoryPublicationProjection>(invalid).is_err(),
                "{rule}/E3"
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeginSessionRealization {
    pub session_id: String,
    pub target: SessionRealizationTarget,
}

/// Extend one already-authoritative realization lease without replaying the
/// projection/phase protocol. The asserted lease is the caller's exact fence;
/// Control may return a monotonic same-epoch successor but can never change its
/// owner, Runtime incarnation, or epoch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenewSessionRealization {
    pub session_id: String,
    pub asserted_lease: SessionRealizationLease,
    pub requested_expires_at_unix_ms: u64,
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

    /// Renew only the existing realization fence. Initial assignment and
    /// desired-state progression remain owned by `begin`; heartbeat callers
    /// must not rebuild or reinstall an unchanged frozen projection merely to
    /// prove continued ownership.
    async fn renew_session_realization(
        &self,
        _command: RenewSessionRealization,
    ) -> Result<SessionRealizationLease, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "Session realization lease renewal is unsupported".into(),
        ))
    }

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
    /// [`Self::terminal_cleanup_work`] for the immutable cleanup commands and a
    /// current readback of the same frozen projection.
    /// Implementations that do not own external Worker placement return `None`.
    async fn claim_next_terminal_cleanup(
        &self,
        _target: SessionRealizationTarget,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        Ok(None)
    }

    /// Project the exact terminal assignment and closed next action owned by this
    /// realization generation from one Session-root snapshot. `None` means the
    /// Session has no terminal fence or is durably complete; `Some(work)` with
    /// `Waiting` action means cleanup is fenced but not currently executable.
    /// Remote Workers poll this existing Control channel so the durable
    /// [`crate::SessionCleanupOperation`] remains the only queue.
    async fn terminal_cleanup_work(
        &self,
        _session_id: &str,
        _lease: &SessionRealizationLease,
    ) -> Result<Option<SessionTerminalCleanupWork>, SessionRealizationControlFailure> {
        Ok(None)
    }

    /// Re-derive one exact terminal effect and its canonical Workspace from the
    /// aggregate immediately before an external adapter performs I/O. This is a
    /// read of [`crate::SessionCleanupOperation`], not another claim or queue.
    async fn authorize_terminal_cleanup_effect(
        &self,
        _effect: &crate::SessionTerminalCleanupEffect,
    ) -> Result<SessionTerminalCleanupPreparationAuthorization, SessionRealizationControlFailure>
    {
        Err(SessionRealizationControlFailure::Invalid(
            "terminal cleanup effect authorization is unsupported".into(),
        ))
    }

    /// Re-derive the single aggregate-wide physical disposal from the
    /// durable preparation set and the current realization lease. This is a
    /// read-only authorization edge; provider deletion remains with the
    /// Runtime receiving the exact effect.
    async fn authorize_terminal_cleanup_disposal(
        &self,
        _effect: &crate::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "terminal cleanup disposal authorization is unsupported".into(),
        ))
    }

    /// Re-derive an exact checkpoint source-release Artifact effect from the
    /// current Session root. This authorizes only live output publication while
    /// `Suspending/ReadyToDispose` still owns the durable checkpoint; it neither
    /// advances the Environment state nor creates a cleanup receipt.
    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        _session_id: &str,
        _operation: &crate::SessionEnvironmentOperation,
    ) -> Result<String, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "checkpoint source-release Artifact authorization is unsupported".into(),
        ))
    }

    /// Re-derive one exact terminal Memory target from the Session root. The
    /// returned value adds the canonical Workspace only after the aggregate has
    /// matched the intent against its active input and current Sandbox handle.
    async fn authorize_terminal_memory_intent(
        &self,
        _intent: &crate::SessionTerminalMemoryIntent,
    ) -> Result<crate::SessionTerminalMemoryTarget, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "terminal Memory reconciliation authorization is unsupported".into(),
        ))
    }

    /// Project the one root Repository publication command already frozen in
    /// the terminal cleanup operation together with the Session row's canonical
    /// Workspace. `None` means there is no publication intent, a child cleanup
    /// is still pending, or the exact receipt/rejection is already durable.
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

    /// Admit one exact permanent Repository publication rejection into the same
    /// cleanup sidecar and lease-fenced root CAS as a successful receipt. This is
    /// a terminal effect observation, not a Worker-local retry queue.
    async fn record_terminal_repository_publication_rejection(
        &self,
        _session_id: &str,
        _lease: &SessionRealizationLease,
        _rejection: crate::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "remote Session Repository publication is unsupported".into(),
        ))
    }

    /// Admit one exact source-dependent preparation into the existing Session
    /// cleanup operation. The aggregate verifies both the asserted current
    /// lease and the preparation's persisted predecessor lease before its CAS.
    async fn record_terminal_cleanup_preparation(
        &self,
        _lease: &SessionRealizationLease,
        _preparation: crate::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "remote Session terminal cleanup preparation is unsupported".into(),
        ))
    }

    /// Admit the one exact physical-disposal receipt and atomically retire
    /// the aggregate's Environment/Resource projection. Implementations must
    /// re-derive the command and current lease before committing.
    async fn record_terminal_cleanup_disposal(
        &self,
        _lease: &SessionRealizationLease,
        _receipt: crate::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "remote Session terminal cleanup disposal is unsupported".into(),
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
    loop {
        let previous = directive.clone();
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
        // A lease extension can require any number of monotonic Stage/Activate
        // catch-up rounds. The protocol therefore rejects only a Control
        // transition that made no observable progress; an arbitrary iteration
        // cap would turn a healthy, still-owned execution into a false failure.
        if directive == previous {
            return Err(SessionRealizationDriveError::DidNotConverge);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        FailSessionRealization, RenewSessionRealization, SessionProjectionInstallMode,
        SessionRealizationControl, SessionRealizationControlDisposition,
        SessionRealizationControlFailure, SessionRealizationDirective,
        SessionTerminalCleanupPreparationAuthorization, realization_generation_authorizes,
        realization_lease_authorizes, realization_lease_is_live_at,
    };
    use crate::{McpAttachmentId, McpGeneration, McpGenerationRef, SessionRealizationLease};

    struct MinimalControl;

    #[tokio::test]
    async fn canonical_realization_renewal_port_is_exact_and_unsupported_by_default() {
        // Cause/effect graph: C1 renewal names a Session and the complete
        // owner/incarnation/epoch lease; C2 it requests only a new expiry; C3 a
        // topology does or does not implement the aggregate renewal CAS.
        // Effects: E1 the canonical wire round-trips every exact fact; E2 the
        // default port fails closed and cannot manufacture a terminal-specific
        // assignment, queue, or second renewal authority.
        //
        // | Rule | exact lease | expiry | implementation | Effect |
        // |---|---|---|---|---|
        // | R1 | present | present | any | E1 |
        // | R2 | present | present | absent | E2 |
        let command = RenewSessionRealization {
            session_id: "terminal-session".into(),
            asserted_lease: SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch: 7,
                expires_at_unix_ms: 20,
            },
            requested_expires_at_unix_ms: 40,
        };
        let encoded = serde_json::to_value(&command).expect("R1/E1 encode");
        assert_eq!(
            serde_json::from_value::<RenewSessionRealization>(encoded)
                .expect("R1/E1 exact round trip"),
            command,
            "R1/E1"
        );
        assert!(
            matches!(
                MinimalControl.renew_session_realization(command).await,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "R2/E2"
        );
    }

    fn terminal_effect(
        thread_id: &str,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> crate::SessionTerminalCleanupEffect {
        crate::SessionTerminalCleanupEffect::new(
            crate::SessionCleanupCommand::new("session", thread_id, "terminal-root"),
            SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/boot".into(),
                epoch,
                expires_at_unix_ms,
            },
        )
    }

    #[test]
    fn terminal_preparation_authorization_is_exact_closed_and_root_only() {
        // Cause/effect graph: C1 the authorization echoes the exact effect; C2
        // its Workspace is non-empty; C3 inherited provider preparation is
        // absent, or belongs only to the root and precedes its realization
        // fence; C4 the wire contains only the closed schema. Effects: E1 exact
        // root/child values round-trip and verify; E2 cross-effect replay is
        // rejected; E3 child inheritance and non-successor inheritance are
        // rejected; E4 blank Workspace and unknown wire fields are rejected.
        //
        // | Rule | Effect binding | Workspace | inherited predecessor | wire | Effect |
        // | R1 | exact root | set | valid successor | closed | E1 |
        // | R2 | exact child | set | none | closed | E1 |
        // | R3 | foreign effect | set | any | closed | E2 |
        // | R4 | child | set | present | closed | E3 |
        // | R5 | root | set | later same-epoch fence | closed | E3 |
        // | R6 | root | blank | none | closed | E4 |
        // | R7 | exact | set | valid | unknown sibling | E4 |
        let root = terminal_effect("session", 4, 40);
        let child = terminal_effect("child", 4, 40);
        let inherited = awaken_provisioning_contract::SandboxDisposalPreparation::new(
            awaken_provisioning_contract::SandboxEffectFence::new(
                "continuation-preparation",
                "worker",
                "worker/boot",
                3,
                30,
            )
            .unwrap(),
            "continuation-fingerprint",
        )
        .unwrap();
        let root_authorization = SessionTerminalCleanupPreparationAuthorization::try_new(
            root.clone(),
            "workspace".into(),
            Some(inherited.clone()),
        )
        .expect("R1/E1");
        let encoded = serde_json::to_value(&root_authorization).unwrap();
        let decoded: SessionTerminalCleanupPreparationAuthorization =
            serde_json::from_value(encoded.clone()).expect("R1/E1 round trip");
        assert_eq!(decoded, root_authorization, "R1/E1 bytes");
        decoded.verify_for(&root).expect("R1/E1 exact effect");
        assert_eq!(decoded.workspace_id(), "workspace", "R1/E1 Workspace");
        assert_eq!(
            decoded.inherited_provider_disposal(),
            Some(&inherited),
            "R1/E1 predecessor"
        );

        let child_authorization = SessionTerminalCleanupPreparationAuthorization::try_new(
            child.clone(),
            "workspace".into(),
            None,
        )
        .expect("R2/E1");
        child_authorization.verify_for(&child).expect("R2/E1");
        assert!(child_authorization.inherited_provider_disposal().is_none());
        assert!(root_authorization.verify_for(&child).is_err(), "R3/E2");
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(
                child,
                "workspace".into(),
                Some(inherited),
            )
            .is_err(),
            "R4/E3"
        );

        let non_predecessor = awaken_provisioning_contract::SandboxDisposalPreparation::new(
            awaken_provisioning_contract::SandboxEffectFence::new(
                "later-preparation",
                "worker",
                "worker/boot",
                4,
                41,
            )
            .unwrap(),
            "later-fingerprint",
        )
        .unwrap();
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(
                root.clone(),
                "workspace".into(),
                Some(non_predecessor),
            )
            .is_err(),
            "R5/E3"
        );
        assert!(
            SessionTerminalCleanupPreparationAuthorization::try_new(root, " ".into(), None,)
                .is_err(),
            "R6/E4"
        );
        let mut unknown = encoded;
        unknown["parallel_authority"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<SessionTerminalCleanupPreparationAuthorization>(unknown)
                .is_err(),
            "R7/E4"
        );
    }

    fn legacy_resource_projection(bound: bool) -> super::FrozenSessionProjection {
        let baseline = crate::SessionBaseline::compile(crate::SessionBaselineInputs {
            environment: crate::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: crate::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: crate::EnvironmentFingerprint("environment-v1".into()),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: crate::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Workload,
                        "awaken.workload.acp",
                    ),
                    mcp_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                    resource_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                },
            },
            runtime_placement: crate::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        let mut environment = crate::SessionEnvironmentState::default();
        if bound {
            environment.set_resident("sandbox-binding");
        }
        super::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: crate::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment,
            resource_revision: 7,
            resources: Default::default(),
            previous_resource_manifest: None,
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    #[test]
    fn legacy_resource_transition_decision_table_is_fail_closed_for_effects() {
        // Cause/effect graph: C1 the legacy wire omits the aggregate-authored
        // previous manifest; C2 the durable Environment is bound or unbound;
        // C3 the caller either requires an effect-safe transition or proves it
        // will only validate an already-installed projection. Effects: E1 an
        // unbound Environment conservatively decodes empty->desired; E2 a bound
        // validation-only projection decodes desired->desired; E3 a bound
        // effect-capable projection fails closed. The typed use never changes
        // the unbound result.
        //
        // | Rule | C2 bound | C3 transition use | Effect |
        // |---|---|---|---|
        // | R1 | no | apply effects | E1 |
        // | R2 | no | validate installed | E1 |
        // | R3 | yes | apply effects | E3 |
        // | R4 | yes | validate installed | E2 |
        for (rule, bound, transition_use, expected_previous_revision) in [
            (
                "R1",
                false,
                super::FrozenResourceTransitionUse::ApplyEffects,
                Some(0),
            ),
            (
                "R2",
                false,
                super::FrozenResourceTransitionUse::ValidateInstalled,
                Some(0),
            ),
            (
                "R3",
                true,
                super::FrozenResourceTransitionUse::ApplyEffects,
                None,
            ),
            (
                "R4",
                true,
                super::FrozenResourceTransitionUse::ValidateInstalled,
                Some(7),
            ),
        ] {
            let projection = legacy_resource_projection(bound);
            let result = projection.resource_transition(transition_use);
            match expected_previous_revision {
                Some(expected) => {
                    let transition = result.expect(rule);
                    assert_eq!(transition.previous().revision, expected, "{rule}");
                    assert_eq!(transition.desired().revision, 7, "{rule}");
                }
                None => {
                    let error = result.expect_err(rule);
                    assert_eq!(error.code, "session_resource_transition_missing", "{rule}");
                }
            }
        }
    }

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
