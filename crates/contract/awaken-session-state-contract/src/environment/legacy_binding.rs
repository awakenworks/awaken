//! The historical raw Environment binding: a Resident binding without an
//! effect id or generation. Two predecessors still deliver it, and both enter
//! the Environment state only through this module so that nothing outside the
//! aggregate's contract constructs or rewrites the state (ADR-0089 D1):
//!
//! * a persisted row written before the typed Environment phase carries the
//!   nullable `environment_binding` column ([`SessionEnvironmentState::from_legacy_binding`]);
//! * a claimed Run carries the claim-first raw binding for the realization
//!   directive's frozen projection
//!   ([`SessionEnvironmentState::deliver_legacy_binding`], decided by
//!   [`super::SessionEnvironmentEvent::LegacyBindingDelivered`]).

use super::{
    SessionEnvironmentEvent, SessionEnvironmentFacts, SessionEnvironmentState,
    SessionEnvironmentStep, transition_environment,
};

impl SessionEnvironmentState {
    /// The Resident value of one raw binding: no effect id, no generation, no
    /// idle edge.
    pub(super) fn legacy_resident(binding: impl Into<String>) -> Self {
        Self::Resident {
            binding: binding.into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        }
    }

    /// Decode the predecessor of the typed Environment phase: a present raw
    /// binding was Resident, an absent one never materialized. A row is decoded,
    /// not transitioned, so no decision applies.
    #[must_use]
    pub fn from_legacy_binding(binding: Option<String>) -> Self {
        binding.map_or(Self::Unmaterialized, Self::legacy_resident)
    }

    /// Merge the claim-first raw binding a claimed Run delivered (if any)
    /// into the Environment state of a realization directive's frozen
    /// projection. A missing binding is not evidence of absence: the
    /// continuation steps omit one too, and stay authoritative. The aggregate
    /// decides through `LegacyBindingDelivered`; only an Unmaterialized
    /// projection becomes Resident, every other step keeps its value, and a
    /// refusal changes nothing. Returns whether the state changed.
    pub fn deliver_legacy_binding(&mut self, binding: Option<&str>) -> bool {
        let Some(binding) = binding else {
            return false;
        };
        let admitted = transition_environment(
            SessionEnvironmentFacts::project_realization_directive(self),
            SessionEnvironmentEvent::LegacyBindingDelivered,
        );
        let enters_resident = matches!(
            admitted,
            Ok(decision) if decision.next() != self.step()
                && decision.next() == SessionEnvironmentStep::Resident
        );
        if enters_resident {
            *self = Self::legacy_resident(binding);
        }
        enters_resident
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SandboxGeneration, SessionEnvironmentOperation, SuspendPhase};

    fn generation() -> SandboxGeneration {
        SandboxGeneration::new(
            &crate::SessionId::from("legacy"),
            1,
            10_000,
            "config",
            "image",
        )
    }

    fn generated_resident() -> SessionEnvironmentState {
        SessionEnvironmentState::Resident {
            binding: "current".into(),
            effect_id: Some("effect".into()),
            generation: Some(generation()),
            idle_since_unix_ms: Some(5),
        }
    }

    fn operation(generation: &SandboxGeneration) -> SessionEnvironmentOperation {
        SessionEnvironmentOperation::new(
            "workspace",
            &crate::SessionId::from("legacy"),
            "suspend",
            generation,
            0,
            None,
            None,
        )
    }

