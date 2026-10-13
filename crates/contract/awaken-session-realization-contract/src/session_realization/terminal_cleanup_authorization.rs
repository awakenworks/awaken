// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the extracted closed terminal assignment, work, and
// @formal reason: preparation-authorization vocabulary re-exported by the existing Session realization
// @formal reason: contract. Dynamic path: Control projects the one Session-root cleanup operation and
// @formal reason: exact Workspace/effect authorization to Runtime without adding a queue, receipt store,
// @formal reason: lease authority, or lifecycle state machine; generic Session realization renewal remains
// @formal reason: in its existing owner. This is a product-requirement classification, not a direct
// @formal reason: formal-proof claim.
//! Closed terminal assignment, work, and preparation authorization.
//!
//! This private module is a mechanical responsibility boundary. The Session
//! root remains the sole lifecycle authority, and the parent module re-exports
//! the unchanged public contract consumed by Control and Runtime adapters.

use super::{FrozenSessionProjection, SessionRealizationControlFailure};
use awaken_session_state_contract::PersistedSession;

/// One cold Worker assignment for an already-fenced terminal Session.
///
/// Cleanup commands deliberately do not travel in this assignment. The Worker
/// installs the frozen projection and then polls the parent Control's terminal
/// work and root Repository-publication projections, keeping
/// [`crate::SessionCleanupOperation`] as the only durable work queue and
/// completion registry.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTerminalCleanupAssignment {
    pub session_id: crate::SessionId,
    pub projection: FrozenSessionProjection,
    /// Derived only from the exact committed unattempted-profiled archive
    /// receipt. Historical assignments remain Unknown; empty handles are not
    /// evidence that no Create was admitted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub environment_creation_not_attempted: bool,
    #[serde(flatten)]
    pub authority: super::SessionRealizationLeaseReceipt,
}

