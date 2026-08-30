use super::*;

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    }
}

fn fence(operation: &str, epoch: u64) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(operation, "owner", "runtime", epoch, u64::MAX).unwrap()
}

fn disposal_authorization(
    prepared: &pc::SandboxEffectFence,
    fingerprint: &str,
    owner: &str,
    runtime: &str,
    epoch: u64,
) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
    let successor = pc::SandboxEffectFence::new(
        preparation.operation_id().unwrap(),
        owner,
        runtime,
        epoch,
        u64::MAX,
    )
    .unwrap();
    preparation.authorize(successor).unwrap()
}

fn source<'a>(
    fingerprint: &'a pc::SandboxRealizationFingerprint,
    evidence: &'a RealizationEvidence,
) -> RebuildSource<'a> {
    RebuildSource {
        fingerprint,
        effect_fence: evidence.effect_fence(),
        physical_incarnation: evidence.physical_incarnation(),
    }
}

fn empty_receipt() -> RealizationCompletionReceipt {
    RealizationCompletionReceipt::new(&[], Vec::new()).unwrap()
}

#[test]
fn physical_attempt_incarnation_is_random_and_not_effect_derived() {
    // Incarnation cause/effect table: C1 immutable spec/effect inputs are
    // equal/different; C2 physical attempt is same/restarted. I1 the same
    // admitted attempt reuses its persisted marker value; I2 two newly
    // allocated attempts receive distinct opaque values even with identical
    // inputs. The lifecycle tests below own I1; this row proves I2 cannot be
    // reconstructed from fingerprint/fence and accidentally authorize ABA.
    let root = Path::new("/tmp/random-incarnation");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("random"));
    let effect = fence("create", 1);
    let first = marker_for_new_attempt(
        root,
        &fingerprint,
        &effect,
        RealizationPhase::Creating,
        None,
        None,
    )
    .unwrap();
    let second = marker_for_new_attempt(
        root,
        &fingerprint,
        &effect,
        RealizationPhase::Creating,
        None,
        None,
    )
    .unwrap();
    assert_ne!(
        first.physical_incarnation, second.physical_incarnation,
        "I2"
    );
    assert_eq!(first.physical_incarnation.len(), 64, "I2 opaque encoding");
    assert_eq!(second.physical_incarnation.len(), 64, "I2 opaque encoding");
}

#[test]
fn provider_creation_guard_preserves_current_and_legacy_completion_and_abort() {
    // Provider guard decision table: C1 authority is current/legacy; C2 outcome
    // is complete/abort. G1 current+complete publishes fenced Current evidence;
    // G2 current+abort removes its exact root and marker; G3 legacy+complete
    // returns only one-process LegacyCreated evidence; G4 legacy+abort removes
    // its exact root without minting a marker. Heap indirection of the larger
    // current guard must not change any row or release its lock before the row's
    // terminal action.
    let base = tempfile::tempdir().unwrap();
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("guard"));

    let current_complete_root = base.path().join("current-complete");
    let mut current_complete = ProviderCreationGuard::Current(Box::new(
        begin(
            &current_complete_root,
            &fingerprint,
            &fence("current-complete", 1),
            None,
            None,
        )
        .unwrap(),
    ));
    current_complete.prepare_root().unwrap();
    assert!(
        matches!(
            current_complete.complete(&empty_receipt()).unwrap(),
            LiveRealization::Current(_)
        ),
        "G1"
    );

    let current_abort_root = base.path().join("current-abort");
    let mut current_abort = ProviderCreationGuard::Current(Box::new(
        begin(
            &current_abort_root,
            &fingerprint,
            &fence("current-abort", 1),
            None,
            None,
        )
        .unwrap(),
    ));
    current_abort.prepare_root().unwrap();
    current_abort.abort_creation().unwrap();
    assert!(!current_abort_root.exists(), "G2");

    let legacy_complete_root = base.path().join("legacy-complete");
    let mut legacy_complete =
        ProviderCreationGuard::Legacy(begin_legacy(&legacy_complete_root).unwrap());
    legacy_complete.prepare_root().unwrap();
    assert!(
        matches!(
            legacy_complete.complete(&empty_receipt()).unwrap(),
            LiveRealization::LegacyCreated(_)
        ),
        "G3"
    );

    let legacy_abort_root = base.path().join("legacy-abort");
    let mut legacy_abort = ProviderCreationGuard::Legacy(begin_legacy(&legacy_abort_root).unwrap());
    legacy_abort.prepare_root().unwrap();
    legacy_abort.abort_creation().unwrap();
    assert!(!legacy_abort_root.exists(), "G4");
}

