// @formal evidence session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
use super::*;
use crate::host::worker_resolver::test_support::{
    AdoptionModel, deferred_environment, empty_frozen_projection_for_snapshot, managed_test_host,
    test_activation,
};
use awaken_session_realization_contract::SessionProjectionInstallMode;
use awaken_session_execution_contract::SessionRuntime;
use awaken_session_state_contract::SessionEnvironmentState;

fn observation(host: &SharedHost) -> serde_json::Value {
    host.session_slots.read("projection-convergence", |slot| {
        let receipt = slot.frozen_projection_receipt.as_ref();
        let binding = match &slot.environment_owner {
            crate::session_slot::SessionEnvironmentOwner::Preparing(crate::session_slot::SessionEnvironmentPreparation::AwaitingAdoption { binding, .. }) => binding.as_str(),
            _ => "none",
        };
        serde_json::json!({"exists": receipt.is_some(), "revision": receipt.map_or(0, |r| r.revision.0), "fingerprint": receipt.map_or("none", |r| r.fingerprint.as_str()), "binding": binding})
    }).unwrap_or_else(|| serde_json::json!({"exists": false, "revision": 0, "fingerprint": "none", "binding": "none"}))
}

async fn deliver(
    host: &SharedHost,
    managed: &crate::ManagedHost,
    projection: awaken_session_realization_contract::FrozenSessionProjection,
    rule: String,
    trace: &mut Vec<serde_json::Value>,
) -> Result<(), awaken_session_error_contract::RunError> {
    let before = observation(host);
    let receipt =
        crate::session_slot::FrozenProjectionReceipt::from_projection(&projection).unwrap();
    let incoming = serde_json::json!({"revision": receipt.revision.0, "fingerprint": receipt.fingerprint, "binding": projection.environment.binding().unwrap_or("none")});
    let result = managed
        .realization()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "RealizationRuntime is unsupported by this runtime",
            )
        })?
        .install_session_projection(
            "projection-convergence",
            projection,
            SessionProjectionInstallMode::Dispatch,
        )
        .await;
    trace.push(serde_json::json!({"rule": rule, "before": before, "incoming": incoming, "after": observation(host), "admitted": result.is_ok()}));
    result
}

#[tokio::test]
async fn independent_coordinators_converge_after_binding_replacement_and_reject_late_snapshots() {
    // C1 two independent process-local Hosts receive the same Control snapshot A;
    // C2 one receives newer B before the other; C3 delivery is newer, duplicate,
    // older, or same-revision conflicting; C4 a cold restart has no local receipt.
    // E1 both cached owners become B; E2 replay is idempotent; E3 rejection changes
    // no correlated projection fact; E4 a cold Host accepts authoritative B.
    // R1 A->B / R2 B->B / R3 B->A / R4 B->same-revision C / R5 cold->B.
    // Each step uses ManagedHost -> complete installation -> canonical owner;
    // descriptors are deliberately opaque: a Coordinator must not create/adopt
    // a physical Sandbox to replace its dispatch cache. Physical-owner protection
    // is covered by the Environment lifecycle matrix, not weakened here.
    let snapshot = test_activation("projection-convergence", "run").snapshot;
    let mut a =
        empty_frozen_projection_for_snapshot("workspace", deferred_environment(), &snapshot);
    a.agent_publication = Some(snapshot);
    a.environment = SessionEnvironmentState::Resident {
        binding: "A".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let mut b = a.clone();
    b.revision.0 += 1;
    b.environment = SessionEnvironmentState::Resident {
        binding: "B".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let mut conflict = b.clone();
    conflict.environment = a.environment.clone();
    // R3b a stale root with the SAME payload must still not lower the receipt;
    // R4b equal root + SAME binding + changed logical identity must reject.
    // These kill weakened ordering/equality even when the physical-binding
    // guard would accept, so a changed error string cannot be the only oracle.
    let mut stale_replay = b.clone();
    stale_replay.revision = a.revision;
    let mut identity_conflict = b.clone();
    if let SessionEnvironmentState::Resident { effect_id, .. } = &mut identity_conflict.environment
    {
        *effect_id = Some("contradictory-effect".into());
    }

    let hosts: Vec<_> = (0..3)
        .map(|_| Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub")))
        .collect();
    let managed: Vec<_> = hosts
        .iter()
        .map(|host| managed_test_host(host.clone()))
        .collect();
    let mut trace = Vec::new();
    for i in 0..2 {
        deliver(
            &hosts[i],
            &managed[i],
            a.clone(),
            format!("p{i}-A"),
            &mut trace,
        )
        .await
        .unwrap();
    }
    for i in [1, 0, 2] {
        deliver(
            &hosts[i],
            &managed[i],
            b.clone(),
            format!("p{i}-B"),
            &mut trace,
        )
        .await
        .expect("R1/R5 authoritative replacement");
        deliver(
            &hosts[i],
            &managed[i],
            b.clone(),
            format!("p{i}-replay"),
            &mut trace,
        )
        .await
        .expect("R2 replay");
        for (label, rejected) in [
            ("stale", &a),
            ("stale-replay", &stale_replay),
            ("conflict", &conflict),
            ("identity-conflict", &identity_conflict),
        ] {
            let before = observation(&hosts[i]);
            let error = deliver(
                &hosts[i],
                &managed[i],
                rejected.clone(),
                format!("p{i}-{label}"),
                &mut trace,
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("Session root projection"),
                "R3/R4: {error}"
            );
            assert_eq!(
                error.kind,
                if label.starts_with("stale") {
                    awaken_session_error_contract::RunErrorKind::Unavailable
                } else {
                    awaken_session_error_contract::RunErrorKind::Internal
                },
                "R3 retries a delayed read; R4 rejects contradictory truth",
            );
            assert_eq!(
                observation(&hosts[i]),
                before,
                "E3 all measured coordinates unchanged"
            );
            assert_eq!(before["binding"], "B", "E1 B retained");
            hosts[i]
                .session_slots
                .read("projection-convergence", |slot| {
                    assert!(slot.runtime.is_none(), "no physical realization");
                    assert!(slot.session_dispatch);
                })
                .unwrap();
        }
    }
    awaken_reliability_testkit::write_observation_trace(
        "environment-projection-convergence",
        "environment-projection",
        serde_json::json!(trace),
    );
}
