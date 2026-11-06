// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
//! Recovery from closed provider evidence: which exact owner a closed
//! observation may consume. The provider validates the observation; this plan
//! classifies the installed owner against the owners the caller captured and
//! admits only a rebuild policy over an exact, unprotected owner. The sole
//! lifecycle writer then consumes the returned fence.

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, RetiringSessionEnvironment, SessionEnvironmentOwner,
    SessionEnvironmentPreparation, SessionEnvironmentRetirementCause,
};
use crate::realization_error::RealizationError;
use crate::session_environment::SessionEnvironmentUnavailablePolicy;
use std::sync::Arc;

/// Exact process-local owner selected after closed provider evidence has been
/// validated. The value itself is the fence consumed by the sole lifecycle
/// writer; no binding-only or cause-only proxy can authorize recovery.
pub(super) enum ClosedEnvironmentRecoveryOwner {
    Retiring(Box<RetiringSessionEnvironment>),
    Resident(Arc<crate::session_environment::SessionEnvironment>),
    Pending(Box<(BoundSessionEnvironmentIdentity, String)>),
    Vacant,
}

/// Capability minted only after the provider validates this exact closed
/// observation against the exact handle.
pub(super) struct ValidatedClosedEnvironmentObservation<'a>(
    pub(super) &'a awaken_provisioning_contract::SandboxObservation,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosedEnvironmentRecoveryOwnerClass {
    Revoked,
    RecoveryDiscard,
    Resident,
    Pending,
    Vacant,
    ProtectedOrStale,
}

/// The only closed-observation recovery plan. `Consume` retains the exact
/// owner fence through the subsequent installed Session mutation; `Retain` is fail-closed.
pub(super) enum ClosedEnvironmentRecoveryPlan<'a> {
    Consume {
        observation: ValidatedClosedEnvironmentObservation<'a>,
        owner: ClosedEnvironmentRecoveryOwner,
    },
    Retain,
}

const fn closed_environment_recovery_admission(
    policy: SessionEnvironmentUnavailablePolicy,
    owner: ClosedEnvironmentRecoveryOwnerClass,
) -> bool {
    matches!(policy, SessionEnvironmentUnavailablePolicy::Rebuild)
        && !matches!(owner, ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale)
}