#[test]
fn fenced_marker_phase_root_fence_and_incarnation_table_is_total() {
    // Cause/effect graph: C1 phase is Creating/Ready/Recreating; C2 root is
    // absent/exact/foreign; C3 incoming fence is exact/newer/foreign; C4
    // rebuild source is absent/exact/stale; C5 restore checkpoint input is
    // absent/exact/drifted. Effects: E1 exact response-loss
    // reuses one inode/incarnation; E2 Ready+present rejects every distinct
    // effect; E3 Ready+absent rebuilds only from the exact V2 source and
    // changes incarnation; E4 Recreating observes Provisioning and exact
    // effect retry resumes; E5 foreign identities never mutate.
    //
    // | Rule | phase/root | fence/source | Effect |
    // |---|---|---|---|
    // | M1 | Creating/exact | exact/none | resume same inode |
    // | M2 | Ready/exact | distinct/any | reject, preserve bytes |
    // | M3 | Ready/absent | newer/exact | Recreating, new incarnation |
    // | M4 | Recreating/exact | exact/exact | Provisioning/resume |
    // | M5 | any/foreign | any | reject, zero mutation |
    // | M6 | Ready/exact | same effect/different restore fp | reject, preserve bytes |
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("session");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("session"));
    let create = fence("create", 1);

    let mut first = begin(&root, &fingerprint, &create, None, None).expect("M1 begin");
    let identity = required_root_identity(&first.marker).unwrap();
    std::fs::write(root.join("partial"), b"partial").unwrap();
    first.prepare_root().unwrap();
    assert_eq!(
        awaken_sandbox_fs::directory_identity_nofollow(&root).unwrap(),
        identity
    );
    let first_evidence = first.complete(&empty_receipt()).unwrap();
    let handle_incarnation = first_evidence.physical_incarnation().to_string();

    std::fs::write(root.join("preserved"), b"ready").unwrap();
    assert!(
        begin(&root, &fingerprint, &fence("other", 1), None, None).is_err(),
        "M2"
    );
    assert_eq!(std::fs::read(root.join("preserved")).unwrap(), b"ready");

    awaken_sandbox_fs::remove_directory_tree_exact(&root, identity).unwrap();
    let rebuild = fence("rebuild", 2);
    let mut recreating = begin(
        &root,
        &fingerprint,
        &rebuild,
        Some(source(&fingerprint, &first_evidence)),
        None,
    )
    .expect("M3");
    assert_eq!(recreating.admission(), Admission::Recreating);
    assert_ne!(
        recreating.marker.physical_incarnation, handle_incarnation,
        "M3"
    );
    assert_eq!(
        observe_adoption(
            &root,
            Some(&fingerprint),
            Some(&rebuild),
            Some(&recreating.marker.physical_incarnation),
            Some(&rebuild),
        )
        .unwrap(),
        pc::SandboxObservation::Provisioning,
        "M4"
    );
    recreating.prepare_root().unwrap();
    let rebuilt = recreating.complete(&empty_receipt()).unwrap();

    let replacement = base.path().join("foreign");
    std::fs::create_dir(&replacement).unwrap();
    let rebuilt_identity = rebuilt.root_identity();
    awaken_sandbox_fs::remove_directory_tree_exact(&root, rebuilt_identity).unwrap();
    std::fs::rename(&replacement, &root).unwrap();
    assert!(
        begin(&root, &fingerprint, &rebuild, None, None).is_err(),
        "M5"
    );
    assert!(root.exists(), "M5 foreign root preserved");

    let restore_root = base.path().join("restore-replay");
    let restore_fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("restore-replay"));
    let restore_effect = fence("restore", 4);
    let mut restore = begin(
        &restore_root,
        &restore_fingerprint,
        &restore_effect,
        None,
        Some("checkpoint-a"),
    )
    .unwrap();
    restore.prepare_root().unwrap();
    restore.complete(&empty_receipt()).unwrap();
    std::fs::write(restore_root.join("preserved"), b"checkpoint-a").unwrap();
    assert!(
        begin(
            &restore_root,
            &restore_fingerprint,
            &restore_effect,
            None,
            Some("checkpoint-b"),
        )
        .is_err(),
        "M6"
    );
    assert_eq!(
        std::fs::read(restore_root.join("preserved")).unwrap(),
        b"checkpoint-a",
        "M6 zero write"
    );
}

