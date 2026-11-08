// @formal surface requirement=model.routing.exact_binding
// @formal reason: The one selection of a Session's Agent publication and exact model candidate from its
// @formal reason: frozen coordinates; Coordinator admission and Worker realization share it. Pure over the
// @formal reason: publication source it is given; no catalog head, state or I/O of its own.
//! Canonical Session publication selection.
//!
//! A complete Session model override wins even when no Agent publication
//! exists, the exact Agent publication is next, and only a genuinely unpinned
//! legacy Session has no candidate. The Coordinator lowers a frozen projection
//! through this selection before dispatch; a Worker's realization selects the
//! same publication before it executes.

use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    ExecutableAgentSnapshot, PublishedAgentSnapshotSource, resolved::ResolvedModelCandidate,
};
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;

use super::{CanonicalSessionProjection, FrozenAgentPublicationDecision};
use crate::RunError;

/// Why a Session's frozen coordinates select no executable publication.
#[derive(Debug, thiserror::Error)]
pub enum CanonicalPublicationError {
    /// The Session's model override carries a publication foreign to its Workspace.
    #[error("frozen Session model override publication is invalid: {0}")]
    ModelOverridePublication(awaken_session_configuration_contract::SessionModelResolutionError),
    /// The frozen Agent publication the coordinates require is definitively
    /// absent: its store answered and holds no such publication.
    #[error("frozen Session Agent publication is unavailable")]
    MissingPublication,
    /// The publication store could not answer, so the publication may exist.
    #[error("frozen Session Agent publication could not be fetched: {0}")]
    PublicationSourceUnavailable(awaken_runtime_contract::AgentSnapshotError),
    /// A publication exists but does not match the frozen coordinates.
    #[error(
        "Session Agent/model publication does not match its frozen coordinates \
         (expected agent={expected_agent}, revision={expected_revision:?}; actual={actual})"
    )]
    PublicationMismatch {
        expected_agent: String,
        expected_revision: Option<u64>,
        actual: String,
    },
    /// Projecting the Session's prompt, Skills or model onto the publication failed.
    #[error(transparent)]
    Projection(RunError),
}

/// The neutral Run failure of a publication selection, with the codes the
/// realization reports for the same causes.
impl From<CanonicalPublicationError> for RunError {
    fn from(error: CanonicalPublicationError) -> Self {
        match error {
            CanonicalPublicationError::ModelOverridePublication(_) => {
                RunError::classified("session_realization_evidence_invalid", error.to_string())
            }
            CanonicalPublicationError::MissingPublication
            | CanonicalPublicationError::PublicationMismatch { .. } => {
                RunError::classified("session_realization_invariant_violated", error.to_string())
            }
            CanonicalPublicationError::PublicationSourceUnavailable(_) => {
                RunError::unavailable_classified(
                    SESSION_PUBLICATION_SOURCE_UNAVAILABLE_CODE,
                    error.to_string(),
                )
            }
            CanonicalPublicationError::Projection(error) => error,
        }
    }
}

/// Stable `RunError` code of a publication lookup whose store could not
/// answer. It is retryable: the publication may exist, unlike
/// `session_realization_invariant_violated` for a definitively absent one.
pub const SESSION_PUBLICATION_SOURCE_UNAVAILABLE_CODE: &str =
    "session_publication_source_unavailable";

/// The effective publication and exact model candidate of one Session.
pub type CanonicalSessionPublication = (
    Option<ExecutableAgentSnapshot>,
    Option<ResolvedModelCandidate>,
);

