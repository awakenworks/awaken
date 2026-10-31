//! T29-C03 c (产物来源与路径安全 — provenance): the Artifact harvest identity.
//!
//! `ArtifactHarvester` derives every publication's `effect_id` from
//! `harvest_idempotency_key(thread, logical_path, content_id)` and the
//! provider's `pc::Artifact { id, path, size_bytes, content_hash }`. These are
//! the only per-output identity inputs on the harvest edge.
//!
//! | Row | Production | Expected identity |
//! |---|---|---|
//! | P1 | the same Run's output collected twice | identical (idempotent repeated collection) |
//! | P2 | different path or bytes | distinct |
//! | G1 | two Runs of one Session produce the same path and bytes | distinct, each bound to its Run (product gap) |
use super::*;

const THREAD: &str = "t29-c03-session";

/// The harvester's own derivation for one captured output (see
/// `harvest_with_fence_mode_from_environment`).
fn harvest_identity(artifact: &pc::Artifact) -> String {
    let logical_path = pc::WorkspaceLayout::outputs_relative(&artifact.path)
        .or_else(|| artifact.path.strip_prefix("/outputs/"))
        .unwrap_or(artifact.path.trim_start_matches('/'))
        .to_string();
    awaken_resource_contract::harvest_idempotency_key(THREAD, &logical_path, &artifact.content_hash)
}

/// The provider's capture of one output, exactly as every Local tier scans it.
fn captured(path: &str, bytes: &[u8]) -> pc::Artifact {
    let content_hash = awaken_resource_contract::content_id(bytes);
    pc::Artifact {
        id: content_hash.clone(),
        path: path.into(),
        size_bytes: bytes.len() as u64,
        content_hash,
    }
}

#[test]
fn repeated_collection_of_one_output_keeps_one_identity() {
    let first = captured("/mnt/session/outputs/report.txt", b"ok");
    assert_eq!(
        harvest_identity(&first),
        harvest_identity(&captured("/mnt/session/outputs/report.txt", b"ok")),
        "P1 a repeated collection is the same publication"
    );
    for other in [
        captured("/mnt/session/outputs/other.txt", b"ok"),
        captured("/mnt/session/outputs/report.txt", b"changed"),
    ] {
        assert_ne!(harvest_identity(&first), harvest_identity(&other), "P2");
    }
}

#[test]
#[ignore = "product gap: pc::Artifact and harvest_idempotency_key carry no Run identity, so an output re-produced by a later Run is indistinguishable from the original Run's publication"]
fn each_runs_output_is_bound_to_the_run_that_produced_it() {
    // G1: Run A writes report.txt = "ok"; Run B of the same Session later
    // writes the identical report.txt = "ok". The harvest edge observes the
    // same capture for both, so Run B's publication resolves to Run A's
    // durable record (or vice versa) and the origin Run is not recoverable.
    let produced_by_run_a = captured("/mnt/session/outputs/report.txt", b"ok");
    let produced_by_run_b = captured("/mnt/session/outputs/report.txt", b"ok");
    assert_ne!(
        harvest_identity(&produced_by_run_a),
        harvest_identity(&produced_by_run_b),
        "G1/O1 two Runs' productions must not share one publication identity"
    );
}