#[test]
fn terminal_takeover_phase_root_fence_and_crash_table_is_total() {
    // Terminal cause/effect graph: C1 phase is Creating/Recreating/Ready/
    // Removing/Removed; C2 physical state is absent/exact root/exact private
    // stage/rename-response-loss/foreign; C3 handle is exact/stale/absent;
    // C4 expected operation is exact/drifted; C5 terminal fence is live/
    // stale/foreign; C6 crash is before delete/after delete/before receipt.
    // Effects: T1 exact Ready enters Removing under one guard; T2 delete
    // response loss reuses the recorded source and finishes Removed; T3
    // Removed returns no participant; T4 old-incarnation ABA and foreign
    // evidence cause zero mutation; T5 handle-free partial restore accepts
    // only its exact creating fence, including an unpublished stage cut; T6
    // an aggregate-authorized successor operation in the same realization
    // lease may take over Removing, including through a retained guard.
    //
    // | Rule | phase/root | handle/expected/terminal | Effect |
    // |---|---|---|---|
    // | T1 | Ready/exact | exact/-/live | Some guard, Removing |
    // | T2 | Removing/absent | exact/recorded/same | finish receipt |
    // | T3 | Removed/absent | exact/recorded/same | None |
    // | T4 | Ready/new incarnation | stale/any/any | reject, no delete |
    // | T5 | Creating/stage or absent | none/exact/live | remove or None |
    // | T6 | Removing/exact | exact/recorded/same lease successor | rebind guard |
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("terminal");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("terminal"));
    let create = fence("create", 1);
    let mut guard = begin(&root, &fingerprint, &create, None, None).unwrap();
    guard.prepare_root().unwrap();
    let evidence = guard.complete(&empty_receipt()).unwrap();
    let terminal = fence("terminal", 1);

    assert!(
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&fence("same-lease-other-operation", 1)),
            &terminal,
        )
        .is_err(),
        "T4 expected operation identity must exactly equal the handle source"
    );
    let (_, removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &terminal,
    )
    .expect("T1")
    .expect("T1 participant");
    drop(removal); // aggregate successor after an interrupted finalizer
    let successor = fence("terminal-successor", 1);
    let (_, mut removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &successor,
    )
    .expect("T6 restart takeover")
    .expect("T6 participant");
    let final_effect = fence("terminal-final", 1);
    removal
        .refresh_authorization(&final_effect)
        .expect("T6 retained-guard takeover");
    removal
        .authorize_disposal(&disposal_authorization(
            &final_effect,
            "terminal-final-preparation",
            "owner",
            "runtime",
            1,
        ))
        .expect("T6 aggregate-authorized physical authorization");
    removal.remove_root().unwrap();
    drop(removal); // crash before Removed publication
    assert!(
        matches!(
            observe_adoption(
                &root,
                Some(&fingerprint),
                Some(&fence("forged-create", 1)),
                Some(evidence.physical_incarnation()),
                Some(&final_effect),
            )
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "T4 marker requires both the opaque incarnation and original handle fence"
    );
    let (_, replay) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &final_effect,
    )
    .expect("T2 exact terminal reconstruction")
    .expect("T2 participant");
    replay.finish().unwrap();
    assert!(
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&create),
            &final_effect,
        )
        .unwrap()
        .is_none(),
        "T3"
    );
    assert!(
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&create),
            &fence("foreign", 0),
        )
        .is_err(),
        "T4"
    );

    let partial_root = base.path().join("partial-restore");
    let partial_fingerprint =
        pc::SandboxRealizationFingerprint::from_spec(&spec("partial-restore"));
    let restore = fence("restore", 3);
    let mut marker = marker_for_new_attempt(
        &partial_root,
        &partial_fingerprint,
        &restore,
        RealizationPhase::Creating,
        None,
        Some("restore-input"),
    )
    .unwrap();
    let lock = acquire(&partial_root).unwrap();
    publish_marker_locked(&partial_root, &lock, &marker).unwrap();
    let stage = stage_path(&partial_root, &marker.root).unwrap();
    let previous = marker.clone();
    marker.root.identity = Some(
        lock.create_sibling_directory_noreplace(stage_leaf(&marker.root).unwrap())
            .unwrap()
            .into(),
    );
    replace_marker_locked(&partial_root, &lock, &previous, &marker).unwrap();
    drop(lock);
    assert!(
        begin_terminal_takeover(
            &partial_root,
            &partial_fingerprint,
            None,
            Some(&fence("wrong-restore", 3)),
            &fence("terminal-restore", 3),
        )
        .is_err(),
        "T5 drifted restore evidence"
    );
    let partial_preparation = fence("terminal-restore", 3);
    let (_, mut partial) = begin_terminal_takeover(
        &partial_root,
        &partial_fingerprint,
        None,
        Some(&restore),
        &partial_preparation,
    )
    .unwrap()
    .expect("T5 exact stage participant");
    assert_eq!(partial.owned_root().unwrap().unwrap().0, stage, "T5");
    partial
        .authorize_disposal(&disposal_authorization(
            &partial_preparation,
            "partial-restore-preparation",
            "owner",
            "runtime",
            3,
        ))
        .expect("T5 aggregate-authorized physical authorization");
    partial.remove_root().unwrap();
    partial.finish().unwrap();
}

