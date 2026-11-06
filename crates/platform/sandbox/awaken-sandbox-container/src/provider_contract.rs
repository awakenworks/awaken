// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Defines the fail-closed backend-erased provider and live-environment authority boundary;
// @formal reason: this is a product-requirement classification, not a formal-proof claim.
//! Backend-erased contracts for Session-owned container environments.

use crate::ContainerHandleCodec as _;

use awaken_provisioning_contract::{
    SandboxCreateAttempt, SandboxEffect, SandboxEffectFence, SandboxObservationPurpose,
    SandboxRealizationIntent,
};

/// Exact physical reservation returned by the backend authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContainer {
    pub locator: String,
    pub authority_attempt_id: String,
    pub disposition: PreparedContainerDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedContainerDisposition {
    FreshReservation,
    RecoveringReservation,
    AlreadyActive,
}

impl PreparedContainer {
    pub fn fresh(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::FreshReservation,
        }
    }

    pub fn recovering(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::RecoveringReservation,
        }
    }

    pub fn already_active(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::AlreadyActive,
        }
    }
}

use awaken_provisioning_contract as pc;

/// Stable namespace used to discover a provider effect after a Worker process
/// replacement. Construction is centralized here so adapters cannot disagree
/// about delimiter escaping, validation, or whether a process incarnation is
/// part of durable identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContainerRealizationNamespace(String);

impl ContainerRealizationNamespace {
    pub fn from_stable_parts<'a>(
        parts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, pc::SandboxError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"awaken-container-realization-namespace/v1\0");
        let mut count = 0_u64;
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                return Err(pc::SandboxError::invalid(
                    "container realization namespace has an empty stable identity part",
                ));
            }
            count = count.saturating_add(1);
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        if count == 0 {
            return Err(pc::SandboxError::invalid(
                "container realization namespace requires stable identity evidence",
            ));
        }
        hasher.update(&count.to_be_bytes());
        Ok(Self(hasher.finalize().to_hex().to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod realization_namespace_tests {
    use super::*;

    #[test]
    fn stable_namespace_decision_table_uses_every_installation_fact() {
        // Cause/effect table: C1 ordered stable installation facts are
        // equal/different; C2 process incarnation changes; C3 any stable part is
        // empty. R1 exact C1 produces the same namespace across C2; R2 any C1
        // difference produces a different namespace; R3 C3 rejects rather than
        // falling back to a process identity. Callers own which installation
        // facts are stable; the generic value object owns only exact hashing.
        let first = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
        ])
        .unwrap();
        let restarted = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
        ])
        .unwrap();
        let other_workspace = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-b",
        ])
        .unwrap();
        assert_eq!(first, restarted, "R1");
        assert_ne!(first, other_workspace, "R2/C1");
        assert!(
            ContainerRealizationNamespace::from_stable_parts(["worker-installation", "",]).is_err(),
            "R3"
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerRealizationIntent {
    Create,
    /// Replace only the exact physical incarnation carried by the already
    /// authorized source SandboxHandle. Stable scope and lease age alone are
    /// never deletion authority.
    Rebuild {
        source_incarnation: String,
        /// Exact runtime-owned continuation evidence from the aggregate-authorized
        /// source handle. Kubernetes revalidates the persisted claim UID before
        /// it may recreate an absent Pod; host-bind runtimes carry `None` and
        /// therefore cannot turn an absent retained workspace into a fresh one.
        source_runtime_handle: Option<crate::ContainerContinuationHandle>,
    },
}

impl ContainerRealizationIntent {
    /// Interpret the backend-erased intent a Session owner sends: a rebuild's
    /// source handle is decoded by this provider's own codec into the exact
    /// physical incarnation and runtime continuation evidence it replaces. A
    /// handle this codec cannot read fails closed before any backend effect.
    pub fn from_environment(intent: SandboxRealizationIntent) -> Result<Self, pc::SandboxError> {
        match intent {
            SandboxRealizationIntent::Create => Ok(Self::Create),
            SandboxRealizationIntent::Rebuild { source } => Ok(Self::Rebuild {
                source_incarnation: source.container_physical_incarnation()?,
                source_runtime_handle: source.container_payload()?.runtime_handle,
            }),
        }
    }

    #[must_use]
    pub fn source_incarnation(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Rebuild {
                source_incarnation, ..
            } => Some(source_incarnation),
        }
    }

    #[must_use]
    pub fn source_runtime_handle(&self) -> Option<&crate::ContainerContinuationHandle> {
        match self {
            Self::Create => None,
            Self::Rebuild {
                source_runtime_handle,
                ..
            } => source_runtime_handle.as_ref(),
        }
    }
}

