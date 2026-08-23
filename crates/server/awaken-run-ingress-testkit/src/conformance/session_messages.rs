// Session activity rotation and atomic continuation conformance rules. Included
// at crate root to preserve the public testkit API.
/// Coordinated reply activity cause/effect decision table.
///
/// C1 dispatch is Pending/Leased/Awaiting/Running; C2 it carries the exact
/// canonical Session affinity and either the expected prior activity coordinate
/// or no coordinate before its first Session continuation; C3 reply
/// epoch is nonzero; C4
/// durable evidence is absent/exact/conflicting (same correlation or reused
/// message id); C5 enqueue replay carries the
/// initial/a different activity epoch; C6 another execution-bearing field is
/// exact/changed. E1 reject without delivery or mutation; E2 atomically stage
/// one reply and rotate the row; E3 exact no-op; E4 preserve the rotated epoch;
/// E5 relay/claim exposes the rotated trusted request; E6 completion identity is
/// stable and prevents resurrection; E7 execution payload collisions reject;
/// E8 a resume staged after committed Awaiting but before queue settlement
/// survives that later settlement and becomes runnable; E9 a foreground Run
/// reconstructed as a Pending dispatch can stage its already-committed resume
/// before a Worker claim; E10 stable System context survives Outbox, Inbox, and
/// claim unchanged.
/// Constraints/invariants: exact Run, Thread, Session affinity, nonzero epochs,
/// payload identity, and transactional row ownership remain mandatory; only an
/// exact epochless row may adopt its first continuation coordinate.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
/// |---|---|---|---|---|---|---|---|
/// | AR0 | Pending | stale prior | Y | absent | - | exact | E1 |
/// | AR1U | Pending(epochless) | no prior | Y | absent | - | exact | E2+E9 |
/// | AR1 | Pending | exact | Y | absent | - | exact | E2+E9+E10 |
/// | AR2 | Awaiting | wrong | Y | absent | - | exact | E1 |
/// | AR3 | Awaiting | exact | N | absent | - | exact | E1 |
/// | AR4 | Awaiting | exact | Y | exact | - | exact | E3 |
/// | AR4L | Leased | exact | Y | absent | - | exact | E2+E8 |
/// | AR5 | Awaiting/Running | exact | Y | exact | - | exact | E3 |
/// | AR6 | Awaiting | exact | Y | conflict | - | exact | E1 |
/// | AR7 | Awaiting | exact | Y | exact | initial/different | exact | E4+E5 |
/// | AR8 | Done | exact | Y | consumed | initial | exact | E6 |
/// | AR9 | Awaiting/Done | exact | Y | - | - | changed | E7 |
pub async fn assert_session_reply_activity_rotation_conformance(store: &dyn Dispatch, ns: &str) {
    let parent = thread_id(ns, "activity-parent");
    let child = thread_id(ns, "activity-child");
    let initial_epoch = 11;
    let resumed_epoch = 12;

    // AR1U owns the epochless adoption partition used by foreground protocol
    // Runs: the exact Session affinity and row identity remain mandatory, while
    // the existing transaction installs the first continuation epoch and input.
    let unbound =
        dispatch(ns, "activity-unbound-run", "activity-unbound-child").for_session(parent.clone());
    store
        .enqueue(unbound.clone())
        .await
        .expect("AR1U enqueue epochless Session-affine dispatch");
    let unbound_input = PendingInput {
        message_id: format!("{ns}-activity-unbound-reply"),
        run_id: unbound.run_id().clone(),
        thread_id: unbound.thread_id().clone(),
        correlation_id: format!("{ns}-activity-unbound-correlation"),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("adopt exact foreground run".to_string()),
    };
    assert!(
        store
            .stage_session_resume(unbound_input.clone(), &parent, None, initial_epoch,)
            .await
            .expect("AR1U atomically adopt the epochless row"),
        "AR1U/E2+E9"
    );
    assert!(
        !store
            .stage_session_resume(unbound_input.clone(), &parent, None, initial_epoch,)
            .await
            .expect("AR1U exact adoption replay is idempotent"),
        "AR1U/E3"
    );
    let unbound_first_claim = store
        .claim_run(
            unbound.run_id(),
            "activity-unbound-owner",
            LEASE_MS,
            199_998,
            &Default::default(),
        )
        .await
        .expect("AR1U claim adopted foreground run")
        .expect("AR1U adopted foreground run is runnable");
    assert_eq!(
        unbound_first_claim.request.session_activity_epoch,
        Some(initial_epoch),
        "AR1U/E2 first continuation owns the adopted coordinate"
    );
    assert_eq!(
        store
            .settle(
                unbound.run_id(),
                unbound_first_claim.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("AR1U settle original attempt"),
        SettleOutcome::Applied,
        "AR1U/E9"
    );
    assert_eq!(store.relay().await.expect("AR1U relay"), 1, "AR1U/E2");
    let unbound_resumed = store
        .claim_run(
            unbound.run_id(),
            "activity-unbound-owner",
            LEASE_MS,
            199_999,
            &Default::default(),
        )
        .await
        .expect("AR1U claim adopted continuation")
        .expect("AR1U adopted continuation is runnable");
    assert_eq!(
        unbound_resumed.pending,
        vec![unbound_input.clone()],
        "AR1U/E2 exact continuation is delivered once"
    );
    assert_eq!(
        store
            .settle(
                unbound.run_id(),
                unbound_resumed.lease.epoch,
                DispatchOutcome::Done,
                &[unbound_input.message_id],
            )
            .await
            .expect("AR1U complete adopted continuation"),
        SettleOutcome::Applied,
        "AR1U cleanup preserves later rule isolation"
    );

    let initial = dispatch(ns, "activity-run", "activity-child")
        .for_session(parent.clone())
        .with_session_activity_epoch(initial_epoch);
    store
        .enqueue(initial.clone())
        .await
        .expect("AR1 enqueue coordinated child");

    let input = PendingInput {
        message_id: format!("{ns}-activity-reply"),
        run_id: initial.run_id().clone(),
        thread_id: child.clone(),
        correlation_id: format!("{ns}-activity-correlation"),
        available_at_ms: None,
        context_messages: vec![Message::text(
            MessageId(format!("{ns}-activity-system")),
            Role::System,
            "reply context",
        )],
        result: ResumeResult::Input("approved".to_string()),
    };
    assert!(
        store
            .stage_session_resume(
                input.clone(),
                &parent,
                Some(initial_epoch + 100),
                resumed_epoch,
            )
            .await
            .expect_err("AR0 stale prior activity rejects before staging")
            .to_string()
            .contains("expected prior"),
        "AR0/E1"
    );
    assert!(
        store
            .stage_session_resume(input.clone(), &parent, Some(initial_epoch), resumed_epoch)
            .await
            .expect("AR1 stage committed resume on reconstructed Pending row"),
        "AR1/E2+E9+E10"
    );

    // The activity coordinate is mutable queue state, not caller-owned Run
    // identity. A different replay value is accepted but cannot overwrite the
    // row that was admitted first.
    let different_initial_epoch = initial
        .clone()
        .with_session_activity_epoch(initial_epoch + 100);
    store
        .enqueue(different_initial_epoch)
        .await
        .expect("AR7 initial replay ignores mutable activity coordinate");
    let claimed = store
        .claim_run(
            initial.run_id(),
            "activity-owner",
            LEASE_MS,
            200_000,
            &Default::default(),
        )
        .await
        .expect("AR1 claim child")
        .expect("AR1 child is runnable");
    assert_eq!(
        claimed.request.session_activity_epoch,
        Some(resumed_epoch),
        "AR1+AR7 staged rotation survives enqueue replay"
    );
    assert_eq!(
        store
            .settle(
                initial.run_id(),
                claimed.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("AR1 settle Awaiting"),
        SettleOutcome::Applied
    );

    assert!(
        store
            .stage_session_resume(
                input.clone(),
                &thread_id(ns, "wrong-parent"),
                Some(initial_epoch),
                resumed_epoch,
            )
            .await
            .expect_err("AR2 wrong parent rejects")
            .to_string()
            .contains("Session affinity"),
        "AR2/E1"
    );
    assert!(
        store
            .stage_session_resume(input.clone(), &parent, Some(initial_epoch), 0)
            .await
            .expect_err("AR3 zero epoch rejects")
            .to_string()
            .contains("nonzero"),
        "AR3/E1"
    );

    let unrelated_thread = thread_id(ns, "activity-unrelated");
    let reused_id = PendingInput {
        message_id: input.message_id.clone(),
        run_id: RunId(format!("{ns}-activity-unrelated-run")),
        thread_id: unrelated_thread.clone(),
        correlation_id: format!("{ns}-activity-unrelated-correlation"),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Input("unrelated payload".to_string()),
    };
    assert!(
        store
            .append(reused_id)
            .await
            .expect("AR6 append unrelated reused id"),
        "AR6/E1 setup"
    );
    assert!(
        store
            .stage_session_resume(input.clone(), &parent, Some(initial_epoch), resumed_epoch)
            .await
            .expect_err("AR6 reused Inbox id rejects atomically")
            .to_string()
            .contains("reused"),
        "AR6/E1"
    );
    let unrelated = store
        .list(&unrelated_thread)
        .await
        .expect("AR6 list reused id evidence");
    assert_eq!(unrelated.len(), 1, "AR6 setup has one evidence row");
    assert_eq!(
        store
            .retract(&input.message_id, unrelated[0].revision)
            .await
            .expect("AR6 remove test evidence"),
        CasOutcome::Applied,
        "AR6 setup cleanup"
    );
    assert!(
        !store
            .stage_session_resume(input.clone(), &parent, Some(initial_epoch), resumed_epoch)
            .await
            .expect("AR4 exact Awaiting replay"),
        "AR4/E3"
    );

    let mut competing = input.clone();
    competing.message_id = format!("{ns}-activity-competing-reply");
    competing.result = ResumeResult::Input("denied".to_string());
    assert!(
        store
            .stage_session_resume(competing, &parent, Some(initial_epoch), resumed_epoch + 1,)
            .await
            .expect_err("AR6 competing reply rejects")
            .to_string()
            .contains("another durable reply"),
        "AR6/E1"
    );

    store
        .enqueue(initial.clone())
        .await
        .expect("AR7 original dispatch replays after activity rotation");
    let mut changed_execution = initial.clone();
    changed_execution
        .activation
        .snapshot
        .resolved_spec
        .instructions = "conflicting execution payload".to_string();
    assert!(
        store
            .enqueue(changed_execution.clone())
            .await
            .expect_err("AR9 live execution collision rejects")
            .to_string()
            .contains("reused"),
        "AR9/E7"
    );

    assert_eq!(store.relay().await.expect("AR7 relay"), 1, "AR7/E5");
    let resumed = store
        .claim_run(
            initial.run_id(),
            "activity-owner",
            LEASE_MS,
            200_001,
            &Default::default(),
        )
        .await
        .expect("AR7 claim resumed child")
        .expect("AR7 relayed reply makes child runnable");
    assert_eq!(
        resumed.request.session_activity_epoch,
        Some(resumed_epoch),
        "AR7/E4+E5"
    );
    assert_eq!(resumed.pending, vec![input.clone()], "AR7/E5");
    assert!(
        !store
            .stage_session_resume(input.clone(), &parent, Some(initial_epoch), resumed_epoch)
            .await
            .expect("AR5 exact replay remains valid while Running"),
        "AR5/E3"
    );
    assert_eq!(
        store
            .settle(
                initial.run_id(),
                resumed.lease.epoch,
                DispatchOutcome::Done,
                std::slice::from_ref(&input.message_id),
            )
            .await
            .expect("AR8 settle Done"),
        SettleOutcome::Applied,
        "AR8/E6"
    );

    let completion = store
        .completion_events_after(0, usize::MAX)
        .await
        .expect("AR8 completion evidence")
        .into_iter()
        .find(|completion| completion.run_id == *initial.run_id())
        .expect("AR8 child completion exists");
    assert_eq!(
        completion.request_fingerprint.as_deref(),
        Some(initial.canonical_fingerprint().as_str()),
        "AR8 completion identity is stable across activity rotation"
    );
    store
        .enqueue(initial.clone())
        .await
        .expect("AR8 completed exact replay is a no-op");
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("AR8 list after replay")
            .iter()
            .any(|summary| summary.run_id == *initial.run_id()),
        "AR8/E6 no resurrection"
    );
    assert!(
        store
            .enqueue(changed_execution)
            .await
            .expect_err("AR9 completed execution collision rejects")
            .to_string()
            .contains("reused"),
        "AR9/E7"
    );

    // AR4L models the real publish order: committed Thread truth already owns
    // BudgetReached while the dispatch row still carries the finishing lease.
    // Staging must rotate the new activity coordinate without waiting for (or
    // racing past) the Worker's subsequent Awaiting settlement.
    let race = dispatch(ns, "activity-race-run", "activity-race-child")
        .for_session(parent.clone())
        .with_session_activity_epoch(initial_epoch);
    store
        .enqueue(race.clone())
        .await
        .expect("AR4L enqueue coordinated child");
    let race_claim = store
        .claim_run(
            race.run_id(),
            "activity-race-owner",
            LEASE_MS,
            200_010,
            &Default::default(),
        )
        .await
        .expect("AR4L claim child")
        .expect("AR4L child is runnable");
    let race_input = PendingInput {
        message_id: format!("{ns}-activity-race-resume"),
        run_id: race.run_id().clone(),
        thread_id: race.thread_id().clone(),
        correlation_id: format!("{ns}-activity-race-correlation"),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: ResumeResult::Continue,
    };
    assert!(
        store
            .stage_session_resume(
                race_input.clone(),
                &parent,
                Some(initial_epoch),
                resumed_epoch + 1,
            )
            .await
            .expect("AR4L stage while finishing lease is current"),
        "AR4L/E2"
    );
    assert_eq!(
        store
            .settle(
                race.run_id(),
                race_claim.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("AR4L settle original attempt after staging"),
        SettleOutcome::Applied,
        "AR4L/E8"
    );
    assert_eq!(store.relay().await.expect("AR4L relay"), 1, "AR4L/E8");
    let race_resumed = store
        .claim_run(
            race.run_id(),
            "activity-race-owner",
            LEASE_MS,
            200_011,
            &Default::default(),
        )
        .await
        .expect("AR4L claim resumed child")
        .expect("AR4L staged resume is runnable after settlement");
    assert_eq!(
        race_resumed.request.session_activity_epoch,
        Some(resumed_epoch + 1),
        "AR4L/E2+E8"
    );
    assert_eq!(race_resumed.pending, vec![race_input], "AR4L/E8");
}

/// Atomic terminal-report continuation cause/effect decision table:
///
/// C1 the caller supplies one canonical input; C2 it is immediate, bound to the
/// exact fresh Run, and targets that Run's Thread; C3 Run identity is
/// absent/exact/conflicting;
/// C4 the prior input has already been consumed; C5 admission is
/// root/Session-child; C6 the child Thread is known/new/archived under trusted
/// disposition evidence; C7 exact evidence was staged/relayed by a legacy
/// caller before retry; C8 two messages admit two Runs on the same Thread.
/// E1 one pending input + one dispatch become visible in
/// the same transaction; E2 exact retry has no second effect; E3 direct reject
/// leaves neither input nor dispatch (legacy staged evidence remains owned by
/// ordinary relay); E4 do not resurrect consumed input; E5 enforce the same
/// distinct-unarchived-Thread bound as direct child admission; E6 each claim
/// receives only the input explicitly bound to that Run.
///
/// | Rule | C1/C7 | C2 | C3 | C4 | C5 | C6 | Effect |
/// |---|---|---|---|---|---|---|---|
/// | RC1 | direct | Y | absent | N | root | - | E1 |
/// | RC2 | direct | Y | exact | N | root | - | E2 |
/// | RC3 | direct | Y | exact | Y | root | - | E2+E4 |
/// | RC4 | staged | Y | conflict | - | root | - | E3, legacy evidence retained |
/// | RC5 | direct | wrong target | absent | - | root | - | E3 |
/// | RC6 | direct | scheduled | absent | - | root | - | E3 |
/// | RC7 | relayed | Y | absent | N | root | - | E1 repair enqueue |
/// | RC8 | direct | Y | absent | N | child | known at cap | E1+E5, no new slot |
/// | RC9 | direct | Y | absent | N | child | new at cap | E3+E5 |
/// | RC10 | direct | Y | absent | N | child | prior archived | E1+E5, slot released |
/// | RC11 | direct | Y | absent | N | child | target archived | E3+E5 |
/// | RC12 | direct | Y | absent | N | root | foreign affinity | E3, no cap bypass |
/// | RC13 | two direct | Y | absent | N | root | same Thread | E1+E6 |
/// | RC14 | direct | wrong Run | absent | N | root | - | E3, no cross-Run binding |
pub async fn assert_atomic_report_continuation_conformance(store: &dyn Dispatch, ns: &str) {
    let continuation_input =
        |message_id: String, request: &RunDispatch, content: &str| PendingInput {
            message_id,
            run_id: request.run_id().clone(),
            thread_id: request.thread_id().clone(),
            correlation_id: String::new(),
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Input(content.to_string()),
        };
    let primary = thread_id(ns, "report-primary");
    let message_id = format!("{ns}-terminal-report");
    // A Session primary has the established canonical self-affinity written by
    // SharedHost::resolved_dispatch; Root admission must preserve that shape.
    let mut root = dispatch(ns, "report-root", "report-primary").for_session(primary.clone());
    root.activation.input.clear();
    let report = continuation_input(message_id.clone(), &root, "child completed");
    store
        .relay_and_enqueue(report.clone(), root.clone(), ContinuationAdmission::Root)
        .await
        .expect("RC1 deliver report and enqueue root");
    let pending = store.list(&primary).await.expect("RC1 list pending report");
    assert_eq!(pending.len(), 1, "RC1 pending effect");
    assert_eq!(pending[0].input, report, "RC1 exact payload");
    assert!(
        store
            .list_dispatches()
            .await
            .expect("RC1 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *root.run_id()),
        "RC1 dispatch effect"
    );
    assert_eq!(store.relay().await.expect("RC1 outbox drained"), 0, "RC1");

    store
        .relay_and_enqueue(report.clone(), root.clone(), ContinuationAdmission::Root)
        .await
        .expect("RC2 exact command retry");
    assert_eq!(
        store.list(&primary).await.expect("RC2 list").len(),
        1,
        "RC2"
    );
    assert_eq!(store.relay().await.expect("RC2 outbox drained"), 0, "RC2");

    let claimed = store
        .claim_run(
            root.run_id(),
            "report-root-owner",
            LEASE_MS,
            100_000,
            &Default::default(),
        )
        .await
        .expect("RC3 claim report root")
        .expect("RC3 report root is runnable");
    assert_eq!(
        store
            .settle(
                root.run_id(),
                claimed.lease.epoch,
                DispatchOutcome::Done,
                std::slice::from_ref(&message_id),
            )
            .await
            .expect("RC3 settle consumed report"),
        SettleOutcome::Applied,
        "RC3"
    );
    assert!(store.list(&primary).await.expect("RC3 consumed").is_empty());
    store
        .relay_and_enqueue(report.clone(), root.clone(), ContinuationAdmission::Root)
        .await
        .expect("RC3 completed-Run replay");
    assert!(
        store
            .list(&primary)
            .await
            .expect("RC3 no resurrection")
            .is_empty(),
        "RC3/RC4"
    );
    assert_eq!(store.relay().await.expect("RC3 outbox drained"), 0, "RC3");

    let fallback_primary = thread_id(ns, "report-fallback-primary");
    let fallback_id = format!("{ns}-terminal-report-fallback");
    let mut fallback_root = dispatch(ns, "report-fallback-root", "report-fallback-primary");
    fallback_root.activation.input.clear();
    let fallback_report = continuation_input(
        fallback_id.clone(),
        &fallback_root,
        "child completed during generic relay",
    );
    assert!(
        store
            .stage(fallback_report.clone())
            .await
            .expect("RC7 stage")
    );
    assert_eq!(
        store.relay().await.expect("RC7 generic relay wins"),
        1,
        "RC7"
    );
    store
        .relay_and_enqueue(
            fallback_report,
            fallback_root.clone(),
            ContinuationAdmission::Root,
        )
        .await
        .expect("RC7 exact pending evidence repairs the fresh dispatch");
    assert_eq!(
        store
            .list(&fallback_primary)
            .await
            .expect("RC7 pending evidence")
            .len(),
        1,
        "RC7"
    );
    assert!(
        store
            .list_dispatches()
            .await
            .expect("RC7 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *fallback_root.run_id()),
        "RC7 repaired dispatch"
    );

    let conflict_id = format!("{ns}-terminal-report-conflict");
    let mut conflict_report = report.clone();
    conflict_report.message_id = conflict_id.clone();
    assert!(
        store
            .stage(conflict_report.clone())
            .await
            .expect("RC4 stage")
    );
    let mut conflicting_root = root.clone();
    conflicting_root
        .activation
        .snapshot
        .resolved_spec
        .instructions = "changed".to_string();
    assert!(
        store
            .relay_and_enqueue(
                conflict_report,
                conflicting_root,
                ContinuationAdmission::Root
            )
            .await
            .expect_err("RC4 canonical conflict")
            .to_string()
            .contains("reused"),
        "RC4"
    );

    let mismatch_id = format!("{ns}-terminal-report-mismatch");
    let mut mismatched_root = dispatch(ns, "report-mismatch-root", "another-primary");
    mismatched_root.activation.input.clear();
    let mismatch_report = PendingInput {
        message_id: mismatch_id.clone(),
        thread_id: primary.clone(),
        ..continuation_input(mismatch_id.clone(), &mismatched_root, "child completed")
    };
    assert!(
        store
            .stage(mismatch_report.clone())
            .await
            .expect("RC5 stage")
    );
    assert!(
        store
            .relay_and_enqueue(
                mismatch_report,
                mismatched_root.clone(),
                ContinuationAdmission::Root,
            )
            .await
            .expect_err("RC5 target mismatch")
            .to_string()
            .contains("different Threads"),
        "RC5"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC5 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *mismatched_root.run_id()),
        "RC5 no half dispatch"
    );

    let mut invalid = dispatch(ns, "report-invalid-root", "report-invalid-primary");
    invalid.activation.input.clear();
    let invalid_input = PendingInput {
        available_at_ms: Some(1),
        ..continuation_input(
            format!("{ns}-invalid-scheduled"),
            &invalid,
            "scheduled continuation is invalid",
        )
    };
    assert!(
        store
            .relay_and_enqueue(invalid_input, invalid.clone(), ContinuationAdmission::Root,)
            .await
            .expect_err("RC6 scheduled continuation payload")
            .to_string()
            .contains("immediate"),
        "RC6"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC6 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *invalid.run_id()),
        "RC6 no half dispatch"
    );
    assert_eq!(
        store
            .relay()
            .await
            .expect("RC4/RC5 rejected rows remain staged"),
        2,
        "RC4/RC5 preserve the outbox"
    );

    let child_parent = thread_id(ns, "continuation-parent");
    let child_thread = thread_id(ns, "continuation-child");
    let child_policy = || SessionChildAdmission::new(1, Vec::new());
    let child = dispatch(ns, "continuation-child-seed", "continuation-child")
        .for_session(child_parent.clone());
    store
        .enqueue_session_child(child, child_policy())
        .await
        .expect("RC8 seed the sole unarchived child slot");

    let follow_up_id = format!("{ns}-child-follow-up");
    let mut follow_up_run = dispatch(ns, "continuation-child-follow-up", "continuation-child")
        .for_session(child_parent.clone());
    follow_up_run.activation.input.clear();
    let follow_up = continuation_input(
        follow_up_id.clone(),
        &follow_up_run,
        "follow up on the existing child",
    );
    store
        .relay_and_enqueue(
            follow_up,
            follow_up_run.clone(),
            ContinuationAdmission::SessionChild(child_policy()),
        )
        .await
        .expect("RC8 a follow-up on the known child does not consume another slot");
    assert_eq!(
        store
            .list(&child_thread)
            .await
            .expect("RC8 list child input")
            .len(),
        1,
        "RC8 one canonical follow-up input"
    );

    let replacement_thread = thread_id(ns, "continuation-replacement");
    let replacement_id = format!("{ns}-child-replacement");
    let mut replacement_run = dispatch(
        ns,
        "continuation-replacement-run",
        "continuation-replacement",
    )
    .for_session(child_parent.clone());
    replacement_run.activation.input.clear();
    let replacement = continuation_input(
        replacement_id.clone(),
        &replacement_run,
        "start replacement child",
    );
    assert!(
        store
            .relay_and_enqueue(
                replacement.clone(),
                replacement_run.clone(),
                ContinuationAdmission::SessionChild(child_policy()),
            )
            .await
            .expect_err("RC9 a new child at capacity is rejected atomically")
            .to_string()
            .contains("maximum 1"),
        "RC9"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC9 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *replacement_run.run_id()),
        "RC9 rejected continuation leaves no half dispatch"
    );
    assert_eq!(
        store.relay().await.expect("RC9 no rejected outbox residue"),
        0,
        "RC9 direct rejection cannot leak a message to generic relay"
    );

    let archived_policy = SessionChildAdmission::new(1, vec![child_thread.clone()]);
    store
        .relay_and_enqueue(
            replacement,
            replacement_run,
            ContinuationAdmission::SessionChild(archived_policy.clone()),
        )
        .await
        .expect("RC10 committed archive evidence releases the prior child slot");
    assert_eq!(
        store
            .list(&replacement_thread)
            .await
            .expect("RC10 list replacement input")
            .len(),
        1,
        "RC10 replacement input and Run become visible together"
    );

    let archived_follow_up_id = format!("{ns}-archived-child-follow-up");
    let mut archived_follow_up_run =
        dispatch(ns, "continuation-archived-follow-up", "continuation-child")
            .for_session(child_parent);
    archived_follow_up_run.activation.input.clear();
    let archived_follow_up = continuation_input(
        archived_follow_up_id.clone(),
        &archived_follow_up_run,
        "must not revive archived child",
    );
    assert!(
        store
            .relay_and_enqueue(
                archived_follow_up,
                archived_follow_up_run.clone(),
                ContinuationAdmission::SessionChild(archived_policy),
            )
            .await
            .expect_err("RC11 archived child cannot be revived")
            .to_string()
            .contains("is archived"),
        "RC11"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC11 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *archived_follow_up_run.run_id()),
        "RC11 rejected continuation leaves no half dispatch"
    );

    let bypass_id = format!("{ns}-child-root-admission-bypass");
    let mut bypass_run = dispatch(ns, "continuation-bypass-run", "continuation-bypass")
        .for_session(thread_id(ns, "continuation-parent"));
    bypass_run.activation.input.clear();
    let bypass = continuation_input(
        bypass_id.clone(),
        &bypass_run,
        "must use bounded child admission",
    );
    assert!(
        store
            .relay_and_enqueue(bypass, bypass_run.clone(), ContinuationAdmission::Root)
            .await
            .expect_err("RC12 foreign affinity cannot use root admission")
            .to_string()
            .contains("requires SessionChild"),
        "RC12"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC12 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *bypass_run.run_id()),
        "RC12 root admission cannot bypass the child bound"
    );

    let exact_thread = thread_id(ns, "continuation-exact-binding");
    let mut first_run = dispatch(
        ns,
        "continuation-exact-binding-first",
        "continuation-exact-binding",
    );
    first_run.activation.input.clear();
    let mut second_run = dispatch(
        ns,
        "continuation-exact-binding-second",
        "continuation-exact-binding",
    );
    second_run.activation.input.clear();
    let first_input = continuation_input(
        format!("{ns}-continuation-exact-binding-first"),
        &first_run,
        "first",
    );
    let second_input = continuation_input(
        format!("{ns}-continuation-exact-binding-second"),
        &second_run,
        "second",
    );
    store
        .relay_and_enqueue(
            first_input.clone(),
            first_run.clone(),
            ContinuationAdmission::Root,
        )
        .await
        .expect("RC13 admit first exact continuation");
    store
        .relay_and_enqueue(
            second_input.clone(),
            second_run.clone(),
            ContinuationAdmission::Root,
        )
        .await
        .expect("RC13 admit second exact continuation");
    assert_eq!(
        store.list(&exact_thread).await.expect("RC13 list").len(),
        2,
        "RC13 both messages remain durable"
    );

    let first_claim = store
        .claim_run(
            first_run.run_id(),
            "continuation-exact-owner",
            LEASE_MS,
            200_000,
            &Default::default(),
        )
        .await
        .expect("RC13 claim first")
        .expect("RC13 first is runnable");
    assert_eq!(
        first_claim.pending,
        vec![first_input.clone()],
        "RC13/E6 the first Run receives only its bound message"
    );
    assert!(
        store
            .claim_run(
                second_run.run_id(),
                "continuation-exact-owner",
                LEASE_MS,
                200_000,
                &Default::default(),
            )
            .await
            .expect("RC13 inspect blocked second")
            .is_none(),
        "RC13 the same-Thread writer fence keeps the second Run queued"
    );
    assert_eq!(
        store
            .settle(
                first_run.run_id(),
                first_claim.lease.epoch,
                DispatchOutcome::Done,
                std::slice::from_ref(&first_input.message_id),
            )
            .await
            .expect("RC13 settle first"),
        SettleOutcome::Applied,
        "RC13 first settles independently"
    );
    let second_claim = store
        .claim_run(
            second_run.run_id(),
            "continuation-exact-owner",
            LEASE_MS,
            200_001,
            &Default::default(),
        )
        .await
        .expect("RC13 claim second")
        .expect("RC13 second is runnable after first settles");
    assert_eq!(
        second_claim.pending,
        vec![second_input.clone()],
        "RC13/E6 the second Run retains only its bound message"
    );
    assert_eq!(
        store
            .settle(
                second_run.run_id(),
                second_claim.lease.epoch,
                DispatchOutcome::Done,
                std::slice::from_ref(&second_input.message_id),
            )
            .await
            .expect("RC13 settle second"),
        SettleOutcome::Applied,
        "RC13 second settles independently"
    );
    assert!(
        store
            .list(&exact_thread)
            .await
            .expect("RC13 consumed")
            .is_empty(),
        "RC13 both exact messages are consumed by their own Runs"
    );

    let mut wrong_run = dispatch(
        ns,
        "continuation-wrong-binding",
        "continuation-wrong-binding",
    );
    wrong_run.activation.input.clear();
    let wrong_input = PendingInput {
        run_id: RunId(format!("{ns}-another-run")),
        ..continuation_input(
            format!("{ns}-continuation-wrong-binding"),
            &wrong_run,
            "wrong run",
        )
    };
    assert!(
        store
            .relay_and_enqueue(wrong_input, wrong_run.clone(), ContinuationAdmission::Root)
            .await
            .expect_err("RC14 reject cross-Run continuation binding")
            .to_string()
            .contains("bound to its fresh Run"),
        "RC14"
    );
    assert!(
        !store
            .list_dispatches()
            .await
            .expect("RC14 list dispatches")
            .iter()
            .any(|summary| summary.run_id == *wrong_run.run_id()),
        "RC14 a wrong binding leaves no half dispatch"
    );
}