pub(super) fn closed_environment_recovery_plan<'a>(
    sessions: &crate::session_realizations::SessionRealizations,
    thread: &str,
    policy: SessionEnvironmentUnavailablePolicy,
    observation: ValidatedClosedEnvironmentObservation<'a>,
    revoked: Option<&'a RetiringSessionEnvironment>,
    resident: Option<&'a Arc<crate::session_environment::SessionEnvironment>>,
    pending: Option<&'a (BoundSessionEnvironmentIdentity, String)>,
) -> Result<ClosedEnvironmentRecoveryPlan<'a>, RealizationError> {
    let (owner_class, exact_owner) = sessions
        .read_realization(thread, |r| {
            if let Some(expected) = revoked
                && expected.cause == SessionEnvironmentRetirementCause::RealizationRevocation
                && matches!(
                    &r.environment().owner,
                    SessionEnvironmentOwner::Retiring(current)
                        if current.exact_matches(expected)
                )
            {
                (
                    ClosedEnvironmentRecoveryOwnerClass::Revoked,
                    Some(ClosedEnvironmentRecoveryOwner::Retiring(Box::new(
                        expected.clone(),
                    ))),
                )
            } else if let Some(expected) = resident
                && let SessionEnvironmentOwner::Retiring(current) = &r.environment().owner
                && current.cause == SessionEnvironmentRetirementCause::RecoveryDiscard
                && Arc::ptr_eq(&current.owned.environment(), expected)
                && current.owned.environment().lifecycle().handle() == expected.lifecycle().handle()
            {
                (
                    ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
                    Some(ClosedEnvironmentRecoveryOwner::Retiring(Box::new(
                        current.clone(),
                    ))),
                )
            } else if let Some(expected) = resident
                && matches!(
                    &r.environment().owner,
                    SessionEnvironmentOwner::Resident(current)
                        if Arc::ptr_eq(&current.environment, expected)
                            && current.environment.lifecycle().handle() == expected.lifecycle().handle()
                )
            {
                (
                    ClosedEnvironmentRecoveryOwnerClass::Resident,
                    Some(ClosedEnvironmentRecoveryOwner::Resident(expected.clone())),
                )
            } else if let Some((identity, binding)) = pending
                && matches!(
                    &r.environment().owner,
                    SessionEnvironmentOwner::Preparing(
                        SessionEnvironmentPreparation::AwaitingAdoption {
                            identity: current_identity,
                            binding: current_binding,
                        }
                    ) if current_identity == identity && current_binding == binding
                )
            {
                (
                    ClosedEnvironmentRecoveryOwnerClass::Pending,
                    Some(ClosedEnvironmentRecoveryOwner::Pending(Box::new((
                        identity.clone(),
                        binding.clone(),
                    )))),
                )
            } else if revoked.is_none()
                && resident.is_none()
                && pending.is_none()
                && matches!(r.environment().owner, SessionEnvironmentOwner::Vacant)
            {
                (
                    ClosedEnvironmentRecoveryOwnerClass::Vacant,
                    Some(ClosedEnvironmentRecoveryOwner::Vacant),
                )
            } else {
                (ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale, None)
            }
        })?
        .unwrap_or((ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale, None));
    if !closed_environment_recovery_admission(policy, owner_class) {
        return Ok(ClosedEnvironmentRecoveryPlan::Retain);
    }
    Ok(match owner_class {
        ClosedEnvironmentRecoveryOwnerClass::Revoked
        | ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard
        | ClosedEnvironmentRecoveryOwnerClass::Resident
        | ClosedEnvironmentRecoveryOwnerClass::Pending
        | ClosedEnvironmentRecoveryOwnerClass::Vacant => ClosedEnvironmentRecoveryPlan::Consume {
            observation,
            owner: exact_owner.expect("admitted recovery class retains its exact owner"),
        },
        ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale => {
            ClosedEnvironmentRecoveryPlan::Retain
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_environment_recovery_admission_covers_every_owner_class() {
        // Causes: C1 Reject/Rebuild policy; C2 exact Revocation,
        // RecoveryDiscard, Resident, pending, Vacant, or protected/stale owner.
        // Effects: E1 only Rebuild plus an exact unprotected class consumes; E2
        // every Reject or protected/stale row retains. This is the complete 2x6
        // decision table used before the production plan carries the exact fence.
        // Layout rule: large retirement/pending payloads remain exact owned values
        // behind indirection; small variants and the fail-closed plan must not reserve
        // hundreds of inline bytes across async recovery. A relative pointer-width
        // budget covers the representation independently of target architecture.
        assert!(
            std::mem::size_of::<ClosedEnvironmentRecoveryOwner>()
                <= 4 * std::mem::size_of::<usize>()
        );
        assert!(
            std::mem::size_of::<ClosedEnvironmentRecoveryPlan<'_>>()
                <= 6 * std::mem::size_of::<usize>()
        );
        let owners = [
            ClosedEnvironmentRecoveryOwnerClass::Revoked,
            ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
            ClosedEnvironmentRecoveryOwnerClass::Resident,
            ClosedEnvironmentRecoveryOwnerClass::Pending,
            ClosedEnvironmentRecoveryOwnerClass::Vacant,
            ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale,
        ];
        for policy in [
            SessionEnvironmentUnavailablePolicy::Reject,
            SessionEnvironmentUnavailablePolicy::Rebuild,
        ] {
            for owner in owners {
                assert_eq!(
                    closed_environment_recovery_admission(policy, owner),
                    policy == SessionEnvironmentUnavailablePolicy::Rebuild
                        && owner != ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale
                );
            }
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_POLICY: [SessionEnvironmentUnavailablePolicy; 2] = [
        SessionEnvironmentUnavailablePolicy::Reject,
        SessionEnvironmentUnavailablePolicy::Rebuild,
    ];
    const EVERY_OWNER: [ClosedEnvironmentRecoveryOwnerClass; 6] = [
        ClosedEnvironmentRecoveryOwnerClass::Revoked,
        ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
        ClosedEnvironmentRecoveryOwnerClass::Resident,
        ClosedEnvironmentRecoveryOwnerClass::Pending,
        ClosedEnvironmentRecoveryOwnerClass::Vacant,
        ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale,
    ];

    const fn listed_policy(policy: SessionEnvironmentUnavailablePolicy) -> usize {
        match policy {
            SessionEnvironmentUnavailablePolicy::Reject => 0,
            SessionEnvironmentUnavailablePolicy::Rebuild => 1,
        }
    }

    const fn listed_owner(owner: ClosedEnvironmentRecoveryOwnerClass) -> usize {
        match owner {
            ClosedEnvironmentRecoveryOwnerClass::Revoked => 0,
            ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard => 1,
            ClosedEnvironmentRecoveryOwnerClass::Resident => 2,
            ClosedEnvironmentRecoveryOwnerClass::Pending => 3,
            ClosedEnvironmentRecoveryOwnerClass::Vacant => 4,
            ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale => 5,
        }
    }

    #[test]
    fn every_recovery_policy_and_owner_class_is_listed() {
        for (index, policy) in EVERY_POLICY.into_iter().enumerate() {
            assert_eq!(listed_policy(policy), index);
        }
        for (index, owner) in EVERY_OWNER.into_iter().enumerate() {
            assert_eq!(listed_owner(owner), index);
        }
    }

    /// @exhaustive SessionEnvironmentUnavailablePolicy(2) x ClosedEnvironmentRecoveryOwnerClass(6) = 12
    /// @obligation session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
    #[test]
    fn closed_environment_recovery_consumes_only_rebuildable_owner_classes() {
        // C1 Reject/Rebuild policy; C2 every owner class. E1 Consume iff
        // Rebuild and the class is exact and unprotected; E2 Reject, protected,
        // and stale inputs retain.
        let mut cases = 0;
        for policy in EVERY_POLICY {
            for owner in EVERY_OWNER {
                assert_eq!(
                    closed_environment_recovery_admission(policy, owner),
                    policy == SessionEnvironmentUnavailablePolicy::Rebuild
                        && owner != ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale
                );
                cases += 1;
            }
        }
        assert_eq!(cases, 12);
    }
}
