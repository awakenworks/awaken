// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: immutable materialization facts in the existing Session root.
// @formal reason: Dynamic path: the controlled Environment command admits the first Create before I/O,
// @formal reason: and receipt/replacement validation preserves its exact original value. Provider request
// @formal reason: closure remains provider-owned; this adds no ledger, service or physical takeover grant.
//! First materialization provenance owned by the existing Session aggregate.

use super::{
    SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind, SessionEnvironmentReceiptError,
};
use crate::{PersistedSession, SessionMutationValidationError};

/// Present-empty is persisted only by fresh creation; an absent outer value is
/// historical unknown. Source and pending target are distinct physical facts,
/// not copies of a mutable "last effect".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEnvironmentMaterialization {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<Box<SessionEnvironmentEffectIntent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Box<SessionEnvironmentEffectIntent>>,
}

impl SessionEnvironmentMaterialization {
    /// Project the original Create identity for cold terminal observation.
    /// Expiry is historical; current cleanup authorization is supplied separately.
    pub fn original_create_fence(
        materialization: Option<&Self>,
        session_id: &str,
    ) -> Result<awaken_provisioning_contract::SandboxEffectFence, SessionEnvironmentReceiptError>
    {
        let intent = materialization
            .and_then(|facts| facts.pending().or_else(|| facts.origin()))
            .ok_or(SessionEnvironmentReceiptError::MaterializationUnknown)?;
        intent.verify()?;
        if intent.session_id().as_str() != session_id
            || !matches!(intent.kind(), SessionEnvironmentEffectKind::Create)
        {
            return Err(SessionEnvironmentReceiptError::Mismatch);
        }
        intent
            .realization()
            .ok_or(SessionEnvironmentReceiptError::RealizationStale)?
            .sandbox_effect_fence(intent.effect_id())
            .map_err(|_| SessionEnvironmentReceiptError::Mismatch)
    }

    pub(crate) const fn fresh() -> Self {
        Self {
            origin: None,
            pending: None,
        }
    }

    #[must_use]
    pub fn origin(&self) -> Option<&SessionEnvironmentEffectIntent> {
        self.origin.as_deref()
    }

    #[must_use]
    pub fn pending(&self) -> Option<&SessionEnvironmentEffectIntent> {
        self.pending.as_deref()
    }

    #[must_use]
    pub const fn has_admitted(&self) -> bool {
        self.origin.is_some() || self.pending.is_some()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.has_admitted()
    }

    /// Shared intrinsic law for aggregate import/mutation and the one Store
    /// codec. Original leases are historical facts, never current grants.
    pub fn validate_for(session: &PersistedSession) -> Result<(), SessionMutationValidationError> {
        let Some(facts) = session.environment_materialization.as_deref() else {
            return Ok(());
        };
        for intent in [facts.origin(), facts.pending()].into_iter().flatten() {
            let valid_source = match intent.kind() {
                SessionEnvironmentEffectKind::Create => intent.source_binding().is_none(),
                SessionEnvironmentEffectKind::Rebuild {
                    source_generation_id,
                } => {
                    intent
                        .source_binding()
                        .is_some_and(|binding| !binding.trim().is_empty())
                        && source_generation_id
                            .as_ref()
                            .is_none_or(|id| !id.trim().is_empty())
                }
                SessionEnvironmentEffectKind::Adopt
                | SessionEnvironmentEffectKind::ResourceProjectionReservation { .. } => false,
            };
            if intent.verify().is_err()
                || intent.session_id() != session.session_id
                || !valid_source
                || !intent.realization().is_some_and(|lease| {
                    // Structural fence validation only: old expiry and a
                    // different aggregate-current owner remain readable.
                    lease.sandbox_effect_fence(intent.effect_id()).is_ok()
                })
                || session.frozen_baseline().is_none_or(|baseline| {
                    intent.environment_fingerprint()
                        != Some(baseline.environment.config_fingerprint.0.as_str())
                })
            {
                return Err(SessionMutationValidationError::EnvironmentMaterializationMismatch);
            }
        }
        if let (Some(origin), Some(pending)) = (facts.origin(), facts.pending())
            && (origin.effect_id() == pending.effect_id()
                || !matches!(pending.kind(), SessionEnvironmentEffectKind::Rebuild { .. }))
        {
            return Err(SessionMutationValidationError::EnvironmentMaterializationMismatch);
        }
        Ok(())
    }