#[test]
fn removing_reconstruction_preserves_preparation_until_the_canonical_mutation_edge() {
    /* Removing reconstruction table T7. Causes: C1 the marker is already
     * Removing under a durable provider preparation A; C2 a terminal recovery
     * presents an authorized later source-effect fence T, or a lower-epoch,
     * shorter-expiry, or foreign fence; C3 physical disposal presents exact
     * A/fpA->C or the same A with a different fingerprint; C4 an ordinary
     * terminal cleanup has no prior A and invokes an explicit takeover-fence
     * mutation after reconstruction. Effects: E1 reconstruction validates T
     * but preserves immutable A; E2 exact A/fpA->C is admitted; E3 stale,
     * foreign, shorter, and different-fingerprint requests leave marker and
     * root unchanged; E4 a genuinely new terminal preparation advances only
     * through `RemovalGuard::refresh_authorization`.
     *
     * | Rule | durable marker | request | Effect |
     * |---|---|---|---|
     * | T7a | Removing/A | authorized T reconstruction | retain A / E1 |
     * | T7b | Removing/A | exact A/fpA->C | persist gate / E2 |
     * | T7c | Removing/A | lower, shorter, or foreign T | reject / E3 |
     * | T7d | Removing/A/fpA/C | A/different-fp->D | reject / E3 |
     * | T7e | Ready | ordinary T then T2 preparation | T then T2 / E4 |
     *
     * Constraint: `begin_terminal_takeover` is the effect-free reconstruction
     * owner. `refresh_authorization` is the explicit cross-operation takeover;
     * `authorize_disposal` is the sole physical-authorization mutation. */
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("prepared-reconstruction");
    let fingerprint =
        pc::SandboxRealizationFingerprint::from_spec(&spec("prepared-reconstruction"));
    let create =
        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 30).unwrap();
    let mut creation = begin(&root, &fingerprint, &create, None, None).unwrap();
    creation.prepare_root().unwrap();
    let evidence = creation.complete(&empty_receipt()).unwrap();
    let prepared_a = pc::SandboxEffectFence::new(
        "continuation-preparation-a",
        "owner",
        "runtime",
        1,
        u64::MAX - 20,
    )
    .unwrap();
    let (_, prepared) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared_a,
    )
    .unwrap()
    .expect("T7 durable continuation preparation");
    drop(prepared);
    let durable_a = read_marker(&root).unwrap().unwrap();
    assert_eq!(durable_a.effect_fence, prepared_a, "T7 setup A");

    let lower =
        pc::SandboxEffectFence::new("terminal-lower", "owner", "runtime", 0, u64::MAX).unwrap();
    let shorter =
        pc::SandboxEffectFence::new("terminal-shorter", "owner", "runtime", 1, u64::MAX - 21)
            .unwrap();
    let foreign = pc::SandboxEffectFence::new(
        "terminal-foreign",
        "foreign-owner",
        "foreign-runtime",
        1,
        u64::MAX,
    )
    .unwrap();
    for (label, rejected) in [
        ("lower", &lower),
        ("shorter", &shorter),
        ("foreign", &foreign),
    ] {
        assert!(
            begin_terminal_takeover(
                &root,
                &fingerprint,
                Some(source(&fingerprint, &evidence)),
                Some(&create),
                rejected,
            )
            .is_err(),
            "T7c {label}"
        );
        assert_eq!(
            read_marker(&root).unwrap().unwrap(),
            durable_a,
            "T7c {label}"
        );
        assert!(root.is_dir(), "T7c {label} zero root mutation");
    }

    let terminal_t = pc::SandboxEffectFence::new(
        "terminal-reconstruction-t",
        "owner",
        "runtime",
        1,
        u64::MAX - 10,
    )
    .unwrap();
    let (_, mut reconstructed) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &terminal_t,
    )
    .unwrap()
    .expect("T7a exact reconstruction");
    assert_eq!(
        reconstructed.marker.effect_fence, prepared_a,
        "T7a reconstruction must not rewrite A as T"
    );

    let preparation_fingerprint = "continuation-preparation-fingerprint-a";
    let authorization_c = disposal_authorization(
        &prepared_a,
        preparation_fingerprint,
        "successor-owner",
        "successor-runtime",
        2,
    );
    reconstructed
        .authorize_disposal(&authorization_c)
        .expect("T7b exact inherited A/fpA->C");
    let admitted = reconstructed.marker.clone();
    assert_eq!(admitted.effect_fence, prepared_a, "T7b immutable A");
    assert_eq!(
        admitted
            .disposal_authorization
            .as_ref()
            .expect("T7b physical gate")
            .effect_fence(),
        authorization_c.effect_fence(),
        "T7b latest C"
    );
    let different_fingerprint = disposal_authorization(
        &prepared_a,
        "different-preparation-fingerprint",
        "later-owner",
        "later-runtime",
        3,
    );
    assert!(
        reconstructed
            .authorize_disposal(&different_fingerprint)
            .is_err(),
        "T7d different fingerprint"
    );
    assert_eq!(reconstructed.marker, admitted, "T7d zero marker mutation");
    assert!(root.is_dir(), "T7d zero root mutation");
    drop(reconstructed);

    let ordinary_root = base.path().join("ordinary-terminal-preparation");
    let ordinary_fingerprint =
        pc::SandboxRealizationFingerprint::from_spec(&spec("ordinary-terminal-preparation"));
    let mut ordinary_creation =
        begin(&ordinary_root, &ordinary_fingerprint, &create, None, None).unwrap();
    ordinary_creation.prepare_root().unwrap();
    let ordinary_evidence = ordinary_creation.complete(&empty_receipt()).unwrap();
    let ordinary_t =
        pc::SandboxEffectFence::new("ordinary-terminal-t", "owner", "runtime", 1, u64::MAX - 20)
            .unwrap();
    let (_, ordinary) = begin_terminal_takeover(
        &ordinary_root,
        &ordinary_fingerprint,
        Some(source(&ordinary_fingerprint, &ordinary_evidence)),
        Some(&create),
        &ordinary_t,
    )
    .unwrap()
    .expect("T7e ordinary terminal participant");
    assert_eq!(ordinary.marker.effect_fence, ordinary_t, "T7e initial T");
    drop(ordinary);
    let ordinary_t2 =
        pc::SandboxEffectFence::new("ordinary-terminal-t2", "owner", "runtime", 1, u64::MAX - 10)
            .unwrap();
    let (_, mut ordinary_reconstruction) = begin_terminal_takeover(
        &ordinary_root,
        &ordinary_fingerprint,
        Some(source(&ordinary_fingerprint, &ordinary_evidence)),
        Some(&create),
        &ordinary_t2,
    )
    .unwrap()
    .expect("T7e ordinary reconstruction");
    assert_eq!(
        ordinary_reconstruction.marker.effect_fence, ordinary_t,
        "T7e reconstruction retains the last durable preparation"
    );
    ordinary_reconstruction
        .refresh_authorization(&ordinary_t2)
        .expect("T7e canonical provider preparation advances T");
    assert_eq!(
        ordinary_reconstruction.marker.effect_fence, ordinary_t2,
        "T7e only the preparation owner advances T"
    );
}

