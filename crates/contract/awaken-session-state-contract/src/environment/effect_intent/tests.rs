use super::*;

#[test]
fn unowned_materialization_is_one_absent_root_admission_row() {
    // Causes: C1 a Session root is absent; C2 the intent asserts a lease;
    // C3 its exact identity verifies; C4 kind is Create/Adopt/Rebuild/Reserve.
    // Effects: U1 C1+!C2+C3, every C4 => Unowned preserving the exact intent,
    // no Session instruction or next-state change; U2 C1+C2+C3 =>
    // RealizationStale; U3 !C3 => Mismatch before any authorization. An
    // ordinary Run still uses its original claim; Unowned grants no lease.
    use crate::{SessionEnvironmentEvent as E, SessionEnvironmentFacts as F};
    let kinds = [
        SessionEnvironmentEffectKind::Create,
        SessionEnvironmentEffectKind::Adopt,
        SessionEnvironmentEffectKind::Rebuild {
            source_generation_id: None,
        },
        SessionEnvironmentEffectKind::ResourceProjectionReservation {
            transition_fingerprint: "transition".into(),
        },
    ];
    for kind in kinds {
        for asserted in [false, true] {
            let lease = asserted.then(|| crate::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "boot".into(),
                epoch: 4,
                expires_at_unix_ms: 10_000,
            });
            let intent = SessionEnvironmentEffectIntent::new("session", kind.clone(), lease);
            let admitted = SessionEnvironmentEffectAuthorization::admit_unowned(&intent);
            if asserted {
                assert_eq!(
                    admitted,
                    Err(SessionEnvironmentReceiptError::RealizationStale),
                    "U2"
                );
            } else {
                let admitted = admitted.unwrap();
                assert!(
                    matches!(
                        admitted,
                        SessionEnvironmentEffectAuthorization::Unowned { .. }
                    ),
                    "U1"
                );
                assert_eq!(admitted.intent(), &intent, "U1 exact identity");
                let facts = F::project_unowned(&intent);
                let decision = super::super::transition_environment(
                    facts,
                    E::AdmitMaterialization {
                        kind: kind.materialization_kind(),
                        already_applied: false,
                    },
                )
                .unwrap();
                assert!(
                    decision.instruction().is_none(),
                    "U1 no Session instruction"
                );
                assert_eq!(
                    decision.next(),
                    super::super::SessionEnvironmentStep::Unmaterialized,
                    "U1 no state"
                );
            }
            let mut corrupt = intent;
            corrupt.effect_id.push('x');
            assert_eq!(
                SessionEnvironmentEffectAuthorization::admit_unowned(&corrupt),
                Err(SessionEnvironmentReceiptError::Mismatch),
                "U3"
            );
        }
    }
}