/// Select one Session publication and its exact model candidate from explicit
/// frozen coordinates: `delivered` when the caller already holds the exact
/// publication, else `publications` at the frozen revision (or its current
/// one for an unpinned coordinate).
///
/// Decision table (causes → effect):
/// | Rule | model override publication | publication decision | Effect |
/// |---|---|---|---|
/// | S1 | foreign to the Workspace | - | `ModelOverridePublication` |
/// | S0 | absent or valid | the store could not answer the lookup | `PublicationSourceUnavailable` (retryable) |
/// | S2 | absent or valid | `MissingRequired` | `MissingPublication` |
/// | S3 | absent or valid | `Mismatch` | `PublicationMismatch` naming both sides |
/// | S4 | absent or valid | `Unpinned`, `OptionalMissing`, `Exact` | the projected publication; the candidate is the override's primary, else the publication's binding |
pub fn resolve_canonical_session_projection(
    publications: Option<&dyn PublishedAgentSnapshotSource>,
    workspace: &str,
    coordinates: CanonicalSessionProjection<'_>,
    delivered: Option<ExecutableAgentSnapshot>,
    skill_pins: Option<&[ResolvedSkillBinding]>,
) -> Result<CanonicalSessionPublication, CanonicalPublicationError> {
    // S1 is a local invariant, independent of a publication store outage.
    let model_override = coordinates.model_override();
    if let Some(publication) = model_override.and_then(|override_| override_.publication.as_ref()) {
        publication
            .validate_for_workspace(workspace)
            .map_err(CanonicalPublicationError::ModelOverridePublication)?;
    }
    let agent = coordinates.agent();
    let frozen_revision = coordinates.agent_revision();
    let installed = match (delivered, publications) {
        (Some(delivered), _) => Some(delivered),
        (None, None) => None,
        (None, Some(source)) => {
            let agent_id = AgentId(agent.to_string());
            match frozen_revision {
                Some(revision) => source.at_revision(workspace, &agent_id, revision),
                None => source.current(workspace, &agent_id),
            }
            .map_err(CanonicalPublicationError::PublicationSourceUnavailable)?
        }
    };
    match coordinates.publication_decision(installed.as_ref()) {
        FrozenAgentPublicationDecision::Unpinned
        | FrozenAgentPublicationDecision::OptionalMissing
        | FrozenAgentPublicationDecision::Exact => {}
        FrozenAgentPublicationDecision::MissingRequired => {
            return Err(CanonicalPublicationError::MissingPublication);
        }
        FrozenAgentPublicationDecision::Mismatch => {
            let actual = installed.as_ref().map(|publication| {
                format!(
                    "agent={}, revision={}, backend={}",
                    publication.root_agent_id.0,
                    publication.metadata.source.revision,
                    publication.resolved_spec.model_binding.identity().2,
                )
            });
            return Err(CanonicalPublicationError::PublicationMismatch {
                expected_agent: coordinates.agent().to_owned(),
                expected_revision: coordinates.agent_revision(),
                actual: actual.unwrap_or_else(|| "missing".to_owned()),
            });
        }
    }
    let installed = installed
        .map(|snapshot| {
            let (prompt, skill_pins) = match &coordinates {
                CanonicalSessionProjection::Baseline(baseline) => {
                    (baseline.system_prompt.as_ref(), skill_pins)
                }
                CanonicalSessionProjection::Layout(_) => (
                    &awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
                    None,
                ),
                CanonicalSessionProjection::LegacyAgent(_) => return Ok(snapshot),
            };
            super::project_effective_agent_publication(
                model_override,
                prompt,
                skill_pins,
                workspace,
                snapshot,
            )
            .map_err(CanonicalPublicationError::Projection)
        })
        .transpose()?;
    let candidate = model_override
        .and_then(|override_| override_.publication.as_ref())
        .map(|publication| publication.primary.clone())
        .or_else(|| {
            installed
                .as_ref()
                .map(|snapshot| snapshot.resolved_spec.model_binding.clone())
        });
    Ok((installed, candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionBaseline, SessionBaselineInputs, SessionRuntimePlacement};
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;
    use awaken_runtime_contract::resolved::ModelBinding;

    /// A publication store that cannot answer any lookup.
    struct UnreachablePublications;

    impl awaken_runtime_contract::PublishedAgentSnapshotSource for UnreachablePublications {
        fn exact(
            &self,
            _: &str,
            _: &awaken_runtime_contract::CatalogFingerprint,
        ) -> Result<Option<ExecutableAgentSnapshot>, awaken_runtime_contract::AgentSnapshotError>
        {
            Err(
                awaken_runtime_contract::AgentSnapshotError::SourceUnavailable(
                    "store offline".into(),
                ),
            )
        }

        fn current(
            &self,
            _: &str,
            _: &AgentId,
        ) -> Result<Option<ExecutableAgentSnapshot>, awaken_runtime_contract::AgentSnapshotError>
        {
            Err(
                awaken_runtime_contract::AgentSnapshotError::SourceUnavailable(
                    "store offline".into(),
                ),
            )
        }

        fn at_revision(
            &self,
            _: &str,
            _: &AgentId,
            _: u64,
        ) -> Result<Option<ExecutableAgentSnapshot>, awaken_runtime_contract::AgentSnapshotError>
        {
            Err(
                awaken_runtime_contract::AgentSnapshotError::SourceUnavailable(
                    "store offline".into(),
                ),
            )
        }
    }

    fn snapshot(agent: &str) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(agent)
            .model(ModelBinding::parse("provider", "model", "native").expect("valid model"))
            .build()
    }

    fn baseline(placement: SessionRuntimePlacement) -> SessionBaseline {
        SessionBaseline::compile(SessionBaselineInputs {
            environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "publication-environment".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "publication-environment-v1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization:
                    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            },
            runtime_placement: placement,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: Some(2),
            model: "provider/model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        })
    }

    #[test]
    fn canonical_publication_selection_follows_its_decision_table() {
        // Rows S1-S4 of `resolve_canonical_session_projection`, plus the Run
        // code each refusal reports (the realization reports the same codes).
        // S4 legacy: no source => unpinned (no publication, no candidate); a
        // current publication => that publication and its binding.
        let none = resolve_canonical_session_projection(
            None,
            "workspace",
            CanonicalSessionProjection::LegacyAgent("agent"),
            None,
            None,
        )
        .expect("S4 unpinned legacy");
        assert!(none.0.is_none() && none.1.is_none(), "S4 unpinned");
        let published = snapshot("agent");
        let source =
            StaticPublishedAgentSnapshots::try_new([published.clone()]).expect("valid catalog");
        let (selected, candidate) = resolve_canonical_session_projection(
            Some(&source),
            "workspace",
            CanonicalSessionProjection::LegacyAgent("agent"),
            None,
            None,
        )
        .expect("S4 legacy current publication");
        assert_eq!(selected.as_ref(), Some(&published), "S4 publication");
        assert_eq!(
            candidate,
            Some(published.resolved_spec.model_binding.clone()),
            "S4 candidate is the publication's binding"
        );

        // S2: a Worker-placed Session pinned to revision 2 with no publication.
        let worker = baseline(SessionRuntimePlacement::Worker);
        let missing = resolve_canonical_session_projection(
            None,
            "workspace",
            CanonicalSessionProjection::Baseline(&worker),
            None,
            None,
        )
        .expect_err("S2 missing publication");
        assert!(
            matches!(missing, CanonicalPublicationError::MissingPublication),
            "S2"
        );
        assert_eq!(
            RunError::from(missing).code,
            "session_realization_invariant_violated",
            "S2 code"
        );

        // S2 vs S0: the same pinned lookup through a source that answers
        // "absent" is the permanent refusal; through a source whose store
        // could not answer it is the retryable one, never MissingPublication.
        let empty = StaticPublishedAgentSnapshots::try_new([]).expect("empty catalog");
        let absent = resolve_canonical_session_projection(
            Some(&empty),
            "workspace",
            CanonicalSessionProjection::Baseline(&worker),
            None,
            None,
        )
        .expect_err("S2 a store that holds no such revision");
        assert!(
            matches!(absent, CanonicalPublicationError::MissingPublication),
            "S2 definitive absence"
        );
        let unreachable = resolve_canonical_session_projection(
            Some(&UnreachablePublications),
            "workspace",
            CanonicalSessionProjection::Baseline(&worker),
            None,
            None,
        )
        .expect_err("S0 a store that could not answer");
        assert!(
            matches!(
                &unreachable,
                CanonicalPublicationError::PublicationSourceUnavailable(_)
            ),
            "S0 {unreachable}"
        );
        let unreachable = RunError::from(unreachable);
        assert_eq!(
            unreachable.code, SESSION_PUBLICATION_SOURCE_UNAVAILABLE_CODE,
            "S0 code"
        );
        assert_eq!(
            crate::effect_failure_disposition(&unreachable),
            crate::SessionRealizationControlDisposition::Retryable,
            "S0 is retryable"
        );
        // A delivered publication needs no lookup: an unreachable store
        // cannot refuse the exact publication the caller already holds.
        let mut pinned = snapshot("agent");
        pinned.metadata.source.agent_id = AgentId("agent".into());
        pinned.metadata.source.revision = 2;
        assert!(
            resolve_canonical_session_projection(
                Some(&UnreachablePublications),
                "workspace",
                CanonicalSessionProjection::Baseline(&worker),
                Some(pinned),
                None,
            )
            .is_ok(),
            "S0 never applies to a delivered publication"
        );

        // S3: a delivered publication of another Agent never stands in.
        let mismatch = resolve_canonical_session_projection(
            None,
            "workspace",
            CanonicalSessionProjection::Baseline(&worker),
            Some(snapshot("other")),
            None,
        )
        .expect_err("S3 mismatched publication");
        assert!(
            matches!(
                &mismatch,
                CanonicalPublicationError::PublicationMismatch { expected_agent, actual, .. }
                    if expected_agent == "agent" && actual.contains("agent=other")
            ),
            "S3 names both sides: {mismatch}"
        );
        assert_eq!(
            RunError::from(mismatch).code,
            "session_realization_invariant_violated",
            "S3 code"
        );

        // S1: an override whose publication repeats a candidate is refused
        // before any publication is selected.
        let mut overridden = baseline(SessionRuntimePlacement::Local);
        let duplicate = ResolvedModelCandidate::host(
            ModelBinding::parse("provider", "model", "native").expect("valid model"),
        );
        overridden.model_override = Some(crate::SessionModelOverride {
            publication: Some(Box::new(
                awaken_session_configuration_contract::SessionModelPublication {
                    primary: duplicate.clone(),
                    candidates: vec![duplicate],
                },
            )),
            inference: Default::default(),
        });
        let invalid = resolve_canonical_session_projection(
            Some(&source),
            "workspace",
            CanonicalSessionProjection::Baseline(&overridden),
            None,
            None,
        )
        .expect_err("S1 invalid override publication");
        assert!(
            matches!(
                invalid,
                CanonicalPublicationError::ModelOverridePublication(_)
            ),
            "S1"
        );
        assert_eq!(
            RunError::from(invalid).code,
            "session_realization_evidence_invalid",
            "S1 code"
        );
        // S1 + S0: an invalid override is refused before any unavailable
        // publication source can misclassify it as a retryable dependency.
        let invalid_during_outage = resolve_canonical_session_projection(
            Some(&UnreachablePublications),
            "workspace",
            CanonicalSessionProjection::Baseline(&overridden),
            None,
            None,
        )
        .expect_err("S1 wins independently of source availability");
        assert!(
            matches!(
                invalid_during_outage,
                CanonicalPublicationError::ModelOverridePublication(_)
            ),
            "S1 + S0 keeps the invariant refusal"
        );
    }
}