#[test]
fn terminal_successor_admission_is_expiry_monotonic() {
    /* Successor-admission table S1. Causes: C1 an exact Removing marker is
     * fenced at expiry E; C2 the incoming fence is an exact replay, a
     * same-lease successor with shorter/longer expiry, a higher epoch, or a
     * foreign same-epoch lease; C3 admission happens after marker reopen or on
     * the retained RemovalGuard. Effects: E1 exact/nondecreasing/higher-epoch
     * successors retain the one participant; E2 shorter-expiry and foreign
     * successors fail before marker/root mutation. The neutral
     * SandboxEffectFence::authorizes_successor predicate owns every row.
     *
     * | Rule | incoming relative to marker | edge | Effect |
     * |---|---|---|---|
     * | S1a | exact | reopen | E1 admit |
     * | S1b | same lease, shorter expiry | reopen | E2 reject |
     * | S1c | same lease, shorter expiry | retained guard | E2 reject |
     * | S1d | same lease, longer expiry | retained guard | E1 advance |
     * | S1e | higher epoch | reopen | E1 admit without rewrite |
     * | S1f | foreign same epoch | reopen | E2 reject |
     */
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("successor-expiry");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("successor-expiry"));
    let create =
        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 20).unwrap();
    let mut create_guard = begin(&root, &fingerprint, &create, None, None).unwrap();
    create_guard.prepare_root().unwrap();
    let evidence = create_guard.complete(&empty_receipt()).unwrap();
    let prepared =
        pc::SandboxEffectFence::new("prepare-a", "owner", "runtime", 1, u64::MAX - 10).unwrap();
    let shorter =
        pc::SandboxEffectFence::new("prepare-shorter", "owner", "runtime", 1, u64::MAX - 11)
            .unwrap();
    let longer =
        pc::SandboxEffectFence::new("prepare-longer", "owner", "runtime", 1, u64::MAX - 9).unwrap();

    let (_, removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared,
    )
    .unwrap()
    .expect("S1a exact participant");
    drop(removal);
    assert!(
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&create),
            &shorter,
        )
        .is_err(),
        "S1b/E2"
    );
    let (_, mut removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared,
    )
    .unwrap()
    .expect("S1c retained participant");
    assert!(removal.refresh_authorization(&shorter).is_err(), "S1c/E2");
    assert_eq!(removal.marker.effect_fence, prepared, "S1c zero mutation");
    removal
        .refresh_authorization(&longer)
        .expect("S1d/E1 nondecreasing renewal");
    drop(removal);

    let higher = pc::SandboxEffectFence::new(
        "prepare-higher",
        "owner-higher",
        "runtime-higher",
        2,
        u64::MAX - 20,
    )
    .unwrap();
    let (_, mut removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &higher,
    )
    .unwrap()
    .expect("S1e/E1 higher epoch");
    assert_eq!(
        removal.marker.effect_fence, longer,
        "S1e effect-free reopen does not reserve the higher epoch"
    );
    removal
        .refresh_authorization(&higher)
        .expect("S1e canonical preparation reserves the higher epoch");
    drop(removal);
    let foreign = pc::SandboxEffectFence::new(
        "prepare-foreign",
        "owner-foreign",
        "runtime-foreign",
        2,
        u64::MAX - 1,
    )
    .unwrap();
    assert!(
        begin_terminal_takeover(
            &root,
            &fingerprint,
            Some(source(&fingerprint, &evidence)),
            Some(&create),
            &foreign,
        )
        .is_err(),
        "S1f/E2"
    );
    assert!(root.is_dir(), "S1b/S1c/S1f zero root mutation");
}

