//! Artifact capture provenance uses the existing FileCatalog CAS and Run fence.
//! Content-addressed File reuse is independent from a capture's publishing Run.
use super::*;
use awaken_lease_contract::fence::Epoch;
use awaken_resource_contract::ReplaceArtifactCaptureOutcome;

fn fence(run: &str, epoch: Epoch) -> ResourceOperationFence {
    ResourceOperationFence::Run(awaken_worker_contract::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id(run.into()),
        owner: "worker".into(),
        epoch: epoch.get(),
    })
}

#[test]
fn repeated_collection_of_one_output_keeps_one_identity() {
    // P1 exact request replay with the same Run/fence, revision and File set
    // returns the same capture. P2 a stale expected revision with a different
    // Run cannot overwrite it. Use the production capture compiler and reducer,
    // not a second derivation from provider bytes that have no Run authority.
    let first = ArtifactHarvester::capture_replacement(
        "workspace".into(),
        "session",
        0,
        vec!["file-report".into()],
        Some(&fence("run-a", Epoch::FIRST)),
    )
    .unwrap();
    let ReplaceArtifactCaptureOutcome::Applied(capture) = first.decide(None).unwrap() else {
        panic!("P1 first capture");
    };
    let replay = first.decide(Some(&capture)).unwrap();
    assert_eq!(
        replay,
        ReplaceArtifactCaptureOutcome::Replayed(capture.clone()),
        "P1"
    );
    first.verify_outcome(&replay).unwrap();
    let stale = ArtifactHarvester::capture_replacement(
        "workspace".into(),
        "session",
        0,
        vec!["file-report".into()],
        Some(&fence("run-b", Epoch::FIRST.next().unwrap())),
    )
    .unwrap();
    assert_eq!(
        stale.decide(Some(&capture)).unwrap(),
        ReplaceArtifactCaptureOutcome::Conflict,
        "P2"
    );
}

#[test]
fn each_runs_output_is_bound_to_the_run_that_produced_it() {
    // | Rule | File ids | Run/fence       | Expected capture                  |
    // | G1   | same     | A then B        | distinct revision + exact origin |
    // | G2   | same     | cold replay B   | B's origin survives serialization|
    // | G3   | same     | forged A for B  | denied before replacement        |
    // | G4   | same     | no Run fence    | no invented Run provenance       |
    // | G5   | same     | zero epoch      | invalid request                  |
    // The existing capture, not the provider Artifact or content-dedup key,
    // owns Run provenance. This also protects embedded/local publishers: the
    // harvester binds its expected origin before accepting any peer receipt.
    let files = vec!["file-report".into()];
    let first = ArtifactHarvester::capture_replacement(
        "workspace".into(),
        "session",
        0,
        files.clone(),
        Some(&fence("run-a", Epoch::FIRST)),
    )
    .unwrap();
    let ReplaceArtifactCaptureOutcome::Applied(first_capture) = first.decide(None).unwrap() else {
        panic!("G1 first capture");
    };
    let second = ArtifactHarvester::capture_replacement(
        "workspace".into(),
        "session",
        1,
        files.clone(),
        Some(&fence("run-b", Epoch::FIRST.next().unwrap())),
    )
    .unwrap();
    let ReplaceArtifactCaptureOutcome::Applied(second_capture) =
        second.decide(Some(&first_capture)).unwrap()
    else {
        panic!("G1 successor capture");
    };
    assert_eq!(
        first_capture.file_ids, second_capture.file_ids,
        "G1 immutable File reuse"
    );
    assert_ne!(
        first_capture.origin, second_capture.origin,
        "G1 different publishing Runs"
    );
    assert_eq!(second_capture.revision, 2, "G1");
    let origin = second_capture
        .origin
        .as_ref()
        .expect("G1 exact origin is mandatory");
    assert_eq!(
        origin.identity(),
        ("run-b", "worker", std::num::NonZeroU64::new(2).unwrap()),
        "G1"
    );
    let loaded: awaken_resource_contract::ArtifactCapture =
        serde_json::from_slice(&serde_json::to_vec(&second_capture).unwrap()).unwrap();
    second
        .verify_outcome(&ReplaceArtifactCaptureOutcome::Replayed(loaded.clone()))
        .unwrap();
    assert_eq!(loaded, second_capture, "G2");
    let mut forged = second.clone();
    forged.origin = first_capture.origin.clone();
    assert!(
        matches!(
            forged.bind_run_origin(Some(("run-b", "worker", 2))),
            Err(ArtifactPublicationError::Denied { .. })
        ),
        "G3"
    );
    let mut unbound_receipt = second_capture.clone();
    unbound_receipt.origin = None;
    assert!(
        second
            .verify_outcome(&ReplaceArtifactCaptureOutcome::Replayed(unbound_receipt))
            .is_err(),
        "G3 a peer cannot erase the harvester's expected origin"
    );
    let unclaimed = ArtifactHarvester::capture_replacement(
        "workspace".into(),
        "session",
        0,
        files.clone(),
        None,
    )
    .unwrap();
    assert!(unclaimed.origin.is_none(), "G4");
    let mut invalid = fence("run-a", Epoch::FIRST);
    let ResourceOperationFence::Run(claim) = &mut invalid else {
        panic!("G5 requires a Run fence");
    };
    claim.epoch = 0; // Deliberately malformed persisted wire data.
    assert!(
        matches!(
            ArtifactHarvester::capture_replacement(
                "workspace".into(),
                "session",
                0,
                files,
                Some(&invalid)
            ),
            Err(ArtifactPublicationError::InvalidRequest { .. })
        ),
        "G5"
    );
}
