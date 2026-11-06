//! Guard-passing decision table for the filesystem terminal participant.
use super::*;

fn admitted_as<T>(
    fence: &pc::SandboxEffectFence,
    apply: impl FnOnce(&pc::SandboxEffectGuard<'_>) -> Result<T, pc::SandboxError>,
) -> Result<T, pc::SandboxError> {
    pc::with_current_sandbox_effect(fence, &pc::test_support::EffectAuthority::new(fence), apply)
}

#[test]
fn terminal_mutations_require_the_admitted_effect_and_a_current_record() {
    // Causes: C1 the admitted guard names the same effect as the fence passed
    // to the mutation / another operation; C2 the held marker's recorded
    // authority is not newer / newer than the admitted lease. Effects: E1 the
    // marker CAS runs; E2 FenceRejected before any marker or root mutation.
    // | Rule | C1        | C2        | Effect |
    // | A1   | different | current   | E2     |
    // | A2   | same      | newer     | E2     |
    // | A3   | same      | current   | E1     |
    // A2 is the TOCTOU row: an effect admitted for an older lease cannot
    // remove the root after a successor recorded its takeover in the marker,
    // even though the older process still holds its in-memory participant.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("guarded");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("guarded"));
    let create = fence("create", 1);
    let mut creation = begin(&root, &fingerprint, &create, None, None).unwrap();
    assert!(
        matches!(
            admitted_as(&fence("other-create", 1), |admitted| creation
                .prepare_root(admitted)),
            Err(pc::SandboxError::FenceRejected(ref reason))
                if reason.contains("different effect than its admitted guard")
        ),
        "A1 creation"
    );
    let evidence = creation
        .recorded_admission(|held, admitted| {
            held.prepare_root(admitted)?;
            held.complete(&empty_receipt(), admitted)
        })
        .unwrap();
    let terminal = fence("terminal", 1);
    let (_, mut removal) = admitted_as(&terminal, |_| {
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&create),
            &terminal,
            None,
        )
    })
    .unwrap()
    .expect("participant");
    let marker_before = read_marker(&root).unwrap();

    let other = fence("other-terminal", 1);
    assert!(
        matches!(
            admitted_as(&other, |admitted| removal
                .prepare_disposal_effect(&terminal, admitted)),
            Err(pc::SandboxError::FenceRejected(ref reason))
                if reason.contains("different effect than its admitted guard")
        ),
        "A1 preparation"
    );
    assert_eq!(read_marker(&root).unwrap(), marker_before, "A1 no mutation");

    // A successor lease records its takeover in the durable marker.
    let successor = fence("terminal", 2);
    admitted_as(&successor, |admitted| {
        removal.refresh_authorization(&successor, admitted)
    })
    .expect("successor takeover");
    let recorded = read_marker(&root).unwrap();
    for (rule, result) in [
        (
            "A2 remove",
            admitted_as(&terminal, |admitted| removal.remove_root(admitted)),
        ),
        (
            "A2 prepare",
            admitted_as(&terminal, |admitted| {
                removal
                    .prepare_disposal_effect(&terminal, admitted)
                    .map(|_| ())
            }),
        ),
    ] {
        assert!(
            matches!(
                result,
                Err(pc::SandboxError::FenceRejected(ref reason))
                    if reason.contains("newer than the admitted effect")
            ),
            "{rule} rejected by the guard, before provider-specific checks"
        );
        assert_eq!(read_marker(&root).unwrap(), recorded, "{rule} no mutation");
        assert!(root.exists(), "{rule} root retained");
    }
    admitted_as(&successor, |admitted| {
        removal.prepare_disposal_effect(&successor, admitted)
    })
    .expect("A3 current lease");
}