#[test]
fn provider_preparation_returns_one_immutable_same_generation_predecessor() {
    // Provider preparation table P1. Causes: C1 durable marker is C; C2 input
    // is exact C, same-operation longer D, shorter B, foreign operation/owner,
    // higher epoch E, or late old C after E. Effects: E1 C/D return C without
    // marker mutation, closing both root-CAS response-loss orders; E2 B/foreign
    // reject without mutation; E3 higher epoch returns and persists E; E4 the
    // old generation cannot regain authority. The provisioning contract's
    // effect-scoped successor predicate is the only comparison authority.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("provider-predecessor");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("provider-predecessor"));
    let create =
        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 100).unwrap();
    let mut creation = begin(&root, &fingerprint, &create, None, None).unwrap();
    creation.prepare_root().unwrap();
    let evidence = creation.complete(&empty_receipt()).unwrap();
    let prepared_c =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 30).unwrap();
    let (_, mut removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared_c,
    )
    .unwrap()
    .expect("P1 durable C participant");
    let renewal_d =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 20).unwrap();
    assert_eq!(
        removal.prepare_disposal_effect(&prepared_c).unwrap(),
        prepared_c,
        "P1 exact replay/E1",
    );
    assert_eq!(
        removal.prepare_disposal_effect(&renewal_d).unwrap(),
        prepared_c,
        "P1 C/D replay/E1",
    );
    assert_eq!(
        removal.marker.effect_fence, prepared_c,
        "P1/E1 zero mutation"
    );

    let shorter_b =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 40).unwrap();
    let foreign =
        pc::SandboxEffectFence::new("foreign-terminal", "owner", "runtime", 1, u64::MAX - 10)
            .unwrap();
    assert!(
        removal.prepare_disposal_effect(&shorter_b).is_err(),
        "P1/E2"
    );
    assert!(removal.prepare_disposal_effect(&foreign).is_err(), "P1/E2");
    assert_eq!(
        removal.marker.effect_fence, prepared_c,
        "P1/E2 zero mutation"
    );

    let higher_e = pc::SandboxEffectFence::new(
        "terminal",
        "replacement-owner",
        "replacement-runtime",
        2,
        u64::MAX - 50,
    )
    .unwrap();
    assert_eq!(
        removal.prepare_disposal_effect(&higher_e).unwrap(),
        higher_e,
        "P1/E3",
    );
    assert_eq!(removal.marker.effect_fence, higher_e, "P1/E3 persisted");
    assert!(
        removal.prepare_disposal_effect(&prepared_c).is_err(),
        "P1/E4"
    );
    assert_eq!(removal.marker.effect_fence, higher_e, "P1/E4 zero mutation");
}

#[test]
fn physical_authorization_preserves_original_preparation_across_failover() {
    /* Filesystem authorization table F1. Causes: C1 the Removing marker has no
     * physical gate / exact A+fingerprint+latest B/C; C2 a caller presents exact
     * B replay, an aggregate-authorized higher-epoch C, late B after C, or a
     * foreign original preparation/fingerprint; C3 the provider response is
     * delivered/lost before the aggregate records physical completion. Effects:
     * E1 first B persists the typed gate without deleting the root; E2 B replay
     * after C3 is idempotent; E3 C advances only latest_successor while immutable
     * A/fingerprint survive marker restart; E4 late B/foreign reject before root
     * or secret mutation. One RealizationMarker/RemovalGuard owns every row.
     *
     * | Rule | durable gate | request | Effect |
     * |---|---|---|---|
     * | F1a | none | A→B | E1 persist |
     * | F1b | A/fp/B | A→B | E2 replay |
     * | F1c | A/fp/B | A→C | E3 advance |
     * | F1d | A/fp/C | A→B | E4 reject |
     * | F1e | A/fp/C | foreign A/fp | E4 reject |
     */
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("authorization-failover");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("authorization-failover"));
    let create = fence("create", 1);
    let mut create_guard = begin(&root, &fingerprint, &create, None, None).unwrap();
    create_guard.prepare_root().unwrap();
    let evidence = create_guard.complete(&empty_receipt()).unwrap();
    let prepared = fence("prepare-a", 1);
    let preparation_fingerprint = "preparation-fingerprint-a";
    let (_, mut first) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared,
    )
    .unwrap()
    .expect("F1a exact Removing participant");
    let authorization_b =
        disposal_authorization(&prepared, preparation_fingerprint, "owner", "runtime", 1);
    first.authorize_disposal(&authorization_b).expect("F1a");
    first
        .authorize_disposal(&authorization_b)
        .expect("F1b exact response-loss replay");
    drop(first);

    let authorization_c = disposal_authorization(
        &prepared,
        preparation_fingerprint,
        "owner-c",
        "runtime-c",
        2,
    );
    let (_, mut replay) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        authorization_c.effect_fence(),
    )
    .unwrap()
    .expect("F1c reopen exact participant after B response loss");
    replay
        .authorize_disposal(&authorization_c)
        .expect("F1c aggregate-authorized failover");
    let persisted = replay
        .marker
        .disposal_authorization
        .as_ref()
        .expect("F1c durable authorization gate");
    assert_eq!(
        persisted.prepared_effect_fence(),
        &prepared,
        "F1c immutable A"
    );
    assert_eq!(
        persisted.preparation_fingerprint(),
        preparation_fingerprint,
        "F1c immutable fingerprint"
    );
    assert_eq!(
        persisted.effect_fence(),
        authorization_c.effect_fence(),
        "F1c latest C"
    );
    assert!(
        replay.authorize_disposal(&authorization_b).is_err(),
        "F1d late B"
    );

    let foreign_prepared = pc::SandboxEffectFence::new(
        "prepare-foreign",
        "owner-foreign",
        "runtime-foreign",
        1,
        u64::MAX,
    )
    .unwrap();
    let foreign = disposal_authorization(
        &foreign_prepared,
        "preparation-fingerprint-foreign",
        "owner-foreign",
        "runtime-foreign",
        1,
    );
    assert!(
        replay.authorize_disposal(&foreign).is_err(),
        "F1e foreign preparation"
    );
    assert!(root.is_dir(), "F1d/F1e zero physical effect");
}

