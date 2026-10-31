use super::*;

#[test]
fn unowned_materialization_is_one_absent_root_admission_row() {
    // Causes: C1 a Session root is absent; C2 the intent asserts a lease;
    // C3 its exact identity verifies; C4 kind is Create/Adopt/Rebuild/Reserve.
    // Effects: U1 C1+!C2+C3, every C4 => Unowned preserving the exact intent,
    // no Session instruction or next-state change; U2 C1+C2+C3 =>
    // RealizationStale; U3 !C3 => Mismatch before any authorization. An
    // ordinary Run still uses its original claim; Unowned grants no lease.
    use crate::state::{SessionEnvironmentEvent as E, SessionEnvironmentFacts as F};
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
            let lease = asserted.then(|| crate::state::SessionRealizationLease {
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

#[test]
fn rebuild_source_evidence_is_admission_data_not_identity() {
    // Causes: C1 the intent row predates `rebuild_source` (field absent);
    // C2 the evidence is closed/open × each access value or none.
    // Effects: H1 a historical Rebuild row decodes with no evidence, verifies,
    // and keeps the effect id it was written with; H2 attaching any evidence
    // keeps the effect id (a retry names the same operation) and survives a
    // round trip; H3 an intent without evidence serializes byte-identically
    // to the historical row, so no stored or fingerprinted value changes.
    use crate::state::{RebuildSourceAccess as A, RebuildSourceEvidence as Evidence};
    let lease = crate::state::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "boot".into(),
        epoch: 4,
        expires_at_unix_ms: 10_000,
    };
    let plain = SessionEnvironmentEffectIntent::new(
        "session",
        SessionEnvironmentEffectKind::Rebuild {
            source_generation_id: Some("generation".into()),
        },
        Some(lease),
    )
    .for_environment("fingerprint")
    .from_binding("binding");
    // The exact row shape written before the evidence field existed.
    let historical = serde_json::json!({
        "session_id": "session",
        "effect_id": plain.effect_id(),
        "kind": { "rebuild": { "source_generation_id": "generation" } },
        "source_binding": "binding",
        "environment_fingerprint": "fingerprint",
        "realization": {
            "owner": "worker",
            "runtime_incarnation": "boot",
            "epoch": 4,
            "expires_at_unix_ms": 10_000
        }
    });
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        historical,
        "H3 absent evidence is not written"
    );
    let decoded: SessionEnvironmentEffectIntent = serde_json::from_value(historical).unwrap();
    assert_eq!(decoded.rebuild_source(), None, "H1 no evidence");
    decoded.verify().expect("H1 historical identity verifies");
    assert_eq!(decoded.effect_id(), plain.effect_id(), "H1 same effect id");
    for closed in [false, true] {
        for access in [None, Some(A::RetainedVolume), Some(A::Repository)] {
            let evidence = Evidence::new(closed, access);
            let carried = plain.clone().with_rebuild_source(evidence);
            assert_eq!(carried.effect_id(), plain.effect_id(), "H2 identity");
            carried.verify().expect("H2 verifies");
            let round: SessionEnvironmentEffectIntent =
                serde_json::from_value(serde_json::to_value(&carried).unwrap()).unwrap();
            assert_eq!(round, carried, "H2 round trip");
            assert_eq!(round.rebuild_source(), Some(&evidence), "H2 evidence");
        }
    }
}
