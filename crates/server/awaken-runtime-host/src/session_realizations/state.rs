// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! The realization layer of one installed Session: its Environment owner,
//! the Control-issued realization lease, and what is staged for it.

use awaken_session_configuration_contract::resource as session_resource;

/// One process-local projection of aggregate authority, not another lease.
/// Closing retains the exact lease for cleanup CAS, but never for admission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum SessionRealizationAuthority {
    #[default]
    Uninstalled,
    Live(awaken_session_realization_contract::ObservedSessionRealizationLease),
    Closed(Option<session_resource::SessionRealizationLease>),
}

impl SessionRealizationAuthority {
    pub(crate) fn proof(
        &self,
    ) -> Option<&awaken_session_realization_contract::ObservedSessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(proof),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub(crate) fn remaining(&self) -> Option<std::time::Duration> {
        self.proof()?
            .remaining_at(awaken_authority_clock::authority_proof_started().ok()?)
    }

    /// One local admission rule: exact aggregate generation and a still-live
    /// request-relative observation. Callers hold the existing slot mutex;
    /// neither wall time nor a retained Closed lease grants authority.
    pub(crate) fn remaining_for(
        &self,
        asserted: &session_resource::SessionRealizationLease,
    ) -> Option<std::time::Duration> {
        awaken_session_realization_contract::realization_lease_generation_authorizes(
            self.lease()?,
            asserted,
        )
        .then(|| self.remaining())
        .flatten()
    }

    pub(crate) fn lease(&self) -> Option<&session_resource::SessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(&proof.lease),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }

    /// Called under the Session slot mutex used for renewal application. The
    /// pending provider GO callback must hold this same mutex through its grant.
    /// A retry may select the retained closed identity; a stale revoker cannot.
    pub(crate) fn close(
        &mut self,
        expected: Option<&session_resource::SessionRealizationLease>,
    ) -> bool {
        let retained = match self {
            Self::Live(proof) => Some(&mut proof.lease),
            Self::Closed(lease) => lease.as_mut(),
            Self::Uninstalled => None,
        };
        if expected.is_some_and(|expected| retained.as_deref() != Some(expected)) {
            return false;
        }
        *self = Self::Closed(retained.cloned());
        true
    }
}

/// The environment-realization layer: who owns the Session's Environment,
/// under which realization lease, and which Resources, memory, Skills and MCP
/// generations are staged for it. Built while realizing; revocation keeps
/// only what a retry of a retained Environment owner still needs.
#[derive(Default)]
pub(crate) struct SessionRealization {
    pub(crate) environment: crate::environment_holding::EnvironmentHoldingState,
    pub(crate) staging: crate::resource_staging::ResourceStagingState,
    pub(crate) cleanup: crate::terminal_cleanup::TerminalCleanupState,
    pub(crate) execution: crate::session_realizations::ExecutionAuthorityState,
}

impl SessionRealization {
    pub(crate) fn validate_preparation_publication(
        &self,
        asserted: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), awaken_session_types::RunError> {
        if self.staging.preparation.is_some()
            && !asserted.is_some_and(|lease| self.resource_preparation_authorized(lease))
        {
            return Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_authority_lost",
                "Resource preparation cannot publish after realization authority ended",
            ));
        }
        Ok(())
    }

    /// Resources the Environment held before an in-flight resource transition.
    pub(crate) fn previous_resources(&self) -> Option<session_resource::ResolvedSessionResources> {
        let transition = self.staging.transition.as_ref()?;
        Some(transition.previous().resources.clone())
    }

    /// What revocation keeps while an Environment owner is still retained:
    /// the slot's lock identities, the closed authority, the owner itself and
    /// the content it was staged with. Everything else is rebuilt.
    pub(crate) fn take_retained_on_revocation(&mut self) -> Self {
        Self {
            execution: crate::session_realizations::ExecutionAuthorityState {
                phase: self.execution.phase.clone(),
                changed: self.execution.changed.clone(),
                authority: self.execution.authority.clone(),
                ..Default::default()
            },
            environment: crate::environment_holding::EnvironmentHoldingState {
                lifecycle: self.environment.lifecycle.clone(),
                owner: std::mem::take(&mut self.environment.owner),
                ..Default::default()
            },
            staging: crate::resource_staging::ResourceStagingState {
                projection: self.staging.projection.clone(),
                content_delivery: self.staging.content_delivery,
                resources: std::mem::take(&mut self.staging.resources),
                // Only the canonical durable projection may reopen MCP effects.
                mcp: self.staging.mcp.take_retained_on_revocation(),
                ..Default::default()
            },
            cleanup: Default::default(),
        }
    }

    pub(crate) fn close_authority(
        &mut self,
        expected: Option<&session_resource::SessionRealizationLease>,
    ) -> bool {
        if !self.execution.authority.close(expected) {
            return false;
        }
        if let Some(preparation) = &self.staging.preparation {
            preparation.cancel();
        }
        self.execution.changed.notify_waiters();
        true
    }

    pub(crate) fn install_lease(
        &mut self,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        let Ok(now) = awaken_authority_clock::authority_proof_started() else {
            return false;
        };
        if proof.remaining_at(now).is_none() {
            return false;
        }
        let lease = &proof.lease;
        if let SessionRealizationAuthority::Closed(Some(closed)) = &self.execution.authority
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                closed, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, closed,
            ))
        {
            // An assertion or delayed renewal cannot reopen a closed generation.
            return false;
        }
        if let Some(current) = self.execution.authority.lease()
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            ))
            && self.execution.authority.remaining().is_none()
        {
            return false;
        }
        if self.execution.authority.lease().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            )
        }) {
            // A slow preparation must not overwrite an intervening renewal.
            return true;
        }
        if self.execution.authority.lease().is_some_and(|current| {
            !awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            )
        }) && let Some(preparation) = &self.staging.preparation
        {
            preparation.cancel();
        }
        self.execution.authority = SessionRealizationAuthority::Live(proof);
        true
    }

    /// The only renewal response application edge. Callers with an I/O
    /// snapshot must supply it unchanged; a fresh same-lock caller may use
    /// the currently visible lease. Neither can recreate absent/closed state.
    pub(crate) fn renew_lease(
        &mut self,
        asserted: &session_resource::SessionRealizationLease,
        renewed: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        if self.execution.authority.lease() != Some(asserted)
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &renewed.lease,
                asserted,
            )
        {
            return false;
        }
        let Some(now) = awaken_authority_clock::authority_proof_started().ok() else {
            return false;
        };
        if self.execution.authority.remaining().is_none() || renewed.remaining_at(now).is_none() {
            return false;
        }
        self.execution.authority = SessionRealizationAuthority::Live(renewed);
        self.execution.changed.notify_waiters();
        true
    }

    pub(crate) fn resource_preparation_authorized(
        &self,
        asserted: &session_resource::SessionRealizationLease,
    ) -> bool {
        !self.execution.authority.is_closed()
            && self.staging.preparation.as_ref().is_none_or(|preparation| {
                !preparation.is_cancelled()
                    && self.execution.authority.remaining_for(asserted).is_some()
            })
    }
}