#[test]
fn disposal_authorization_projection_is_canonical_and_phase_bound() {
    /* Filesystem observation/decode table F2. Causes: C1 an exact Removing
     * marker has no physical-disposal authorization / canonical A+fp+B; C2 a
     * persisted authorization is canonical / has an unbound B operation; C3
     * the canonical authorization appears in Removing / an impossible Ready
     * phase. Effects: E1 preparation-only remains Terminal and permits no
     * physical claim; E2 the durable physical gate projects Disposing so Host
     * performs zero live source I/O; E3 malformed or phase-inconsistent marker
     * bytes fail closed as Incompatible. The one marker decoder owns E2/E3.
     *
     * | Rule | phase | authorization | Effect |
     * |---|---|---|---|
     * | F2a | Removing | none | E1 Terminal |
     * | F2b | Removing | canonical | E2 Disposing |
     * | F2c | Removing | unbound B | E3 Incompatible |
     * | F2d | Ready | canonical | E3 Incompatible |
     */
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("authorization-projection");
    let fingerprint =
        pc::SandboxRealizationFingerprint::from_spec(&spec("authorization-projection"));
    let create = fence("create", 1);
    let mut create_guard = begin(&root, &fingerprint, &create, None, None).unwrap();
    create_guard.prepare_root().unwrap();
    let evidence = create_guard.complete(&empty_receipt()).unwrap();
    let prepared = fence("prepare-a", 1);
    let (_, removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared,
    )
    .unwrap()
    .expect("F2a exact Removing participant");
    drop(removal);
    assert!(
        matches!(
            observe_adoption(
                &root,
                Some(&fingerprint),
                Some(evidence.effect_fence()),
                Some(evidence.physical_incarnation()),
                Some(&prepared),
            )
            .unwrap(),
            pc::SandboxObservation::Terminal { .. }
        ),
        "F2a/E1"
    );

    let authorization = disposal_authorization(
        &prepared,
        "preparation-fingerprint-a",
        "owner",
        "runtime",
        1,
    );
    let (_, mut removal) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        Some(&create),
        &prepared,
    )
    .unwrap()
    .expect("F2b exact Removing participant");
    removal.authorize_disposal(&authorization).unwrap();
    drop(removal);
    assert!(
        matches!(
            observe_adoption(
                &root,
                Some(&fingerprint),
                Some(evidence.effect_fence()),
                Some(evidence.physical_incarnation()),
                Some(authorization.effect_fence()),
            )
            .unwrap(),
            pc::SandboxObservation::Disposing { .. }
        ),
        "F2b/E2"
    );

    let marker_path = marker_path(&root).unwrap();
    let canonical = std::fs::read(&marker_path).unwrap();
    let mut unbound: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    unbound["disposal_authorization"]["effect_fence"]["operation_id"] =
        serde_json::json!("foreign-operation");
    std::fs::write(&marker_path, serde_json::to_vec(&unbound).unwrap()).unwrap();
    assert!(
        matches!(
            observe_adoption(
                &root,
                Some(&fingerprint),
                Some(evidence.effect_fence()),
                Some(evidence.physical_incarnation()),
                Some(authorization.effect_fence()),
            )
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "F2c/E3"
    );

    let mut wrong_phase: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    wrong_phase["phase"] = serde_json::json!("ready");
    std::fs::write(&marker_path, serde_json::to_vec(&wrong_phase).unwrap()).unwrap();
    assert!(
        matches!(
            observe_adoption(
                &root,
                Some(&fingerprint),
                Some(evidence.effect_fence()),
                Some(evidence.physical_incarnation()),
                Some(authorization.effect_fence()),
            )
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "F2d/E3"
    );
}

#[test]
fn ready_operation_admission_holds_one_exact_effect_boundary() {
    // Ready-operation cause/effect table: C1 marker phase Ready/non-Ready;
    // C2 root exact/substituted; C3 operation fence live+authorized/stale/
    // expired. R1 Ready+exact+live authorized admits under the lifecycle
    // lock and revalidates before the external effect; R2 stale/expired
    // rejects before any effect; R3 a root substitution after admission is
    // detected at the second boundary and the foreign root is untouched.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("operation");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("operation"));
    let create = fence("create", 2);
    let mut create_guard = begin(&root, &fingerprint, &create, None, None).unwrap();
    create_guard.prepare_root().unwrap();
    let evidence = create_guard.complete(&empty_receipt()).unwrap();

    let suspend = fence("suspend", 2);
    let operation = begin_ready_operation(&root, &evidence, &suspend).expect("R1");
    operation.validate_before_effect().expect("R1");
    drop(operation);
    assert!(
        begin_ready_operation(&root, &evidence, &fence("stale", 1)).is_err(),
        "R2"
    );
    let expired = pc::SandboxEffectFence::new("expired", "owner", "runtime", 2, 0).unwrap();
    assert!(
        begin_ready_operation(&root, &evidence, &expired).is_err(),
        "R2 expired successor is rejected at admission"
    );

    let operation = begin_ready_operation(&root, &evidence, &suspend).expect("R3 admit");
    // Retain the displaced inode so the filesystem cannot immediately reuse
    // its `(device, inode)` pair for the foreign replacement and make this ABA
    // oracle allocator-dependent.
    std::fs::rename(&root, base.path().join("operation-displaced")).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("foreign"), b"preserve").unwrap();
    assert!(operation.validate_before_effect().is_err(), "R3");
    assert_eq!(
        std::fs::read(root.join("foreign")).unwrap(),
        b"preserve",
        "R3"
    );
}