/// One canonical, borrowed context shared by every runtime realization phase.
///
/// The context does not own or reinterpret any identity. It keeps the existing
/// adoption fingerprint, effect fence, lifecycle intent, and create-attempt
/// authorities together. The mutable [`crate::ContainerPlan`] and phase-specific
/// physical fingerprint remain explicit port arguments and retain their
/// existing compile-time lifecycle.
#[derive(Clone, Copy, Debug)]
pub struct ContainerRealizationContext<'a> {
    pub scope: &'a str,
    pub adoption_fingerprint: &'a pc::SandboxRealizationFingerprint,
    pub effect_fence: Option<&'a SandboxEffectFence>,
    pub intent: &'a ContainerRealizationIntent,
    pub attempt: &'a SandboxCreateAttempt,
    // Borrow the provider's original effect; this context owns
    // neither a lease nor permission to substitute a newly observed grant.
    effect: SandboxEffect<'a>,
}

impl<'a> ContainerRealizationContext<'a> {
    #[must_use]
    pub fn new(
        scope: &'a str,
        adoption_fingerprint: &'a pc::SandboxRealizationFingerprint,
        effect_fence: Option<&'a SandboxEffectFence>,
        intent: &'a ContainerRealizationIntent,
        attempt: &'a SandboxCreateAttempt,
    ) -> Self {
        Self {
            scope,
            adoption_fingerprint,
            effect_fence,
            intent,
            attempt,
            effect: SandboxEffect::Unattached,
        }
    }

    pub fn with_effect(mut self, effect: SandboxEffect<'a>) -> Self {
        self.effect = effect;
        self
    }

    pub fn admit_current_effect(&self) -> Result<(), pc::SandboxError> {
        // Renewal may change the copied expiry; the original captured effect
        // still owns admission. Reconcile cannot attach another operation,
        // owner, incarnation or epoch to this static physical projection.
        match (self.effect_fence, self.effect.fence()) {
            (None, None) => self.effect.admit(),
            (Some(current), Some(captured)) if current.same_effect_identity(captured) => {
                self.effect.admit()
            }
            _ => Err(pc::SandboxError::fence_rejected(
                "container effect cleanup lacks its captured current authority",
            )),
        }
    }

    #[must_use]
    pub fn with_attempt<'b>(
        &'b self,
        attempt: &'b SandboxCreateAttempt,
    ) -> ContainerRealizationContext<'b> {
        ContainerRealizationContext { attempt, ..*self }
    }
}

#[cfg(test)]
mod realization_context_tests {
    use super::*;

    #[test]
    fn realization_context_preserves_one_exact_effect_identity() {
        // Cause/effect decision table: C1 fence is absent/present; C2 intent is
        // Create/Rebuild; C3 attempt is original/recovery. Q1 one context
        // preserves scope, adoption, C1, C2, and original C3 by reference; Q2
        // replacing C3 changes only the ephemeral attempt witness. The context
        // borrows the existing intent/fence/attempt authorities and creates no
        // parallel identity. Phase-specific physical fingerprints remain typed
        // arguments on the runtime ports and are covered by the provider test.
        // Q3 no fence needs no aggregate authority; a copied fence alone cannot
        // admit cleanup. Q4 attach the original current authority, then revoke
        // it after with_attempt: recovery must reject that same captured proof,
        // rather than forgetting its checkpoint or substituting another grant.
        // Q5 renewed copied expiry preserves exact effect identity and the
        // original admission; different operation/owner/incarnation/epoch or
        // omitted fence rejects before cleanup. The copied expiry is no grant.
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "session-a".into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            control_services: Default::default(),
            lease_ttl_secs: None,
        };
        let adoption = pc::SandboxRealizationFingerprint::from_spec(&spec);
        let fence = SandboxEffectFence::new("operation-a", "owner-a", "runtime-a", 1, u64::MAX - 1)
            .unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = SandboxCreateAttempt::new("attempt-a");
        let recovery_attempt = SandboxCreateAttempt::new("attempt-recovery");