    fn checkpoint(generation: &SandboxGeneration) -> crate::SandboxCheckpointRef {
        crate::SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "awaken-fs-v1".into(),
            digest: "digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 10_000,
            environment_fingerprint: generation.environment_fingerprint().to_owned(),
            base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: operation(generation).effect_id,
        }
    }

    fn suspending(phase: SuspendPhase) -> SessionEnvironmentState {
        let generation = generation();
        SessionEnvironmentState::Suspending {
            source_binding: "current".into(),
            source_effect_id: Box::new("effect".into()),
            operation: operation(&generation),
            generation,
            suspend_phase: phase,
            checkpoint: None,
            source_release_preparation: None,
        }
    }

    #[test]
    fn legacy_row_binding_decodes_to_resident_or_unmaterialized() {
        // Decision table L1. Cause: the predecessor column is a string / absent.
        // Effect: raw Resident (no effect id, generation or idle edge) /
        // Unmaterialized.
        assert_eq!(
            SessionEnvironmentState::from_legacy_binding(Some("raw".into())),
            SessionEnvironmentState::Resident {
                binding: "raw".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
            "L1a"
        );
        assert_eq!(
            SessionEnvironmentState::from_legacy_binding(None),
            SessionEnvironmentState::Unmaterialized,
            "L1b"
        );
    }

    /// Legacy raw-claim delivery (`LegacyBindingDelivered` on a realization
    /// directive's projection, which stands Active and Fenced). C1 the
    /// projection's Environment step; C2 the claim carries a raw binding. E1
    /// only a genuinely legacy Unmaterialized projection becomes the raw
    /// Resident; E2 every resident or continuation step stays byte-for-byte
    /// root-owned; E3 absence is a no-op.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | L1 | Unmaterialized | present | E1 |
    /// | L2 | Resident (raw or generated) / Suspending | present | E2 |
    /// | L3 | Hibernated / Restoring | present | E2; never resurrect the source |
    /// | L4 | any | absent | E3 |
    #[test]
    fn legacy_raw_claim_only_repairs_an_unmaterialized_environment() {
        let raw_claim = "{\"provider_kind\":\"claim\"}";
        let mut legacy = SessionEnvironmentState::Unmaterialized;
        assert!(legacy.deliver_legacy_binding(Some(raw_claim)), "L1 changed");
        assert_eq!(
            legacy,
            SessionEnvironmentState::Resident {
                binding: raw_claim.into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
            "L1"
        );

        let generation = generation();
        let checkpoint = checkpoint(&generation);
        let protected = [
            (
                "L2 raw",
                SessionEnvironmentState::from_legacy_binding(Some("root".into())),
            ),
            ("L2 generated", generated_resident()),
            ("L2 quiescing", suspending(SuspendPhase::Quiescing)),
            ("L2 uploading", suspending(SuspendPhase::Uploading)),
            (
                "L3 hibernated",
                SessionEnvironmentState::Hibernated {
                    checkpoint: checkpoint.clone(),
                    generation: generation.clone(),
                },
            ),
            (
                "L3 restoring",
                SessionEnvironmentState::Restoring {
                    operation: operation(&generation),
                    checkpoint,
                    generation,
                },
            ),
        ];
        for (rule, expected) in protected {
            let mut actual = expected.clone();
            assert!(
                !actual.deliver_legacy_binding(Some(raw_claim)),
                "{rule} changed"
            );
            assert_eq!(actual, expected, "{rule}");
        }

        let mut absent = SessionEnvironmentState::Unmaterialized;
        assert!(!absent.deliver_legacy_binding(None), "L4 changed");
        assert_eq!(absent, SessionEnvironmentState::Unmaterialized, "L4");
    }

    #[test]
    fn directive_facts_admit_the_delivery_row_of_every_step() {
        // L3: the directive projection decides exactly the T1 row of
        // `LegacyBindingDelivered` for every step: Unmaterialized enters
        // Resident, every other step stays, and nothing is instructed.
        for step in [
            SessionEnvironmentStep::Unmaterialized,
            SessionEnvironmentStep::Resident,
            SessionEnvironmentStep::Suspending(SuspendPhase::Quiescing),
            SessionEnvironmentStep::Suspending(SuspendPhase::Uploading),
            SessionEnvironmentStep::Suspending(SuspendPhase::ReadyToDispose),
            SessionEnvironmentStep::Suspending(SuspendPhase::Disposing),
            SessionEnvironmentStep::Hibernated,
            SessionEnvironmentStep::Restoring,
        ] {
            let facts = SessionEnvironmentFacts {
                step,
                ..SessionEnvironmentFacts::project_realization_directive(
                    &SessionEnvironmentState::Unmaterialized,
                )
            };
            let decision =
                transition_environment(facts, SessionEnvironmentEvent::LegacyBindingDelivered)
                    .expect("an active directive projection admits the delivery");
            let expected = if step == SessionEnvironmentStep::Unmaterialized {
                SessionEnvironmentStep::Resident
            } else {
                step
            };
            assert_eq!(decision.next(), expected, "L3 {step:?}");
            assert_eq!(decision.instruction(), None, "L3 {step:?}");
        }
    }
}