#[test]
fn checkpoint_participant_wal_closes_every_upload_crash_cut() {
    // Upload decision table: C1 marker Ready/terminal; C2 request fingerprint
    // and checkpoint operation are same/different; C3 snapshot digest+size
    // same/different; C4 crash before pending/after pending/after put/after
    // recorded receipt; C5 terminal
    // takeover happens before cleanup.
    //
    // | Rule | phase | operation/request | digest/size | durable participant | effect |
    // |---|---|---|---|---|---|
    // | U1 | Ready | same | same | absent | publish one pending participant |
    // | U2 | Ready/Removing | same | same | pending | replay without metadata drift |
    // | U3 | Ready/Removing | same | same | completed | return exact receipt |
    // | U4 | any | different | any | present | reject without replacement |
    // | U5 | any | same | different | present | reject without replacement |
    // Constraint: terminal takeover may continue the same participant under its
    // held RemovalGuard, but it cannot mint a second checkpoint authority.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkpoint");
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec("checkpoint"));
    let create = fence("create", 1);
    let mut creation = begin(&root, &fingerprint, &create, None, None).unwrap();
    creation.prepare_root().unwrap();
    let evidence = creation.complete(&empty_receipt()).unwrap();
    let suspend = fence("suspend", 1);

    let mut upload = begin_ready_operation(&root, &evidence, &suspend).unwrap();
    assert_eq!(
        upload.completed_checkpoint("request-a").unwrap(),
        None,
        "U1"
    );
    assert_eq!(
        upload
            .begin_checkpoint_upload("request-a", "digest-a", 7)
            .unwrap(),
        None,
        "U1"
    );
    drop(upload); // crash after pending, before/during put

    let wrong_suspend = fence("other-suspend", 1);
    let wrong_operation = begin_ready_operation(&root, &evidence, &wrong_suspend).unwrap();
    assert!(
        wrong_operation.completed_checkpoint("request-a").is_err(),
        "U2 operation identity conflict"
    );
    drop(wrong_operation);

    let terminal = fence("terminal", 1);
    let (_, mut replay) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        None,
        &terminal,
    )
    .unwrap()
    .expect("U2 Removing participant");
    replay.bind_checkpoint_expected(&suspend).unwrap();
    assert_eq!(
        replay
            .begin_checkpoint_upload("request-a", "digest-a", 7)
            .unwrap(),
        None,
        "U2 exact pending replay"
    );
    assert!(
        replay
            .begin_checkpoint_upload("request-a", "changed", 7)
            .is_err(),
        "U2 bytes conflict"
    );
    let reference = pc::SandboxCheckpointRef {
        id: "object-a".into(),
        format: "awaken-fs-tar-v1".into(),
        digest: "digest-a".into(),
        size_bytes: 7,
        created_at_unix_ms: 1,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: "environment".into(),
        base_image_fingerprint: "base".into(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: "suspend".into(),
    };
    replay
        .complete_checkpoint_upload("request-a", &reference)
        .unwrap();
    drop(replay); // crash after receipt WAL, before Session response

    assert!(
        begin_ready_operation(&root, &evidence, &suspend).is_err(),
        "U4"
    );
    let (_, mut replay) = begin_terminal_takeover(
        &root,
        &fingerprint,
        Some(source(&fingerprint, &evidence)),
        None,
        &terminal,
    )
    .unwrap()
    .expect("U3 response-loss participant");
    replay.bind_checkpoint_expected(&suspend).unwrap();
    assert_eq!(
        replay.completed_checkpoint("request-a").unwrap(),
        Some(reference),
        "U3"
    );
    assert!(
        replay.completed_checkpoint("request-b").is_err(),
        "U2 metadata"
    );
    replay
        .authorize_disposal(&disposal_authorization(
            &terminal,
            "checkpoint-terminal-preparation",
            "owner",
            "runtime",
            1,
        ))
        .expect("U3 aggregate-authorized physical authorization");
    replay.remove_root().unwrap();
    replay.finish().unwrap();
}

#[test]
fn legacy_is_marker_free_adoptable_and_never_mints_delete_evidence() {
    // Compatibility table: L1 one-shot create owns an in-memory inode and
    // may delete it; L2 marker-free V1 adoption observes/adopts but receives
    // no destructive evidence; L3 V1 against any V2 marker is incompatible;
    // L4 file/symlink roots are never classified Ready or removed.
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("legacy");
    let mut create = begin_legacy(&root).unwrap();
    create.prepare_root().unwrap();
    let live = create.complete().unwrap();
    assert_eq!(
        observe_adoption(&root, None, None, None, None).unwrap(),
        pc::SandboxObservation::Ready,
        "L1"
    );
    assert!(dispose_legacy(&root, None).is_err(), "L2 adopted V1");
    dispose_legacy(&root, Some(live.root_identity())).expect("L1 live delete");

    std::fs::write(&root, b"foreign").unwrap();
    assert!(
        matches!(
            observe_adoption(&root, None, None, None, None).unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "L4"
    );
    assert!(std::fs::read(&root).is_ok(), "L4 foreign file preserved");
}