        let context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption,
            Some(&fence),
            &intent,
            &attempt,
        );
        assert_eq!(context.scope, spec.scope, "Q1 scope");
        assert_eq!(context.adoption_fingerprint, &adoption, "Q1 adoption");
        assert_eq!(context.effect_fence, Some(&fence), "Q1 fence");
        assert_eq!(context.intent, &intent, "Q1 intent");
        assert_eq!(context.attempt, &attempt, "Q1 attempt");

        let recovery = context.with_attempt(&recovery_attempt);
        assert_eq!(recovery.attempt, &recovery_attempt, "Q2 attempt");
        assert_eq!(recovery.scope, context.scope, "Q2 scope");
        assert_eq!(
            recovery.adoption_fingerprint, context.adoption_fingerprint,
            "Q2 adoption"
        );

        let unattached =
            ContainerRealizationContext::new(&spec.scope, &adoption, None, &intent, &attempt);
        unattached.admit_current_effect().expect("Q3 unattached");
        assert!(
            context.admit_current_effect().is_err(),
            "Q3 fence is not a grant"
        );

        let authority = pc::test_support::EffectAuthority::new(&fence);
        let attached = context.with_effect(SandboxEffect::Fenced {
            fence: &fence,
            authority: &authority,
        });
        attached.admit_current_effect().expect("Q4 current");
        for (operation, owner, incarnation, epoch, admitted) in [
            ("operation-a", "owner-a", "runtime-a", 1, true),
            ("operation-b", "owner-a", "runtime-a", 1, false),
            ("operation-a", "owner-b", "runtime-a", 1, false),
            ("operation-a", "owner-a", "runtime-b", 1, false),
            ("operation-a", "owner-a", "runtime-a", 2, false),
        ] {
            let projected =
                SandboxEffectFence::new(operation, owner, incarnation, epoch, u64::MAX).unwrap();
            let projected_context = ContainerRealizationContext {
                effect_fence: Some(&projected),
                ..attached
            };
            assert_eq!(
                projected_context.admit_current_effect().is_ok(),
                admitted,
                "Q5 exact identity; copied expiry may renew"
            );
        }
        let omitted = ContainerRealizationContext {
            effect_fence: None,
            ..attached
        };
        assert!(
            omitted.admit_current_effect().is_err(),
            "Q5 no omitted fence selects unattached"
        );
        let recovered = attached.with_attempt(&recovery_attempt);
        *authority.current.lock().unwrap() = None;
        assert!(
            recovered.admit_current_effect().is_err(),
            "Q4 captured proof revoked"
        );
    }
}

/// Complete durable evidence presented to one runtime's effect-free observer.
/// The runtime adapter, not the Host, interprets backend-specific continuation
/// evidence (for example Kubernetes Pod and PVC UIDs). V1 has no realization
/// fingerprint and is therefore continuity-only: it may report Ready or
/// Provisioning but can never prove destructive unavailability.
#[derive(Debug, Clone, Copy)]
pub struct ContainerObservationExpectation<'a> {
    pub purpose: SandboxObservationPurpose,
    pub container_id: &'a str,
    pub adoption_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub realization_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub runtime_handle: Option<&'a crate::ContainerContinuationHandle>,
    pub effect_fence: Option<&'a pc::SandboxEffectFence>,
    pub retained_attempt: Option<&'a SandboxCreateAttempt>,
}

/// One durable container handle decoded once by its owning codec, so an
/// observation expectation can borrow its locator and fingerprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedContainerHandle {
    pub payload: crate::ContainerSandboxHandleV1,
    pub adoption_fingerprint: Option<pc::SandboxRealizationFingerprint>,
    pub realization_fingerprint: Option<pc::SandboxRealizationFingerprint>,
}

impl DecodedContainerHandle {
    pub fn decode(handle: &pc::SandboxHandle) -> Result<Self, pc::SandboxError> {
        Ok(Self {
            payload: handle.container_payload()?,
            adoption_fingerprint: handle.container_adoption_fingerprint()?,
            realization_fingerprint: handle.realization_fingerprint().cloned(),
        })
    }
}

impl<'a> ContainerObservationExpectation<'a> {
    #[must_use]
    pub fn from_decoded(handle: &'a DecodedContainerHandle) -> Self {
        Self {
            purpose: SandboxObservationPurpose::Recovery,
            container_id: &handle.payload.container_id,
            adoption_fingerprint: handle.adoption_fingerprint.as_ref(),
            realization_fingerprint: handle.realization_fingerprint.as_ref(),
            runtime_handle: handle.payload.runtime_handle.as_ref(),
            effect_fence: None,
            retained_attempt: None,
        }
    }

    #[must_use]
    pub fn from_decoded_for_effect(
        handle: &'a DecodedContainerHandle,
        effect_fence: &'a pc::SandboxEffectFence,
    ) -> Self {
        Self {
            effect_fence: Some(effect_fence),
            ..Self::from_decoded(handle)
        }
    }
}