    /// Adopt/reservation may use only a completed known origin. An explicit
    /// fresh-empty fact or unfinished Create/Rebuild cannot be replaced by a
    /// late ordinary receipt. Legacy outer None keeps its original pure law.
    pub(crate) fn validate_existing_origin_effect(
        &self,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<(), SessionEnvironmentReceiptError> {
        if matches!(
            intent.kind(),
            SessionEnvironmentEffectKind::Adopt
                | SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
        ) && (self.origin.is_none() || self.pending.is_some())
        {
            return Err(SessionEnvironmentReceiptError::MaterializationPending);
        }
        Ok(())
    }

    pub(crate) fn validate_receipt(
        &self,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<(), SessionEnvironmentReceiptError> {
        match intent.kind() {
            SessionEnvironmentEffectKind::Create => {
                if self.pending().or_else(|| self.origin()) != Some(intent) {
                    return Err(SessionEnvironmentReceiptError::InvalidTransition);
                }
            }
            SessionEnvironmentEffectKind::Rebuild { .. } => {
                return Err(SessionEnvironmentReceiptError::MaterializationPending);
            }
            SessionEnvironmentEffectKind::Adopt
            | SessionEnvironmentEffectKind::ResourceProjectionReservation { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn record_receipt(&mut self, intent: &SessionEnvironmentEffectIntent) {
        if matches!(intent.kind(), SessionEnvironmentEffectKind::Create)
            && self.pending() == Some(intent)
        {
            self.origin = self.pending.take();
        }
    }

    pub(crate) fn validate_replacement(
        session: &PersistedSession,
        replacement: &PersistedSession,
    ) -> Result<(), SessionMutationValidationError> {
        let invalid = || SessionMutationValidationError::EnvironmentMaterializationChanged;
        match (
            session.environment_materialization.as_deref(),
            replacement.environment_materialization.as_deref(),
        ) {
            (None, None) => Ok(()),
            (None, Some(_)) | (Some(_), None) => Err(invalid()),
            (Some(before), Some(after)) if before == after => Ok(()),
            (Some(before), Some(after)) if before.is_empty() => {
                if after.origin().is_none()
                    && after.pending().is_some_and(|intent| {
                        matches!(intent.kind(), SessionEnvironmentEffectKind::Create)
                            && session
                                .realization
                                .as_ref()
                                .zip(intent.realization())
                                .is_some_and(|(current, first)| {
                                    crate::realization_lease_generation_authorizes(current, first)
                                })
                    })
                    && !session.is_terminal()
                    && matches!(
                        session.environment,
                        super::SessionEnvironmentState::Unmaterialized
                    )
                {
                    Ok(())
                } else {
                    Err(invalid())
                }
            }
            (Some(before), Some(after)) => {
                let move_first = before.origin().is_none()
                    && before.pending().is_some_and(|intent| {
                        matches!(intent.kind(), SessionEnvironmentEffectKind::Create)
                    })
                    && after.pending().is_none()
                    && after.origin() == before.pending()
                    && matches!(&replacement.environment, super::SessionEnvironmentState::Resident { effect_id: Some(id), .. }
                        if Some(id.as_str()) == before.pending().map(SessionEnvironmentEffectIntent::effect_id));
                if move_first { Ok(()) } else { Err(invalid()) }
            }
        }
    }

    pub(crate) fn admit_first(&mut self, intent: &SessionEnvironmentEffectIntent) {
        self.pending = Some(Box::new(intent.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_create_query_preserves_history_and_refuses_unknown_or_drift() {
        // C1 origin/pending holds original Create; C2 original lease present;
        // C3 exact Session; C4 kind Create; C5 outer missing/present-empty.
        // Q1=C1+C2+C3+C4 -> original fence including expired historical expiry;
        // Q2=!C2 or !C3 or !C4 -> reject/pending without a guessed lease;
        // Q3=C5 -> pending. Querying history does not grant live execution.
        let lease = crate::SessionRealizationLease {
            owner: "old".into(),
            runtime_incarnation: "old-boot".into(),
            epoch: 3,
            expires_at_unix_ms: 1,
        };
        for pending in [false, true] {
            for has_lease in [false, true] {
                for same_session in [false, true] {
                    for create in [false, true] {
                        let intent = SessionEnvironmentEffectIntent::new(
                            if same_session { "s1" } else { "other" },
                            if create {
                                SessionEnvironmentEffectKind::Create
                            } else {
                                SessionEnvironmentEffectKind::Adopt
                            },
                            has_lease.then(|| lease.clone()),
                        );
                        let mut fact = SessionEnvironmentMaterialization::fresh();
                        if pending {
                            fact.pending = Some(Box::new(intent.clone()));
                        } else {
                            fact.origin = Some(Box::new(intent.clone()));
                        }
                        let result = SessionEnvironmentMaterialization::original_create_fence(
                            Some(&fact),
                            "s1",
                        );
                        assert_eq!(result.is_ok(), has_lease && same_session && create, "Q1/Q2");
                        if has_lease && same_session && create {
                            assert_eq!(
                                result.unwrap(),
                                lease.sandbox_effect_fence(intent.effect_id()).unwrap(),
                                "Q1 exact history"
                            );
                        }
                    }
                }
            }
        }
        for fact in [None, Some(SessionEnvironmentMaterialization::fresh())] {
            assert_eq!(
                SessionEnvironmentMaterialization::original_create_fence(fact.as_ref(), "s1")
                    .unwrap_err(),
                SessionEnvironmentReceiptError::MaterializationUnknown,
                "Q3"
            );
        }
    }
}