/// Verify consistency of Control's no-Create witness with the frozen state
/// and Runtime's local owner observation. Absence of a witness remains Unknown.
pub fn validate_terminal_no_create_witness(
    creation_not_attempted: bool,
    environment: &awaken_session_state_contract::SessionEnvironmentState,
    owner_vacant: bool,
    materialization_admitted: bool,
) -> Result<(), crate::RunError> {
    if creation_not_attempted
        && (!matches!(
            environment,
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
        ) || !owner_vacant
            || materialization_admitted)
    {
        return Err(crate::RunError::classified(
            "session_terminal_unattempted_environment_conflict",
            "archive no-Create witness conflicts with the frozen state, retained owner or admitted materialization",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod no_create_witness_tests {
    #[test]
    fn terminal_no_create_consistency_retains_unknown_and_rejects_contradictions() {
        // Causes C1 witness, C2 Unmaterialized frozen state, C3 Vacant local
        // owner, C4 canonical materialization.has_admitted from frozen Root.
        // W1 absent witness -> retain ordinary provider requirements;
        // W2 witness + C2/C3 -> allow the known-absence branch; W3 witness with
        // any contradiction -> classified rejection. Cross the eight original
        // rows with both C4 values: present-empty supplies no witness; admitted
        // pending/origin contradicts a true witness even without binding.
        use awaken_session_state_contract::SessionEnvironmentState;
        for (witness, unmaterialized, vacant, allowed) in [
            (false, false, false, true),
            (false, false, true, true),
            (false, true, false, true),
            (false, true, true, true),
            (true, false, false, false),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, true),
        ] {
            let state = if unmaterialized {
                SessionEnvironmentState::Unmaterialized
            } else {
                SessionEnvironmentState::Resident {
                    binding: "binding".into(),
                    effect_id: None,
                    generation: None,
                    idle_since_unix_ms: None,
                }
            };
            for admitted in [false, true] {
                let result =
                    super::validate_terminal_no_create_witness(witness, &state, vacant, admitted);
                assert_eq!(
                    result.is_ok(),
                    allowed && (!witness || !admitted),
                    "witness={witness}, unmaterialized={unmaterialized}, vacant={vacant}, admitted={admitted}"
                );
            }
        }
    }
}

/// One read-only projection of terminal work from a single Session-root
/// snapshot. `assignment` carries that snapshot's current realization lease and
/// frozen Environment/Resource facts; `action` is the mutually-exclusive next
/// step derived by [`crate::PersistedSession`]. The cleanup operation owns
/// progress, while that same aggregate joins an existing continuation disposal
/// predecessor from its Environment when required. This value owns no queue,
/// phase, receipt, or retry state beside that root.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTerminalCleanupWork {
    pub assignment: SessionTerminalCleanupAssignment,
    pub action: crate::SessionTerminalCleanupAction,
}

/// Aggregate-derived authorization for one source-dependent terminal
/// preparation effect.
///
/// The exact effect is echoed privately so a Workspace-scoped authorization or
/// an inherited continuation predecessor cannot be replayed for another child,
/// root, or realization generation. `inherited_provider_disposal` is present
/// only when the root Environment has already durably crossed a continuation
/// preparation gate; ordinary terminal roots and every child receive `None`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTerminalCleanupPreparationAuthorization {
    effect: crate::SessionTerminalCleanupEffect,
    workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inherited_provider_disposal: Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionTerminalCleanupPreparationAuthorizationWire {
    effect: crate::SessionTerminalCleanupEffect,
    workspace_id: String,
    #[serde(default)]
    inherited_provider_disposal: Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
}

impl<'de> serde::Deserialize<'de> for SessionTerminalCleanupPreparationAuthorization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionTerminalCleanupPreparationAuthorizationWire::deserialize(deserializer)?;
        Self::try_new(
            wire.effect,
            wire.workspace_id,
            wire.inherited_provider_disposal,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SessionTerminalCleanupPreparationAuthorization {
    /// Close one root-authorized projection over its exact terminal effect.
    pub fn try_new(
        effect: crate::SessionTerminalCleanupEffect,
        workspace_id: String,
        inherited_provider_disposal: Option<
            awaken_provisioning_contract::SandboxDisposalPreparation,
        >,
    ) -> Result<Self, SessionRealizationControlFailure> {
        let authorization = Self {
            effect,
            workspace_id,
            inherited_provider_disposal,
        };
        authorization.validate()?;
        Ok(authorization)
    }

    fn validate(&self) -> Result<(), SessionRealizationControlFailure> {
        if self.workspace_id.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup preparation authorization has no Workspace".into(),
            ));
        }
        let current = self
            .effect
            .sandbox_effect_fence()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if let Some(inherited) = &self.inherited_provider_disposal {
            if self.effect.command.thread_id != self.effect.command.session_id {
                return Err(SessionRealizationControlFailure::Invalid(
                    "child terminal cleanup cannot inherit a root provider preparation".into(),
                ));
            }
            inherited
                .operation_id()
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if !inherited
                .prepared_effect_fence()
                .authorizes_successor(&current)
            {
                return Err(SessionRealizationControlFailure::Invalid(
                    "terminal cleanup preparation does not succeed its inherited provider fence"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Recheck that a transported authorization still names the exact effect
    /// being executed at the Runtime boundary.
    pub fn verify_for(
        &self,
        effect: &crate::SessionTerminalCleanupEffect,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.validate()?;
        if &self.effect != effect {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup preparation authorization names another effect".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    #[must_use]
    pub fn inherited_provider_disposal(
        &self,
    ) -> Option<&awaken_provisioning_contract::SandboxDisposalPreparation> {
        self.inherited_provider_disposal.as_ref()
    }
}

/// Lower one frozen terminal root into the static capabilities needed by the
/// current Worker for every remaining source-dependent phase. Model execution,
/// ACP, tool recovery, and inference credentials are deliberately absent: the
/// cleanup driver never starts a new Run.
pub fn terminal_cleanup_worker_requirements(
    owner_scope: &str,
    session: &PersistedSession,
    action: &awaken_session_state_contract::SessionTerminalCleanupAction,
) -> Result<awaken_worker_contract::PlacementRequirements, SessionRealizationControlFailure> {
    // Model credentials are irrelevant to cleanup; the physical process owner
    // is not. Preserve this requirement even after resource harvesting so the
    // final Dispose cannot be handed to an incapable replacement Worker.
    let physical_requirements =
        |mut requirements: awaken_worker_contract::PlacementRequirements| {
            if let Some(baseline) = session.frozen_baseline() {
                requirements.sandbox.process_tree_supervision =
                    baseline.environment.sandbox.process_tree_supervision;
                if baseline.environment.sandbox.host_access
                    == awaken_provisioning_contract::HostAccessPolicy::TrustedHost
                {
                    requirements.sandbox_backend =
                        Some(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND.to_owned());
                }
            }
            requirements
        };
    if matches!(
        action,
        awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { .. }
    ) {
        return Ok(physical_requirements(
            awaken_worker_contract::PlacementRequirements::terminal_cleanup(false, false, None),
        ));
    }

    let resource_facts = session
        .resources
        .active_generation()
        .1
        .compatibility_facts();
    let publication_outcome_is_durable = session
        .terminal_cleanup
        .repository_publication_receipt(&session.session_id)
        .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
        .is_some()
        || session
            .terminal_cleanup
            .repository_publication_rejection(&session.session_id)
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
            .is_some();
    let pending_publication = (!publication_outcome_is_durable)
        .then(|| session.terminal_cleanup.repository_publication_intent())
        .flatten();
    if pending_publication.is_some()
        && session
            .environment
            .terminal_repository_publication_binding()
            .is_none()
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "terminal Repository publication has no existing live Environment source".into(),
        ));
    }
    let publication_has_credential = pending_publication.is_some_and(|intent| {
        matches!(
            &intent.input.source,
            awaken_session_configuration_contract::resource::ResolvedInputSource::Repository { config, .. }
                if config.credential_binding.is_some()
        )
    });

    let baseline = session.frozen_baseline().ok_or_else(|| {
        SessionRealizationControlFailure::Invalid(
            "terminal cleanup requires one frozen Session baseline".into(),
        )
    })?;
    let checkpoint_format = if let Some(checkpoint) = session.environment.checkpoint() {
        Some(checkpoint.format.clone())
    } else {
        session
            .environment
            .checkpoint_request(
                owner_scope,
                &session.session_id,
                &baseline.environment.idle_retention,
            )
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
            .map(|request| request.format)
    };

    Ok(physical_requirements(
        awaken_worker_contract::PlacementRequirements::terminal_cleanup(
            resource_facts.has_resources() || pending_publication.is_some(),
            resource_facts.has_credentialed_repository() || publication_has_credential,
            checkpoint_format,
        ),
    ))
}
