//! Independent state/authority decision tables for the controlled working root.
use super::*;
use crate::budget::SessionBudgetState;
use crate::state::{SessionActivityAdmissionPlan, SessionAggregate, SessionAggregateCommandError};
use awaken_lease_contract::fence::Epoch;

fn fresh_root(execution: SessionExecutionState, revision: u64) -> SessionAggregate {
    let fixture = mutation_tests::session("controlled", SessionRevision(revision));
    let crate::state::SessionBaselineState::Preparing(intent) = fixture.baseline else {
        unreachable!()
    };
    let compiled = intent.finalize().unwrap();
    let mut dto = PersistedSession::frozen_with_budget(
        "controlled",
        compiled.baseline,
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    dto.revision = SessionRevision(revision);
    dto.execution = execution;
    SessionAggregate::try_from(dto).unwrap()
}

fn root(execution: SessionExecutionState, revision: u64) -> SessionAggregate {
    // Existing phase/authority tables own the historical missing-field shape;
    // only the first-materialization table explicitly uses the fresh constructor.
    let mut wire = serde_json::to_value(fresh_root(execution, revision).snapshot()).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    wire.as_object_mut().unwrap().remove("run_activities");
    SessionAggregate::try_from(serde_json::from_value::<PersistedSession>(wire).unwrap()).unwrap()
}

#[test]
fn credential_adoption_requires_idle_or_a_proven_unattempted_preparation() {
    // C1 nonterminal Idle; C2 frozen Preparing; C3 explicit empty physical
    // history; C4 no realization owner/attempt; C5 MCP only Requested, unclaimed,
    // unattempted and unacknowledged; C6 a reserved Run/interval already exists.
    // R1=C1 -> allow. R2=C2+C3+C4+C5 with either C6 -> allow. R3 each physical
    // precondition independently false -> deny. R4 terminal/retired -> deny.
    // Archive still additionally requires no active activity or interval: its
    // old admission law is not widened by sharing the physical predicates.
    let mut initial = fresh_root(SessionExecutionState::Preparing, 0).into_snapshot();
    if let crate::state::SessionBaselineState::Frozen(baseline) = &mut initial.baseline {
        baseline.mutation_policy = crate::state::SessionMutationPolicy::Frozen;
    }
    initial.created_at_unix_ms = Some(1);
    let mcp = crate::state::SessionMcpAttachmentSet::from_initial(
        vec![
            awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                name: "application".into(),
                target: awaken_agent_contract::McpTarget::parse_http("https://example.test")
                    .unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
            },
        ],
        None,
    )
    .unwrap();
    initial.mcp = mcp;
    let lease = crate::state::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        epoch: 1,
        expires_at_unix_ms: 500,
    };
    for reserved in [false, true] {
        let mut aggregate = SessionAggregate::try_from(initial.clone()).unwrap();
        if reserved {
            assert_eq!(
                aggregate.activity().open(
                    None,
                    true,
                    false,
                    1,
                    0,
                    Some((
                        "operation",
                        &awaken_agent_contract::ThreadId("controlled".into()),
                        &awaken_agent_contract::RunId("reserved".into())
                    ))
                ),
                SessionActivityAdmissionPlan::Open(1)
            );
        }
        let candidate = aggregate.into_snapshot();
        assert!(
            crate::state::session_can_adopt_mcp_credential_lifecycle(&candidate),
            "R2 reserved={reserved}"
        );
        assert_eq!(
            crate::state::session_is_unattempted_profiled_preparation(&candidate),
            !reserved
        );
        let mut replaced = candidate.clone();
        replaced.mcp.request_full_replacement(vec![
            awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                name: "application".into(),
                target: awaken_agent_contract::McpTarget::parse_http("https://current.example.test").unwrap(),
                prompts_as_skills:false, credential:None,
                origin:awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
            }
        ], None).unwrap();
        assert!(
            crate::state::session_can_adopt_mcp_credential_lifecycle(&replaced),
            "R2 never-started retired history cannot block another credential refresh"
        );
        assert!(
            !crate::state::session_is_unattempted_profiled_preparation(&replaced),
            "archive retains its original all-Requested rule"
        );
        for cause in [
            "unknown",
            "owner",
            "attempt",
            "mcp_attempt",
            "mcp_active",
            "ack",
            "running",
            "terminal",
            "archived",
            "not_frozen",
            "no_creation",
        ] {
            let mut changed = candidate.clone();
            match cause {
                "unknown" => changed.environment_materialization = None,
                "owner" => changed.realization = Some(lease.clone()),
                "attempt" => changed.realization_progress.attempts = 1,
                "mcp_attempt" => changed.mcp.attachments[0].attempts = 1,
                "mcp_active" => {
                    changed.mcp.attachments[0].state =
                        awaken_session_configuration_contract::mcp::McpAttachmentState::Active
                }
                "ack" => changed.mcp.attachments[0].publication_acknowledged = true,
                "running" => changed.execution = SessionExecutionState::Running,
                "terminal" => changed.execution = SessionExecutionState::Terminated,
                "archived" => {
                    changed.disposition = crate::state::SessionDisposition::Archived {
                        archived_at: "1970-01-01T00:00:00Z".into(),
                        archived_at_unix_ms: Some(0),
                    };
                }
                "not_frozen" => {
                    let crate::state::SessionBaselineState::Frozen(baseline) =
                        &mut changed.baseline
                    else {
                        unreachable!()
                    };
                    baseline.mutation_policy = crate::state::SessionMutationPolicy::Managed;
                }
                "no_creation" => changed.created_at_unix_ms = None,
                _ => unreachable!(),
            }
            let before = changed.clone();
            assert!(
                !crate::state::session_can_adopt_mcp_credential_lifecycle(&changed),
                "R3/R4 {cause}"
            );
            assert_eq!(changed, before, "read-only query");
        }
    }
    let mut materializing = SessionAggregate::try_from(initial.clone()).unwrap();
    materializing
        .assignment()
        .claim(lease.clone(), false)
        .unwrap();
    let intent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        Some(lease),
    )
    .for_environment(
        materializing
            .frozen_baseline()
            .unwrap()
            .environment
            .config_fingerprint
            .0
            .clone(),
    );
    materializing
        .environment()
        .admit_environment_effect("workspace", &intent, 1)
        .unwrap();
    let mut materialized = materializing.into_snapshot();
    materialized.realization = None;
    materialized.realization_progress = Default::default();
    assert!(
        !crate::state::session_can_adopt_mcp_credential_lifecycle(&materialized),
        "R3 admitted effect survives pristine counters"
    );
    initial.execution = SessionExecutionState::Idle;
    assert!(
        crate::state::session_can_adopt_mcp_credential_lifecycle(&initial),
        "R1 existing Idle rule"
    );
}

#[test]
fn named_activity_admissions_retain_raw_identity_without_requiring_a_created_domain() {
    // Causes: N1 fresh, before first Create; N2 named operation/raw Thread/Run;
    // N3 same Run with a new operation/epoch; N4 anonymous low admission;
    // N5 invalid epoch/duplicate operation/blank raw identity. Effects: R1
    // N1+N2 appends once in the existing reducer without inventing a domain;
    // R2 N3 appends without rewriting N2; R3 N4 makes coverage sticky unknown;
    // R4 N5 rejects with the whole working root unchanged, including coverage.
    let thread = awaken_agent_contract::ThreadId("original-thread".into());
    let run = awaken_agent_contract::RunId("original-run".into());
    let mut aggregate = fresh_root(SessionExecutionState::Idle, 0);
    assert!(aggregate.environment_materialization().unwrap().is_empty());
    assert_eq!(aggregate.run_activities(), Some((&[][..], false)));
    // Q1 complete fresh prefix + no receipt/record => eligible Missing;
    // Q2 exact retained receipt => eligible ACK observation. Q3 each changed
    // operation/epoch/raw coordinate => error, whole root unchanged. Q4 legacy
    // omitted or sticky-unrecorded history => Unknown, while old optional replay
    // law remains unchanged and never repairs either historical shape.
    assert!(
        validate_complete_run_activity(aggregate.snapshot(), Some("fresh-op"), None, &thread, &run)
            .is_ok(),
        "Q1"
    );
    for (operation, epoch) in [("op-first", 1), ("op-continuation", 2)] {
        assert_eq!(
            aggregate.activity().open(
                Some(epoch),
                false,
                false,
                10,
                0,
                Some((operation, &thread, &run))
            ),
            SessionActivityAdmissionPlan::Open(epoch),
            "R1/R2"
        );
    }
    let (records, unrecorded) = aggregate.run_activities().unwrap();
    assert!(
        !unrecorded,
        "R1 before first Create is not an omitted admission"
    );
    assert_eq!(records.len(), 2);
    for (record, (operation, epoch)) in records
        .iter()
        .zip([("op-first", 1), ("op-continuation", 2)])
    {
        assert_eq!(record.operation_id(), operation);
        assert_eq!(record.activity_epoch().get(), epoch);
        assert_eq!(record.thread_id(), &thread);
        assert_eq!(record.run_id(), &run);
    }
    let before_query = aggregate.snapshot().clone();
    assert!(
        validate_complete_run_activity(
            aggregate.snapshot(),
            Some("op-first"),
            Some(1),
            &thread,
            &run
        )
        .is_ok(),
        "Q2 child"
    );
    assert!(
        validate_complete_run_activity(aggregate.snapshot(), None, Some(1), &thread, &run).is_ok(),
        "Q2 report"
    );
    let foreign_thread = awaken_agent_contract::ThreadId("foreign".into());
    let foreign_run = awaken_agent_contract::RunId("foreign".into());
    for (op, epoch, t, r) in [
        (Some("absent-op"), Some(1), &thread, &run),
        (Some("op-first"), None, &thread, &run),
        (Some("op-first"), Some(0), &thread, &run),
        (Some("op-first"), Some(2), &thread, &run),
        (Some("op-first"), Some(1), &foreign_thread, &run),
        (None, Some(1), &thread, &foreign_run),
    ] {
        assert!(
            validate_complete_run_activity(aggregate.snapshot(), op, epoch, t, r).is_err(),
            "Q3"
        );
        assert_eq!(
            aggregate.snapshot(),
            &before_query,
            "Q3 observation stutter"
        );
    }
    let empty_thread = awaken_agent_contract::ThreadId(" ".into());
    let empty_run = awaken_agent_contract::RunId(String::new());
    for (epoch, operation, source_thread, source_run) in [
        (0, "new", &thread, &run),
        (2, "new", &thread, &run),
        (3, "op-first", &thread, &run),
        (3, " ", &thread, &run),
        (3, "new", &empty_thread, &run),
        (3, "new", &thread, &empty_run),
    ] {
        let before = aggregate.clone();
        assert_eq!(
            aggregate.activity().open(
                Some(epoch),
                false,
                false,
                20,
                0,
                Some((operation, source_thread, source_run))
            ),
            SessionActivityAdmissionPlan::Conflict,
            "R4"
        );
        assert_eq!(aggregate, before, "R4 complete-state atomic rejection");
    }
    let mut dto = aggregate.into_snapshot();
    assert_eq!(
        dto.begin_activity_epoch(),
        Some(3),
        "R3 low anonymous kernel"
    );
    assert!(dto.run_activities().unwrap().1);
    assert!(dto.begin_activity_epoch_at(4, Some(("op-after-anonymous", &thread, &run))));
    assert!(
        dto.run_activities().unwrap().1,
        "R3 later named admission cannot clear unknown"
    );
    assert!(
        validate_complete_run_activity(&dto, Some("op-first"), Some(1), &thread, &run).is_err(),
        "Q4 sticky unknown"
    );
    assert!(
        dto.validate_run_activity_replay("op-first", 1, Some((&thread, &run)))
            .is_ok(),
        "Q4 unchanged optional replay"
    );
    let before = dto.clone();
    assert!(!dto.begin_activity_epoch_at(4, None));
    assert_eq!(
        dto, before,
        "R4 failed anonymous admission cannot mutate history"
    );
    let mut legacy = root(SessionExecutionState::Idle, 0).into_snapshot();
    assert!(legacy.begin_activity_epoch_at(1, Some(("legacy-op", &thread, &run))));
    assert!(
        validate_complete_run_activity(&legacy, Some("legacy-op"), Some(1), &thread, &run).is_err(),
        "Q4 no fabrication"
    );
    assert!(
        legacy
            .validate_run_activity_replay("legacy-op", 1, Some((&thread, &run)))
            .is_ok(),
        "Q4 old replay compatibility"
    );
    assert!(
        legacy.run_activities().is_none(),
        "legacy history is never backfilled"
    );
}

#[test]
fn retained_activity_history_import_and_replacement_reject_fabrication_and_rewrites() {
    // Causes: H1 omitted legacy vs explicit fresh-empty; H2 invalid epoch/order,
    // raw identity or duplicate operation; H3 latest named/Event-batch anchor;
    // H4 replacement removes/rewrites prefix, clears sticky unknown, or appends
    // without a genuinely new activity epoch. R1 H1 roundtrips exact old bytes;
    // R2 malformed H2 or absent H3 rejects import; R3 H4 rejects replacement;
    // R4 a new epoch may append while every old raw record remains immutable.
    let mut named = fresh_root(SessionExecutionState::Idle, 0);
    let thread = awaken_agent_contract::ThreadId("thread".into());
    let run = awaken_agent_contract::RunId("run".into());
    named
        .activity()
        .open(Some(1), false, false, 10, 0, Some(("op-1", &thread, &run)));
    let before = named.into_snapshot();
    let wire = serde_json::to_value(&before).unwrap();
    let restored: PersistedSession = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(
        SessionAggregate::try_from(restored).unwrap().snapshot(),
        &before,
        "R1 known"
    );
    let mut legacy_wire = wire.clone();
    legacy_wire
        .as_object_mut()
        .unwrap()
        .remove("run_activities");
    let legacy: PersistedSession = serde_json::from_value(legacy_wire.clone()).unwrap();
    assert!(legacy.run_activities().is_none());
    assert_eq!(
        serde_json::to_value(&legacy).unwrap(),
        legacy_wire,
        "R1 omitted bytes"
    );
    assert!(
        legacy.validate_observation_replacement(&before).is_err(),
        "R3 no legacy promotion"
    );
    assert!(
        before.validate_observation_replacement(&legacy).is_err(),
        "R3 no erasure"
    );
    let mut zero_epoch = wire.clone();
    *zero_epoch
        .pointer_mut("/run_activities/records/0/activity_epoch")
        .unwrap() = serde_json::json!(0);
    assert!(
        serde_json::from_value::<PersistedSession>(zero_epoch).is_err(),
        "R2 existing nonzero Epoch codec"
    );
    for (pointer, invalid) in [
        (
            "/run_activities/records/0/operation_id",
            serde_json::json!(" "),
        ),
        ("/run_activities/records/0/thread_id", serde_json::json!("")),
        ("/run_activities/records/0/run_id", serde_json::json!("\t")),
        ("/run_activities/records", serde_json::json!([])),
    ] {
        let mut malformed = wire.clone();
        *malformed.pointer_mut(pointer).unwrap() = invalid;
        let dto: PersistedSession = serde_json::from_value(malformed).unwrap();
        assert_eq!(
            SessionAggregate::try_from(dto).unwrap_err(),
            SessionMutationValidationError::ActivityHistoryMismatch,
            "R2 {pointer}"
        );
    }
    let mut after = before.clone();
    assert!(after.begin_activity_epoch_at(2, Some(("op-2", &thread, &run))));
    assert!(
        before.validate_observation_replacement(&after).is_ok(),
        "R4 append"
    );
    for invalid in [
        {
            let mut value = after.clone();
            value.run_activities.as_deref_mut().unwrap().records[0].run_id =
                awaken_agent_contract::RunId("rewritten".into());
            value
        },
        {
            let mut value = after.clone();
            value
                .run_activities
                .as_deref_mut()
                .unwrap()
                .records
                .remove(0);
            value
        },
        {
            let mut value = before.clone();
            value.run_activities.as_deref_mut().unwrap().unrecorded = true;
            value
        },
    ] {
        assert!(
            before.validate_observation_replacement(&invalid).is_err(),
            "R3 prefix/epoch"
        );
    }
    let mut duplicate = after.clone();
    duplicate.run_activities.as_deref_mut().unwrap().records[1].operation_id = "op-1".into();
    assert!(
        SessionAggregate::try_from(duplicate).is_err(),
        "R2 duplicate original operation"
    );
    let mut anonymous = before.clone();
    anonymous.begin_activity_epoch();
    let mut rewound = anonymous.clone();
    rewound.activity_epoch = before.activity_epoch;
    rewound.active_activity_epochs = before.active_activity_epochs.clone();
    assert!(
        anonymous
            .validate_observation_replacement(&rewound)
            .is_err(),
        "R3 no scalar rewind under sticky unknown"
    );
    let mut cleared = anonymous.clone();
    cleared.run_activities.as_deref_mut().unwrap().unrecorded = false;
    assert!(
        anonymous
            .validate_observation_replacement(&cleared)
            .is_err(),
        "R3 sticky"
    );
}

#[test]
fn activity_history_survives_settlement_archive_delete_and_terminal_failure() {
    // R1 named admission then settlement retains the raw fact; R2 archive,
    // delete or failed execution closes active membership but keeps the same
    // history. R3 terminal append is rejected even if a DTO invents a later
    // epoch. Neither an empty active set nor retained raw records proves domain
    // closure; these are preservation tests at the existing Root owner only.
    for terminal in 0..3 {
        let mut root = fresh_root(SessionExecutionState::Idle, 0);
        let thread = awaken_agent_contract::ThreadId("thread".into());
        let run = awaken_agent_contract::RunId("run".into());
        root.activity()
            .open(Some(1), false, false, 10, 0, Some(("op-1", &thread, &run)));
        let original = root.run_activities().unwrap().0.to_vec();
        root.activity().settle_activity_epoch(1);
        assert_eq!(root.run_activities().unwrap().0, original, "R1");
        match terminal {
            0 => {
                root.lifecycle().archive("time", 20).unwrap();
            }
            1 => {
                root.lifecycle().request_delete("workspace", 0, 20).unwrap();
            }
            _ => {
                root.lifecycle()
                    .transition_execution_at(SessionExecutionState::ActivationFailed, 20)
                    .unwrap();
            }
        }
        assert!(root.active_activity_epochs.is_empty());
        assert_eq!(
            root.run_activities().unwrap(),
            (original.as_slice(), false),
            "R2"
        );
        let before = root.clone();
        assert_eq!(
            root.activity()
                .open(Some(2), true, true, 30, 0, Some(("op-2", &thread, &run))),
            SessionActivityAdmissionPlan::Terminal
        );
        assert_eq!(root, before, "R3 aggregate gate");
        let mut invalid = root.snapshot().clone();
        invalid.begin_activity_epoch_at(2, Some(("op-2", &thread, &run)));
        assert!(
            root.validate_observation_replacement(&invalid).is_err(),
            "R3 replacement gate"
        );
    }
}

#[test]
fn first_materialization_is_write_ahead_immutable_and_never_backfilled() {
    // Cause graph: C1 fresh-present-empty/legacy-missing; C2 exact live lease;
    // C3 first/same effect/drift; C4 candidate renewal vs saved first expiry;
    // C5 exact receipt vs unadmitted/rewritten receipt. Effects: R1 fresh+C2
    // +first -> pending before I/O; R2 same+renewal -> unchanged original;
    // R3 matching receipt -> origin; R4 legacy -> no new admission/history;
    // R5 missing C2/drift/rewritten receipt -> no root mutation. Receipt
    // admission remains the old pure authority for genuine legacy outcomes.
    let mut fresh = fresh_root(SessionExecutionState::Preparing, 0);
    // N0 no-Create is explicit immutable history, independently of terminality:
    // fresh-empty -> true, historical missing -> false, admitted -> false.
    // Execution/Worker closure remains the consuming application's own gate.
    assert!(crate::state::session_has_no_create_evidence(
        fresh.snapshot()
    ));
    assert!(!crate::state::session_has_no_create_evidence(
        root(SessionExecutionState::Preparing, 0).snapshot()
    ));
    let fingerprint = fresh
        .frozen_baseline()
        .unwrap()
        .environment
        .config_fingerprint
        .0
        .clone();
    let lease = crate::state::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        epoch: 1,
        expires_at_unix_ms: 500,
    };
    let intent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        Some(lease.clone()),
    )
    .for_environment(fingerprint.clone());
    let before = fresh.clone();
    assert!(
        fresh
            .environment()
            .admit_environment_effect("workspace", &intent, 1)
            .is_err(),
        "R5 no current lease"
    );
    assert_eq!(fresh, before);
    fresh.assignment().claim(lease.clone(), false).unwrap();
    let absent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        None,
    )
    .for_environment(fingerprint.clone());
    assert!(
        fresh
            .environment()
            .admit_environment_effect("workspace", &absent, 1)
            .is_err(),
        "R5 no asserted lease"
    );
    let receipt = crate::state::SessionEnvironmentReceipt::from_intent(&intent, "binding").unwrap();
    let before = fresh.clone();
    assert!(
        fresh
            .environment()
            .try_apply_environment_receipt("workspace", &receipt, 1)
            .is_err(),
        "R5 fresh receipt without admission"
    );
    assert_eq!(fresh, before);
    // N1 known fresh-empty / N2 saved pending Create, crossed with Adopt and
    // reservation, and with new admission / late receipt. Every row must be
    // retryable Pending with identical Environment/fact/revision; a live lease
    // or self-consistent receipt is not evidence of the missing original Create.
    // N3 the exact first Create receipt remains the only pending->origin move.
    for pending in [false, true] {
        let mut subject = fresh.clone();
        if pending {
            subject
                .environment()
                .admit_environment_effect("workspace", &intent, 1)
                .unwrap();
        }
        assert_eq!(
            crate::state::session_has_no_create_evidence(subject.snapshot()),
            !pending,
            "N0 admitted Create is never mistaken for no-Create",
        );
        for kind in [
            crate::state::SessionEnvironmentEffectKind::Adopt,
            crate::state::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                transition_fingerprint: "pending".into(),
            },
        ] {
            let late = crate::state::SessionEnvironmentEffectIntent::new(
                "controlled",
                kind,
                Some(lease.clone()),
            )
            .for_environment(fingerprint.clone())
            .from_binding("binding");
            let before = subject.clone();
            assert_eq!(
                subject
                    .environment()
                    .admit_environment_effect("workspace", &late, 1),
                Err(crate::state::SessionEnvironmentReceiptError::Rejected(
                    crate::state::SessionEnvironmentRejection::MaterializationPending
                )),
                "N1/N2 admission pending={pending}"
            );
            assert_eq!(
                subject.environment().try_apply_environment_receipt(
                    "workspace",
                    &crate::state::SessionEnvironmentReceipt::from_intent(&late, "binding")
                        .unwrap(),
                    1
                ),
                Err(crate::state::SessionEnvironmentReceiptError::Rejected(
                    crate::state::SessionEnvironmentRejection::MaterializationPending
                )),
                "N1/N2 receipt pending={pending}"
            );
            assert_eq!(subject, before, "N1/N2 zero root mutation");
        }
    }
    let (changed, authorization) = fresh
        .environment()
        .admit_environment_effect("workspace", &intent, 1)
        .unwrap();
    assert!(changed, "R1 write-ahead fact");
    assert_eq!(authorization.intent(), &intent);
    assert_eq!(
        fresh
            .environment_materialization
            .as_deref()
            .unwrap()
            .pending(),
        Some(&intent)
    );
    assert!(
        fresh
            .environment_materialization
            .as_deref()
            .unwrap()
            .origin()
            .is_none()
    );
    let mut renewed = lease.clone();
    renewed.expires_at_unix_ms = 1_000;
    fresh.assignment().claim(renewed.clone(), false).unwrap();
    let renewed_intent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        Some(renewed),
    )
    .for_environment(fingerprint.clone());
    let before = fresh.clone();
    let (changed, authorization) = fresh
        .environment()
        .admit_environment_effect("workspace", &renewed_intent, 700)
        .unwrap();
    assert!(!changed, "R2 no second history write");
    assert_eq!(
        authorization.intent(),
        &intent,
        "R2 original expiry survived expiry/renewal"
    );
    assert_eq!(fresh, before);
    for axis in 0..4 {
        let mut drift = intent.realization().unwrap().clone();
        let mut fp = fingerprint.clone();
        match axis {
            0 => drift.owner = "foreign".into(),
            1 => drift.runtime_incarnation = "worker:2:boot".into(),
            2 => {
                drift.epoch = awaken_lease_contract::fence::Epoch::from_persisted(drift.epoch)
                    .and_then(awaken_lease_contract::fence::Epoch::next)
                    .unwrap()
                    .get()
            }
            _ => fp = "foreign-fingerprint".into(),
        }
        let proposed = crate::state::SessionEnvironmentEffectIntent::new(
            "controlled",
            crate::state::SessionEnvironmentEffectKind::Create,
            Some(drift),
        )
        .for_environment(fp);
        assert!(
            fresh
                .environment()
                .admit_environment_effect("workspace", &proposed, 700)
                .is_err(),
            "R5 drift axis {axis}"
        );
        assert_eq!(fresh, before);
    }
    let rewritten =
        crate::state::SessionEnvironmentReceipt::from_intent(&renewed_intent, "binding").unwrap();
    assert!(
        fresh
            .environment()
            .try_apply_environment_receipt("workspace", &rewritten, 700)
            .is_err(),
        "R5 no original-value rewrite"
    );
    assert_eq!(fresh, before);
    assert!(
        fresh
            .environment()
            .try_apply_environment_receipt("workspace", &receipt, 700)
            .unwrap(),
        "R3 matched completion"
    );
    let facts = fresh.environment_materialization.as_deref().unwrap();
    assert_eq!(facts.origin(), Some(&intent));
    assert!(facts.pending().is_none());
    // N4 completed origin admits ordinary adoption without changing first
    // history; N5 legacy None retains its old legal receipt join and stays None.
    let adopt = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Adopt,
        fresh.realization.clone(),
    )
    .for_environment(fingerprint.clone())
    .from_binding("binding");
    let origin = fresh.environment_materialization().unwrap().clone();
    fresh
        .environment()
        .admit_environment_effect("workspace", &adopt, 700)
        .unwrap();
    fresh
        .environment()
        .try_apply_environment_receipt(
            "workspace",
            &crate::state::SessionEnvironmentReceipt::from_intent(&adopt, "binding").unwrap(),
            700,
        )
        .unwrap();
    assert_eq!(
        fresh.environment_materialization(),
        Some(&origin),
        "N4 origin is immutable"
    );
    let source_generation = fresh.environment.generation().unwrap().id().to_owned();
    let rebuild = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Rebuild {
            source_generation_id: Some(source_generation),
        },
        fresh.realization.clone(),
    )
    .for_environment(fingerprint.clone())
    .from_binding("binding");
    let before = fresh.clone();
    assert_eq!(
        fresh
            .environment()
            .admit_environment_effect("workspace", &rebuild, 700),
        Err(crate::state::SessionEnvironmentReceiptError::Rejected(
            crate::state::SessionEnvironmentRejection::MaterializationPending
        )),
        "R7 full Rebuild remains closed"
    );
    assert_eq!(fresh, before);
    let mut legacy_adopt = root(SessionExecutionState::Preparing, 0);
    legacy_adopt
        .assignment()
        .claim(lease.clone(), false)
        .unwrap();
    let old_adopt = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Adopt,
        Some(lease.clone()),
    )
    .for_environment(fingerprint.clone())
    .from_binding("binding");
    legacy_adopt
        .environment()
        .try_apply_environment_receipt(
            "workspace",
            &crate::state::SessionEnvironmentReceipt::from_intent(&old_adopt, "binding").unwrap(),
            1,
        )
        .unwrap();
    assert!(
        legacy_adopt.environment_materialization().is_none(),
        "N5 no history backfill"
    );
    let mut legacy = root(SessionExecutionState::Preparing, 0);
    legacy.assignment().claim(lease, false).unwrap();
    let before = legacy.clone();
    assert_eq!(
        legacy
            .environment()
            .admit_environment_effect("workspace", &intent, 1),
        Err(crate::state::SessionEnvironmentReceiptError::MaterializationUnknown),
        "R4 original bad subject"
    );
    assert_eq!(legacy, before);
    assert!(
        legacy
            .environment()
            .try_apply_environment_receipt("workspace", &receipt, 1)
            .unwrap(),
        "R4 genuine legacy binding join"
    );
    assert!(
        legacy.environment_materialization.is_none(),
        "R4 no first history from receipt"
    );
}

#[test]
fn admitted_materialization_cannot_project_unattempted_archive_or_start_after_terminal_fence() {
    // Causes A1 explicit fresh-empty but no archive receipt; A2 actual saved
    // pending with no physical binding; A3 terminal root before first admission.
    // Effects: empty satisfies only the old pure preparation predicate, never
    // a no-Create witness; admitted history makes that predicate false even
    // when all old counter/lease tests are made pristine; terminal denies new
    // admission without changing the root. Legacy Fenced/Requested receipt
    // joins remain covered by the unchanged late-receipt owner table.
    let mut root = fresh_root(SessionExecutionState::Preparing, 0).into_snapshot();
    if let crate::state::SessionBaselineState::Frozen(baseline) = &mut root.baseline {
        baseline.mutation_policy =
            awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen;
    }
    root.created_at_unix_ms = Some(1);
    assert!(
        crate::state::session_is_unattempted_profiled_preparation(&root),
        "A1 state predicate only"
    );
    let mut root = SessionAggregate::try_from(root).unwrap();
    let lease = crate::state::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        epoch: 1,
        expires_at_unix_ms: 500,
    };
    root.assignment().claim(lease.clone(), false).unwrap();
    let intent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        Some(lease),
    )
    .for_environment(
        root.frozen_baseline()
            .unwrap()
            .environment
            .config_fingerprint
            .0
            .clone(),
    );
    let mut terminal = root.snapshot().clone();
    terminal.execution = SessionExecutionState::Terminated;
    let mut terminal = SessionAggregate::try_from(terminal).unwrap();
    let before = terminal.clone();
    for kind in [
        crate::state::SessionEnvironmentEffectKind::Create,
        crate::state::SessionEnvironmentEffectKind::Adopt,
        crate::state::SessionEnvironmentEffectKind::ResourceProjectionReservation {
            transition_fingerprint: "pending".into(),
        },
        crate::state::SessionEnvironmentEffectKind::Rebuild {
            source_generation_id: None,
        },
    ] {
        let mut proposed = crate::state::SessionEnvironmentEffectIntent::new(
            "controlled",
            kind.clone(),
            intent.realization().cloned(),
        )
        .for_environment(intent.environment_fingerprint().unwrap());
        if !matches!(kind, crate::state::SessionEnvironmentEffectKind::Create) {
            proposed = proposed.from_binding("binding");
        }
        assert_eq!(
            terminal
                .environment()
                .admit_environment_effect("workspace", &proposed, 1),
            Err(crate::state::SessionEnvironmentReceiptError::Rejected(
                crate::state::SessionEnvironmentRejection::Terminal
            )),
            "A3 all kinds"
        );
        assert_eq!(terminal, before);
    }
    root.environment()
        .admit_environment_effect("workspace", &intent, 1)
        .unwrap();
    let mut pristine_counters = root.into_snapshot();
    pristine_counters.realization = None;
    pristine_counters.realization_progress = Default::default();
    assert!(
        !crate::state::session_is_unattempted_profiled_preparation(&pristine_counters),
        "A2 admitted pending is not no-Create"
    );
}

#[test]
fn materialization_intrinsic_and_replacement_laws_preserve_unknown_and_first_value() {
    // Causes: V1 missing/null/present-empty serde; V2 valid original intent vs
    // foreign Session/kind/source/fingerprint; V3 current lease changes after
    // admission; V4 replacement reset/overwrite/promote. Effects: missing and
    // null remain Unknown/omitted; first facts validate independently of current
    // lease; malformed imports and fresh replacements cannot forge/reset history.
    // The two raw negative variants below deliberately model corrupt storage;
    // they are not admission fixtures and never authorize provider I/O.
    let legacy = root(SessionExecutionState::Preparing, 0).into_snapshot();
    let mut raw = serde_json::to_value(&legacy).unwrap();
    assert!(
        raw.get("environment_materialization").is_none(),
        "V1 missing omitted"
    );
    raw["environment_materialization"] = serde_json::Value::Null;
    let decoded: PersistedSession = serde_json::from_value(raw).unwrap();
    assert_eq!(decoded, legacy, "V1 null unknown");
    assert!(
        serde_json::to_value(&decoded)
            .unwrap()
            .get("environment_materialization")
            .is_none()
    );
    let mut fresh = fresh_root(SessionExecutionState::Preparing, 0);
    assert!(
        serde_json::to_value(fresh.snapshot())
            .unwrap()
            .get("environment_materialization")
            .is_some(),
        "V1 present-empty distinct"
    );
    assert!(
        legacy
            .validate_observation_replacement(fresh.snapshot())
            .is_err(),
        "V4 no unknown backfill"
    );
    let lease = crate::state::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker:1:boot".into(),
        epoch: 1,
        expires_at_unix_ms: 500,
    };
    fresh.assignment().claim(lease.clone(), false).unwrap();
    let fingerprint = fresh
        .frozen_baseline()
        .unwrap()
        .environment
        .config_fingerprint
        .0
        .clone();
    let intent = crate::state::SessionEnvironmentEffectIntent::new(
        "controlled",
        crate::state::SessionEnvironmentEffectKind::Create,
        Some(lease),
    )
    .for_environment(fingerprint.clone());
    let empty = fresh.snapshot().clone();
    fresh
        .environment()
        .admit_environment_effect("workspace", &intent, 1)
        .unwrap();
    empty
        .validate_observation_replacement(fresh.snapshot())
        .unwrap();
    assert!(
        fresh.validate_observation_replacement(&empty).is_err(),
        "V4 pending cannot reset"
    );
    let mut historic = fresh.snapshot().clone();
    historic.realization = Some(crate::state::SessionRealizationLease {
        owner: "replacement".into(),
        runtime_incarnation: "replacement:2:boot".into(),
        epoch: 9,
        expires_at_unix_ms: 0,
    });
    crate::state::SessionEnvironmentMaterialization::validate_for(&historic).unwrap();
    SessionAggregate::try_from(historic).unwrap();
    for bad in [
        crate::state::SessionEnvironmentEffectIntent::new(
            "foreign",
            crate::state::SessionEnvironmentEffectKind::Create,
            intent.realization().cloned(),
        )
        .for_environment(fingerprint.clone()),
        crate::state::SessionEnvironmentEffectIntent::new(
            "controlled",
            crate::state::SessionEnvironmentEffectKind::Adopt,
            intent.realization().cloned(),
        )
        .for_environment(fingerprint)
        .from_binding("binding"),
    ] {
        let mut raw = serde_json::to_value(fresh.snapshot()).unwrap();
        raw["environment_materialization"]["pending"] = serde_json::to_value(&bad).unwrap();
        let invalid: PersistedSession = serde_json::from_value(raw).unwrap();
        assert!(
            SessionAggregate::try_from(invalid).is_err(),
            "V2 intrinsic rejection"
        );
    }
    let mut overwritten = serde_json::to_value(fresh.snapshot()).unwrap();
    let mut other = intent.realization().unwrap().clone();
    other.expires_at_unix_ms += 1;
    overwritten["environment_materialization"]["pending"] = serde_json::to_value(
        crate::state::SessionEnvironmentEffectIntent::new(
            "controlled",
            crate::state::SessionEnvironmentEffectKind::Create,
            Some(other),
        )
        .for_environment(intent.environment_fingerprint().unwrap()),
    )
    .unwrap();
    let overwritten: PersistedSession = serde_json::from_value(overwritten).unwrap();
    crate::state::SessionEnvironmentMaterialization::validate_for(&overwritten).unwrap();
    assert!(
        fresh
            .validate_observation_replacement(&overwritten)
            .is_err(),
        "V4 full first value includes expiry"
    );
}

#[test]
fn terminal_cleanup_affinity_distinguishes_owner_and_runtime_incarnation() {
    // Cause graph: C1 logical owner exact; C2 Runtime incarnation exact;
    // C3 prior lease live; C4 physical Environment; C5 cleanup absent,
    // unfrozen, frozen root-only, frozen with child, or from another root;
    // C6 terminal execution. The original same-process continuation is C1&&C2; a
    // Worker restart is not that continuation merely because C1 still holds.
    //
    // | Rule | incumbent | C1+C2 | other causes | effect |
    // | A1 | absent | - | - | original first-placement Unchanged |
    // | A2 | present | both exact | valid cleanup scope | Unchanged |
    // | A3 | present | either changed | expired, terminal, physical, exact frozen scope | require predecessor exit |
    // | A4 | present | either changed | expired, Unmaterialized, exact frozen root-only | require archive receipt |
    // | A5 | present | either | invalid scope or remaining combinations | Retained |
    // All 320 present combinations include before/at/after expiry and u64::MAX.
    // This read-only query requests evidence; the application still owns
    // terminal admission, exact Registry/archive reads and the claim CAS.
    use crate::state::{
        SessionTerminalCleanupAffinity as Affinity, session_terminal_cleanup_affinity_for,
    };
    let initial = root(SessionExecutionState::Terminated, 4).into_snapshot();
    assert_eq!(
        session_terminal_cleanup_affinity_for(&initial, "worker-a", "worker-a:2:boot-new", 10),
        Affinity::Unchanged,
        "A1 no historical affinity; physical placement still has its application gate",
    );
    let mut checked = 0;
    for same_owner in [false, true] {
        for same_runtime in [false, true] {
            for (expires_at, live) in [(1, false), (10, false), (11, true), (u64::MAX, true)] {
                for (execution, terminal) in [
                    (SessionExecutionState::Idle, false),
                    (SessionExecutionState::Terminated, true),
                ] {
                    for physical in [false, true] {
                        for (scope, requested, frozen, child, exact_root) in [
                            ("absent", false, false, false, true),
                            ("unfrozen", true, false, false, true),
                            ("root", true, true, false, true),
                            ("child", true, true, true, true),
                            ("foreign", true, true, false, false),
                        ] {
                            let mut session = initial.clone();
                            session.execution = execution;
                            let cleanup_session = if exact_root {
                                session.session_id.clone()
                            } else {
                                crate::state::SessionId::from("foreign")
                            };
                            if requested {
                                assert!(session.terminal_cleanup.request(&cleanup_session));
                            }
                            if frozen {
                                session
                                    .terminal_cleanup
                                    .freeze_targets(
                                        &cleanup_session,
                                        if child {
                                            vec!["child".into()]
                                        } else {
                                            Vec::new()
                                        },
                                        0,
                                        0,
                                    )
                                    .unwrap();
                            }
                            session.realization = Some(crate::state::SessionRealizationLease {
                                owner: "worker-a".into(),
                                runtime_incarnation: "worker-a:1:boot-old".into(),
                                epoch: 4,
                                expires_at_unix_ms: expires_at,
                            });
                            if physical {
                                session.environment =
                                    crate::state::SessionEnvironmentState::Resident {
                                        binding: "original-binding".into(),
                                        effect_id: None,
                                        generation: None,
                                        idle_since_unix_ms: None,
                                    };
                            }
                            let before = session.clone();
                            if !exact_root {
                                assert!(
                                    session.verified_terminal_cleanup().is_err(),
                                    "foreign cleanup has no root authority"
                                );
                                let wire = serde_json::to_value(&session).unwrap();
                                let decoded: PersistedSession =
                                    serde_json::from_value(wire.clone()).unwrap();
                                assert_eq!(
                                    serde_json::to_value(&decoded).unwrap(),
                                    wire,
                                    "compatible DTO retains historical bytes"
                                );
                                assert!(
                                    matches!(
                                        SessionAggregate::try_from(decoded),
                                        Err(SessionMutationValidationError::TerminalCleanup(
                                            crate::state::SessionCleanupError::OperationMismatch
                                        ))
                                    ),
                                    "foreign cleanup cannot enter controlled root"
                                );
                            }
                            let expected =
                                match (same_owner && same_runtime, live, terminal, physical, scope)
                                {
                                    (_, _, _, _, "foreign") => Affinity::Retained,
                                    (true, _, _, _, _) => Affinity::Unchanged,
                                    (false, false, true, true, "root" | "child") => {
                                        Affinity::RequiresPredecessorExit
                                    }
                                    (false, false, _, false, "root") => {
                                        Affinity::RequiresUnattemptedArchiveReceipt
                                    }
                                    _ => Affinity::Retained,
                                };
                            checked += 1;
                            assert_eq!(
                                session_terminal_cleanup_affinity_for(
                                    &session,
                                    if same_owner { "worker-a" } else { "worker-b" },
                                    if same_runtime {
                                        "worker-a:1:boot-old"
                                    } else {
                                        "worker-a:2:boot-new"
                                    },
                                    10,
                                ),
                                expected,
                                "A2-A5 owner={same_owner} runtime={same_runtime} expires={expires_at} terminal={terminal} physical={physical} cleanup={scope}",
                            );
                            assert_eq!(session, before, "A1-A5 query never mutates root");
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 320, "the complete affinity decision table ran");
}

#[test]
fn compatible_snapshot_roundtrip_preserves_wire_and_unknown_observations() {
    let root = root(SessionExecutionState::Idle, 4);
    let original = serde_json::to_value(root.snapshot()).unwrap();
    assert!(original.get("status").is_some());
    assert!(original.get("execution").is_none());
    let dto: PersistedSession = serde_json::from_value(original.clone()).unwrap();
    let restored = SessionAggregate::try_from(dto).unwrap();
    assert_eq!(
        serde_json::to_value(restored.into_snapshot()).unwrap(),
        original
    );
    assert_eq!(root.created_at_unix_ms, None);
    assert_eq!(root.updated_at_unix_ms, None);
}

#[test]
fn activity_admission_matrix_changes_all_coordinates_or_none() {
    // C1 execution (7); C2 external preparation allowed; C3 exact epoch
    // absent/zero/stale/fresh; C4 retention terminal. Independent expected
    // outcomes protect phase precedence and rejected-command atomicity.
    for execution in [
        SessionExecutionState::Preparing,
        SessionExecutionState::Activating,
        SessionExecutionState::ActivationFailed,
        SessionExecutionState::Running,
        SessionExecutionState::Rescheduling,
        SessionExecutionState::Idle,
        SessionExecutionState::Terminated,
    ] {
        for external in [false, true] {
            for exact in [None, Some(0), Some(3), Some(5)] {
                let mut dto = root(execution, 4).into_snapshot();
                dto.activity_epoch = 3;
                let mut aggregate = SessionAggregate::try_from(dto).unwrap();
                let before = aggregate.clone();
                let actual = aggregate
                    .activity()
                    .open(exact, external, false, 10, 7, None);
                let ready = matches!(
                    execution,
                    SessionExecutionState::Running | SessionExecutionState::Idle
                ) || (external
                    && matches!(
                        execution,
                        SessionExecutionState::Preparing | SessionExecutionState::Activating
                    ));
                let expected = if execution.is_terminal() {
                    SessionActivityAdmissionPlan::Terminal
                } else if !ready {
                    SessionActivityAdmissionPlan::NotReady
                } else if exact.is_some_and(|epoch| epoch <= 3) {
                    SessionActivityAdmissionPlan::Conflict
                } else {
                    SessionActivityAdmissionPlan::Open(exact.unwrap_or(4))
                };
                assert_eq!(actual, expected, "{execution:?}/{external}/{exact:?}");
                if let SessionActivityAdmissionPlan::Open(epoch) = actual {
                    assert_eq!(aggregate.activity_epoch, epoch);
                    assert!(aggregate.active_activity_epochs.contains(&epoch));
                    assert_eq!(aggregate.revision, before.revision);
                    if execution == SessionExecutionState::Idle {
                        assert_eq!(aggregate.execution, SessionExecutionState::Running);
                    }
                    assert_eq!(
                        aggregate.running_interval.is_some(),
                        aggregate.execution == SessionExecutionState::Running
                    );
                } else {
                    assert_eq!(aggregate, before);
                }
            }
        }
    }
    let mut archived = root(SessionExecutionState::Idle, 4);
    archived.lifecycle().archive("archive", 10).unwrap();
    let before = archived.clone();
    assert_eq!(
        archived.activity().open(None, true, true, 20, 7, None),
        SessionActivityAdmissionPlan::Terminal
    );
    assert_eq!(archived, before);
}

#[test]
fn final_activity_closes_interval_and_compiles_one_intrinsic_fact() {
    // Causes: an active epoch, then the final settled epoch; required intrinsic
    // closing fact absent or already supplied. Active refuses without mutation;
    // final settlement emits the canonical event fact once, and supplying that
    // same fact leaves the outbox unchanged. Both application and aggregate use
    // the event contract's factory; the aggregate retains its revision filter.
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    assert_eq!(
        aggregate.activity().open(None, false, false, 10, 7, None),
        SessionActivityAdmissionPlan::Open(1)
    );
    let before = aggregate.clone();
    assert!(aggregate.activity().finish_running(20).is_err());
    assert_eq!(aggregate, before);
    assert_eq!(aggregate.activity().settle_activity_epoch(1), Some(true));
    let interval = aggregate.activity().finish_running(20).unwrap().unwrap();
    assert_eq!(aggregate.execution, SessionExecutionState::Idle);
    assert!(aggregate.running_interval.is_none());
    assert_eq!(interval.closed_revision, SessionRevision(5));
    let record = IdempotencyRecord {
        key: "close".into(),
        payload_hash: "same".into(),
    };
    let first = aggregate.compile_mutation("workspace", record.clone(), Vec::new(), 20);
    assert_eq!(first.expected_revision, SessionRevision(4));
    assert_eq!(first.lifecycle_facts.len(), 1);
    assert_eq!(first.lifecycle_facts[0].runtime_interval, Some(interval));
    assert_eq!(first.validate().unwrap(), SessionRevision(5));
    let second = aggregate.compile_mutation("workspace", record, first.lifecycle_facts.clone(), 5);
    assert_eq!(second.lifecycle_facts, first.lifecycle_facts);
    assert_eq!(aggregate.updated_at_unix_ms, Some(20));
    let before = aggregate.clone();
    assert!(
        aggregate
            .clone()
            .into_applied_snapshot(SessionRevision(6))
            .is_err()
    );
    assert_eq!(aggregate, before);
    assert_eq!(
        aggregate
            .into_applied_snapshot(SessionRevision(5))
            .unwrap()
            .revision,
        SessionRevision(5)
    );
}

#[test]
fn contradictory_interval_fact_is_rejected_at_fresh_repository_validation() {
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    aggregate.activity().open(None, false, false, 10, 0, None);
    aggregate.activity().settle_activity_epoch(1);
    let interval = aggregate.activity().finish_running(20).unwrap().unwrap();
    let wrong = ManagedLifecycleFact {
        id: interval.interval_id.clone(),
        object_id: "controlled".into(),
        workspace_id: Some("workspace".into()),
        event_type: "session.terminated".into(),
        timestamp: 0,
        runtime_interval: None,
    };
    let mutation = aggregate.compile_mutation(
        "workspace",
        IdempotencyRecord {
            key: "same".into(),
            payload_hash: "same".into(),
        },
        vec![wrong],
        20,
    );
    assert_eq!(
        mutation.validate(),
        Err(SessionMutationValidationError::DuplicateLifecycleFact)
    );
}

fn lease(epoch: Epoch, expiry: u64) -> crate::state::SessionRealizationLease {
    crate::state::SessionRealizationLease {
        owner: "owner".into(),
        runtime_incarnation: "incarnation".into(),
        epoch: epoch.get(),
        expires_at_unix_ms: expiry,
    }
}

#[test]
fn realization_generation_matrix_fences_renewal_and_compensation() {
    let mut aggregate = root(SessionExecutionState::Preparing, 4);
    aggregate
        .assignment()
        .claim(lease(Epoch::FIRST, 10), true)
        .unwrap();
    assert_eq!(aggregate.realization_progress.attempts, 1);
    let mut zero = lease(Epoch::FIRST, 20);
    zero.epoch = 0;
    for attempted in [
        zero,
        lease(Epoch::FIRST.next().unwrap().next().unwrap(), 20),
        lease(Epoch::FIRST, 5),
    ] {
        let before = aggregate.clone();
        assert!(aggregate.assignment().claim(attempted, false).is_err());
        assert_eq!(aggregate, before);
    }
    for mut renewal in [
        lease(Epoch::FIRST.next().unwrap(), 20),
        lease(Epoch::FIRST, 5),
        lease(Epoch::FIRST, 20),
    ] {
        renewal.owner = "other".into();
        let before = aggregate.clone();
        assert_eq!(
            aggregate.assignment().renew(renewal),
            Err(SessionAggregateCommandError::InvalidLeaseRenewal)
        );
        assert_eq!(aggregate, before);
    }
    aggregate
        .assignment()
        .renew(lease(Epoch::FIRST, 20))
        .unwrap();
    assert_eq!(aggregate.realization_progress.attempts, 1);
    aggregate
        .assignment()
        .claim(lease(Epoch::FIRST.next().unwrap(), 30), false)
        .unwrap();
    let before = aggregate.clone();
    assert!(
        aggregate
            .assignment()
            .restore_predecessor(&lease(Epoch::FIRST, 20), None)
            .is_err()
    );
    assert_eq!(aggregate, before);
    aggregate
        .assignment()
        .restore_predecessor(
            &lease(Epoch::FIRST.next().unwrap(), 30),
            Some(lease(Epoch::FIRST, 20)),
        )
        .unwrap();
    assert_eq!(aggregate.realization, Some(lease(Epoch::FIRST, 20)));
    aggregate.assignment().refund_capacity_attempt().unwrap();
    let before = aggregate.clone();
    assert_eq!(
        aggregate.assignment().refund_capacity_attempt(),
        Err(SessionAggregateCommandError::MissingCapacityAttempt)
    );
    assert_eq!(aggregate, before);
}

#[test]
fn rejected_event_progress_preserves_the_entire_root() {
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    let id =
        crate::event::session_event_batch_id(&aggregate.session_id, SessionRevision(5)).unwrap();
    let batch = crate::state::SessionEventBatch::compile(
        &aggregate.session_id,
        &id,
        vec![crate::state::SessionEventInput::UserMessage {
            content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "input",
            )],
        }],
    )
    .unwrap();
    let operation = batch.events[0].event.operation_id().to_string();
    let mut batch = batch;
    batch.admitted_revision = SessionRevision(5);
    aggregate.events().append_batch(batch.clone()).unwrap();
    let before = aggregate.clone();
    assert!(aggregate.events().append_batch(batch).is_err());
    assert_eq!(aggregate, before);
    let anchor = crate::state::SessionEventProjectionAnchor {
        source_commit_cursor: 9,
    };
    assert!(
        aggregate
            .events()
            .observe(
                &id,
                &operation,
                anchor,
                false,
                10,
                Some(("missing", "reply"))
            )
            .is_err()
    );
    assert_eq!(aggregate, before);
    aggregate
        .events()
        .observe(&id, &operation, anchor, false, 10, None)
        .unwrap();
    let before = aggregate.clone();
    assert!(
        aggregate
            .events()
            .observe(
                &id,
                &operation,
                crate::state::SessionEventProjectionAnchor {
                    source_commit_cursor: 10
                },
                false,
                10,
                None
            )
            .is_err()
    );
    assert_eq!(aggregate, before);
}

fn budget_snapshot() -> crate::budget::ManagedListPriceSnapshot {
    crate::budget::ManagedListPriceSnapshot {
        snapshot_id: "price".into(),
        version: 1,
        effective_at_unix_ms: 0,
        arithmetic_version: 1,
        model_rates: Default::default(),
        runtime_rates: Default::default(),
        fingerprint: "fingerprint".into(),
    }
}

#[test]
fn initial_configuration_identity_is_shared_by_recording_and_recovery_without_backfill() {
    // Identity table I1-I4: history Some/None x valid/foreign Agent/foreign
    // model/version0. Valid initial values import unchanged; all three corrupt
    // identities return typed UpdateHistoryMismatch even when history is None.
    // One-shot recording uses the SAME identity law but keeps fresh/once-only
    // admission: invalid recording stutters and a second recording rejects.
    // I5 legacy initial=None remains legal for either history shape; no import
    // fills historical absence. I6 legitimate tools/MCP changes retain the old
    // initial configuration, require one fact and survive the next CAS/re-stamp.
    let source = mutation_tests::session("controlled", SessionRevision(0));
    let crate::state::SessionBaselineState::Preparing(intent) = source.baseline else {
        unreachable!()
    };
    let configuration = awaken_session_configuration_contract::SessionAgentConfiguration::creation_presentation_seed(
        &intent.control, 1, None, (None, None), None, &Default::default(), Vec::new(),
    );
    for fault in ["valid", "agent", "model", "version"] {
        let mut value = configuration.clone();
        match fault {
            "agent" => value.agent_id = "foreign".into(),
            "model" => value.model.model_id = "foreign".into(),
            "version" => value.version = 0,
            _ => {}
        }
        let mut recording = fresh_root(SessionExecutionState::Idle, 0);
        let before = recording.clone();
        let recorded = recording.record_initial_agent_configuration(Box::new(value.clone()));
        if fault == "valid" {
            assert_eq!(recorded, Ok(()), "I1 normal first recording");
            let once = recording.clone();
            assert_eq!(
                recording.record_initial_agent_configuration(Box::new(value.clone())),
                Err(SessionAggregateCommandError::InvalidInitialAgentConfiguration)
            );
            assert_eq!(recording, once, "I1 once-only stutter");
        } else {
            assert_eq!(
                recorded,
                Err(SessionAggregateCommandError::InvalidInitialAgentConfiguration)
            );
            assert_eq!(recording, before, "I2-I4 recording refusal stutter");
        }
        for history_present in [false, true] {
            let mut wire = serde_json::to_value(before.snapshot()).unwrap();
            wire["initial_agent_configuration"] = serde_json::to_value(&value).unwrap();
            if !history_present {
                wire.as_object_mut().unwrap().remove("update_history");
            }
            let snapshot: PersistedSession = serde_json::from_value(wire).unwrap();
            let imported = SessionAggregate::try_from(snapshot.clone());
            if fault == "valid" {
                assert_eq!(
                    imported.unwrap().snapshot(),
                    &snapshot,
                    "I1 history={history_present}"
                );
            } else {
                assert_eq!(
                    imported.unwrap_err(),
                    SessionMutationValidationError::UpdateHistoryMismatch,
                    "I2-I4 fault={fault} history={history_present}"
                );
            }
        }
    }
    for history_present in [false, true] {
        let mut snapshot = fresh_root(SessionExecutionState::Idle, 0).into_snapshot();
        if !history_present {
            snapshot.update_history = None;
        }
        assert!(snapshot.initial_agent_configuration.is_none());
        assert_eq!(
            SessionAggregate::try_from(snapshot.clone())
                .unwrap()
                .snapshot(),
            &snapshot,
            "I5 no backfill"
        );
    }
    // I7/I8: a retained frozen publication pin is a required identity axis:
    // matching version passes, another positive version refuses both initial
    // recording and persisted import with the whole candidate unchanged.
    // Without that pin, positive presentation versions remain legal; no
    // historical publication revision is invented. Cross both history shapes.
    for pin in [None, Some(1)] {
        for version in [1, 2] {
            let mut snapshot = fresh_root(SessionExecutionState::Idle, 0).into_snapshot();
            let crate::state::SessionBaselineState::Frozen(baseline) = &mut snapshot.baseline
            else {
                unreachable!("fresh fixture is frozen")
            };
            baseline.agent_revision = pin;
            let mut recording = SessionAggregate::try_from(snapshot.clone()).unwrap();
            let mut value = configuration.clone();
            value.version = version;
            let allowed = pin.is_none_or(|pin| pin == version);
            let recorded = recording.record_initial_agent_configuration(Box::new(value.clone()));
            if allowed {
                assert_eq!(recorded, Ok(()), "I7/I8 pin={pin:?} version={version}");
            } else {
                assert_eq!(
                    recorded,
                    Err(SessionAggregateCommandError::InvalidInitialAgentConfiguration)
                );
                assert_eq!(recording.snapshot(), &snapshot, "I7 refusal stutter");
            }
            for history_present in [false, true] {
                let mut wire = serde_json::to_value(&snapshot).unwrap();
                wire["initial_agent_configuration"] = serde_json::to_value(&value).unwrap();
                if !history_present {
                    wire.as_object_mut().unwrap().remove("update_history");
                }
                let imported = SessionAggregate::try_from(
                    serde_json::from_value::<PersistedSession>(wire).unwrap(),
                );
                if allowed {
                    assert!(imported.is_ok(), "I7/I8 history={history_present}");
                } else {
                    assert_eq!(
                        imported.unwrap_err(),
                        SessionMutationValidationError::UpdateHistoryMismatch
                    );
                }
            }
        }
    }
    for field in ["tools", "mcp"] {
        let mut fresh = fresh_root(SessionExecutionState::Idle, 0);
        fresh
            .record_initial_agent_configuration(Box::new(configuration.clone()))
            .unwrap();
        let mut before = fresh.into_snapshot();
        before.revision = SessionRevision(7);
        let initial = before.initial_agent_configuration.clone();
        let mut next = SessionAggregate::try_from(before.clone()).unwrap();
        if field == "tools" {
            next.configuration()
                .unwrap()
                .set_tools(crate::state::SessionToolConfiguration {
                    toolsets: Vec::new(),
                    client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                        name: "accepted-tool".into(),
                        description: "New authoring".into(),
                        input_schema: serde_json::json!({"type":"object"}),
                    }],
                })
                .unwrap();
        } else {
            next.mcp().request_full_replacement(vec![
                awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                    name: "accepted-mcp".into(), target: awaken_agent_contract::McpTarget::parse_http("https://identity.example.test/mcp").unwrap(),
                    prompts_as_skills: false, credential: None,
                    origin: awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
                }
            ], None).unwrap();
        }
        next.record_configuration_update(
            &before,
            field == "mcp",
            crate::state::SessionEventProjectionAnchor {
                source_commit_cursor: 0,
            },
        )
        .unwrap();
        for now in [100, 200] {
            next.compile_mutation(
                "owner",
                crate::state::IdempotencyRecord {
                    key: "identity-update".into(),
                    payload_hash: "command".into(),
                },
                Vec::new(),
                now,
            );
            assert_eq!(next.update_history.as_deref().unwrap().facts.len(), 1);
            assert_eq!(
                next.update_history.as_deref().unwrap().facts[0].recorded_at_unix_ms,
                Some(now),
                "I6 original re-stamp"
            );
        }
        assert_eq!(
            before.validate_observation_replacement(next.snapshot()),
            Ok(())
        );
        let committed = next.into_applied_snapshot(SessionRevision(8)).unwrap();
        assert_eq!(
            committed.initial_agent_configuration, initial,
            "I6 immutable initial values"
        );
        assert_eq!(
            crate::state::SessionUpdateHistory::validate_for(&committed),
            Ok(())
        );
    }
}

#[test]
fn configuration_changes_require_one_retained_update_fact() {
    // Cause/effect table HC1-HC2: C1 history is fresh-known or legacy-absent;
    // C2 exactly one title/metadata/tools/MCP-desired/budget-limit field changes;
    // C3 the original aggregate records that update or it is omitted. HC1
    // C1+C2+omitted -> UpdateHistoryMismatch before a fresh CAS; HC2 recorded
    // -> one exact next-revision fact. These 2x5x2 rows independently require
    // history for accepted configuration, without manufacturing legacy facts.
    // Runtime phase/accounting changes are the separate HC3-HC4 partitions.
    use awaken_agent_contract::ClientToolDescriptor;
    use awaken_session_configuration_contract::mcp::{McpAttachmentDraft, McpAttachmentOrigin};
    for legacy in [false, true] {
        for field in ["title", "metadata", "tools", "mcp", "budget"] {
            let mut before = fresh_root(SessionExecutionState::Idle, 7).into_snapshot();
            before.budget = SessionBudgetState::active(10, budget_snapshot());
            if legacy {
                before.update_history = None;
            }
            for recorded in [false, true] {
                let mut next = SessionAggregate::try_from(before.clone()).unwrap();
                match field {
                    "title" => next
                        .configuration()
                        .unwrap()
                        .set_title(Some("new title".into())),
                    "metadata" => next.configuration().unwrap().patch_metadata(
                        &std::collections::BTreeMap::from([("key".into(), Some("value".into()))]),
                    ),
                    "tools" => next
                        .configuration()
                        .unwrap()
                        .set_tools(crate::state::SessionToolConfiguration {
                            toolsets: Vec::new(),
                            client_tools: vec![ClientToolDescriptor {
                                name: "client-tool".into(),
                                description: "accepted tool".into(),
                                input_schema: serde_json::json!({"type":"object"}),
                            }],
                        })
                        .unwrap(),
                    "mcp" => {
                        next.mcp()
                            .request_full_replacement(
                                vec![McpAttachmentDraft {
                                    name: "docs".into(),
                                    target: awaken_agent_contract::McpTarget::parse_http(
                                        "https://history.example.test/mcp",
                                    )
                                    .unwrap(),
                                    prompts_as_skills: false,
                                    origin: McpAttachmentOrigin::Session,
                                    credential: None,
                                }],
                                None,
                            )
                            .unwrap();
                    }
                    _ => next.budget().update_limit(Some(20)).unwrap(),
                }
                if recorded {
                    next.record_configuration_update(
                        &before,
                        field == "mcp",
                        crate::event::SessionEventProjectionAnchor {
                            source_commit_cursor: 0,
                        },
                    )
                    .unwrap();
                }
                next.compile_mutation(
                    "owner",
                    crate::state::IdempotencyRecord {
                        key: "configuration-update".into(),
                        payload_hash: "configuration-command".into(),
                    },
                    Vec::new(),
                    100,
                );
                let actual = before.validate_observation_replacement(next.snapshot());
                assert_eq!(
                    actual,
                    if recorded {
                        Ok(())
                    } else {
                        Err(crate::state::SessionMutationValidationError::UpdateHistoryMismatch)
                    },
                    "HC1/HC2 legacy={legacy} field={field} recorded={recorded}"
                );
                if recorded {
                    let history =
                        crate::state::SessionUpdateHistory::for_session(next.snapshot()).unwrap();
                    assert_eq!(history.facts.len(), 1, "HC2 exactly one fact");
                    assert_eq!(history.facts[0].committed_revision, SessionRevision(8));
                    assert_eq!(
                        history.known_after_revision,
                        SessionRevision(if legacy { 7 } else { 0 })
                    );
                }
            }
        }
    }
}

#[test]
fn mcp_runtime_progress_and_retry_do_not_require_an_authoring_fact() {
    // HC3: same desired fingerprint, original Requested -> Realizing -> Failed
    // and retry generation progression changes physical recovery state only.
    // No history append is required, including a legacy-absent history. HC4
    // changed desired configuration is covered by HC1 and cannot use this rule.
    // Use the actual MCP aggregate methods; no fake runtime receipt or sorter.
    use awaken_session_configuration_contract::mcp::{
        McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim,
    };
    let draft = McpAttachmentDraft {
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://history.example.test/mcp")
            .unwrap(),
        prompts_as_skills: false,
        origin: McpAttachmentOrigin::Session,
        credential: None,
    };
    for legacy in [false, true] {
        let mut before = fresh_root(SessionExecutionState::Idle, 7).into_snapshot();
        before.mcp =
            crate::state::SessionMcpAttachmentSet::from_initial(vec![draft.clone()], None).unwrap();
        if legacy {
            before.update_history = None;
        }
        let generation = before.mcp.attachments[0].generation.clone();
        let mut next = SessionAggregate::try_from(before.clone()).unwrap();
        next.mcp()
            .claim_realization(
                &generation,
                McpRealizationClaim {
                    realization_id: "original-realization".into(),
                    runtime_incarnation: "worker/boot".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: 30_000,
                    stage_idempotency_key: "original-stage".into(),
                },
            )
            .unwrap();
        next.mcp()
            .fail_realization(&generation, "original-realization", "transport unavailable")
            .unwrap();
        next.mcp()
            .request_full_replacement(vec![draft.clone()], None)
            .unwrap();
        next.compile_mutation(
            "owner",
            crate::state::IdempotencyRecord {
                key: "mcp-recovery".into(),
                payload_hash: "original-retry".into(),
            },
            Vec::new(),
            100,
        );
        assert_eq!(
            before.validate_observation_replacement(next.snapshot()),
            Ok(()),
            "HC3 legacy={legacy}"
        );
        assert_eq!(next.mcp.desired_fingerprint, before.mcp.desired_fingerprint);
        assert_eq!(
            next.update_history, before.update_history,
            "HC3 no invented authoring fact"
        );
        assert_eq!(
            next.mcp.attachments.last().unwrap().generation.version(),
            2,
            "HC3 actual retry generation"
        );
    }
}

#[test]
fn budget_reconciliation_rejects_rewinds_and_missing_prices_atomically() {
    // B5: a rewind or absent exact price leaves both original root cursors and budget unchanged.
    use crate::budget::{ManagedBudgetUsageCursor, ManagedModelUsageCursor};
    let mut dto = root(SessionExecutionState::Idle, 4).into_snapshot();
    dto.budget = SessionBudgetState::active(10, budget_snapshot());
    let mut aggregate = SessionAggregate::try_from(dto).unwrap();
    let before_usage = aggregate.snapshot().clone();
    let usage = ManagedBudgetUsageCursor {
        active_seconds: 5,
        ..Default::default()
    };
    aggregate
        .budget()
        .reconcile_cumulative_usage(usage.clone())
        .unwrap();
    assert_eq!(aggregate.usage_cursor, usage);
    assert_eq!(aggregate.budget.usage_cursor(), Some(&usage));
    // HC4: cumulative accounting changes the existing budget state but not its
    // accepted limit. It must remain a legal original Runtime mutation with no
    // authoring history; HC1 separately requires facts for limit changes.
    aggregate.compile_mutation(
        "owner",
        crate::state::IdempotencyRecord {
            key: "budget-usage".into(),
            payload_hash: "cumulative-usage".into(),
        },
        Vec::new(),
        100,
    );
    assert_eq!(
        before_usage.validate_observation_replacement(aggregate.snapshot()),
        Ok(())
    );
    assert_eq!(aggregate.update_history, before_usage.update_history);
    let before = aggregate.clone();
    assert!(
        aggregate
            .budget()
            .reconcile_cumulative_usage(ManagedBudgetUsageCursor {
                active_seconds: 4,
                ..Default::default()
            })
            .is_err()
    );
    assert_eq!(aggregate, before);
    let mut missing = usage;
    missing
        .by_model
        .insert("unpriced".into(), ManagedModelUsageCursor::default());
    assert!(
        aggregate
            .budget()
            .reconcile_cumulative_usage(missing)
            .is_err()
    );
    assert_eq!(aggregate, before);
}

#[test]
fn budget_limit_matrix_preserves_price_usage_and_removal_provenance() {
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    let before = aggregate.clone();
    for limit in [None, Some(0), Some(1)] {
        assert_eq!(
            aggregate.budget().update_limit(limit),
            Err(SessionAggregateCommandError::BudgetAbsent)
        );
        assert_eq!(aggregate, before);
    }
    let mut dto = aggregate.into_snapshot();
    dto.budget = SessionBudgetState::active(1, budget_snapshot());
    let mut aggregate = SessionAggregate::try_from(dto).unwrap();
    let before = aggregate.clone();
    assert_eq!(
        aggregate.budget().update_limit(Some(0)),
        Err(SessionAggregateCommandError::BudgetNotAboveConsumed)
    );
    assert_eq!(aggregate, before);
    aggregate.budget().update_limit(Some(2)).unwrap();
    assert_eq!(
        aggregate.budget.price_snapshot(),
        before.budget.price_snapshot()
    );
    aggregate.budget().update_limit(None).unwrap();
    let before = aggregate.clone();
    assert_eq!(
        aggregate.budget().update_limit(Some(3)),
        Err(SessionAggregateCommandError::BudgetRemoved)
    );
    assert_eq!(aggregate, before);
    aggregate.budget().update_limit(None).unwrap();
    assert_eq!(aggregate, before);
}
// O5: active/pending migration cannot change topology or discard a retained generation; no-op stutters.
#[test]
fn credential_migration_cannot_replace_topology_or_drop_a_retained_slot() {
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    let before = aggregate.clone();
    assert!(
        aggregate
            .resources()
            .migrate_repository_credential_pins(Default::default(), Some(Default::default()))
            .is_err()
    );
    assert_eq!(aggregate, before);
    assert!(
        !aggregate
            .resources()
            .migrate_repository_credential_pins(Default::default(), None)
            .unwrap()
    );
    assert_eq!(aggregate, before);
}

#[test]
fn import_uses_the_fresh_mutation_state_law_without_inventing_times() {
    for (execution, epochs, interval) in [
        (SessionExecutionState::Idle, vec![1], false),
        (SessionExecutionState::Running, vec![0], false),
        (SessionExecutionState::Running, vec![2], false),
        (SessionExecutionState::Terminated, vec![1], false),
        (SessionExecutionState::Idle, vec![], true),
    ] {
        let mut dto = root(SessionExecutionState::Running, 4).into_snapshot();
        dto.activity_epoch = 1;
        if interval {
            dto.begin_runtime_interval(10, 0);
        }
        dto.execution = execution;
        dto.active_activity_epochs = epochs.into_iter().collect();
        let mutation = SessionMutation {
            expected_revision: dto.revision,
            payload: SessionMutationPayload::Replace(dto.clone()),
            idempotency: IdempotencyRecord {
                key: "fresh".into(),
                payload_hash: "fresh".into(),
            },
            lifecycle_facts: Vec::new(),
        };
        assert_eq!(
            SessionAggregate::try_from(dto).unwrap_err(),
            mutation.validate().unwrap_err()
        );
    }
    // Historical Running roots with an empty active set remain readable and
    // settle through their retained scalar epoch, preserving legacy compatibility.
    let mut dto = root(SessionExecutionState::Running, 4).into_snapshot();
    dto.activity_epoch = 1;
    let restored = SessionAggregate::try_from(dto).unwrap();
    assert!(restored.active_activity_epochs.is_empty());
    assert_eq!(restored.created_at_unix_ms, None);
}

#[test]
fn admission_budget_and_counter_boundaries_preserve_rejection_atomicity() {
    // C1 exhausted budget; C2 continuation of an already committed await;
    // C3 epoch exhausted. C2 bypasses only budget, never the epoch fence.
    for continue_await in [false, true] {
        for exhausted_epoch in [false, true] {
            let mut dto = root(SessionExecutionState::Idle, 4).into_snapshot();
            dto.budget = SessionBudgetState::active(0, budget_snapshot());
            dto.activity_epoch = if exhausted_epoch { u64::MAX } else { 0 };
            let mut aggregate = SessionAggregate::try_from(dto).unwrap();
            let before = aggregate.clone();
            let expected = if !continue_await {
                SessionActivityAdmissionPlan::BudgetReached
            } else if exhausted_epoch {
                SessionActivityAdmissionPlan::EpochExhausted
            } else {
                SessionActivityAdmissionPlan::Open(1)
            };
            assert_eq!(
                aggregate
                    .activity()
                    .open(None, false, continue_await, 10, 0, None),
                expected
            );
            if !matches!(expected, SessionActivityAdmissionPlan::Open(_)) {
                assert_eq!(aggregate, before);
            }
        }
    }
    let mut dto = root(SessionExecutionState::Preparing, 4).into_snapshot();
    dto.realization_progress.attempts = u32::MAX;
    let mut aggregate = SessionAggregate::try_from(dto).unwrap();
    let before = aggregate.clone();
    assert!(
        aggregate
            .assignment()
            .claim(lease(Epoch::FIRST, 10), true)
            .is_err()
    );
    assert_eq!(aggregate, before);
}

#[test]
fn terminal_commands_close_running_interval_and_emit_one_revision_bound_fact() {
    // Independent triggers: archive, delete, and terminal execution failure.
    // Every trigger clears admission, closes the interval, and supplies the
    // exact required lifecycle fact without relying on an application caller.
    for trigger in 0..3 {
        let mut aggregate = root(SessionExecutionState::Idle, 4);
        aggregate.activity().open(None, false, false, 10, 7, None);
        match trigger {
            0 => {
                aggregate.lifecycle().archive("time", 20).unwrap();
            }
            1 => {
                aggregate
                    .lifecycle()
                    .request_delete("workspace", 0, 20)
                    .unwrap();
            }
            _ => {
                aggregate
                    .lifecycle()
                    .transition_execution_at(SessionExecutionState::ActivationFailed, 20)
                    .unwrap();
            }
        }
        assert!(aggregate.is_terminal());
        assert!(!aggregate.has_active_activities());
        assert!(aggregate.running_interval.is_none());
        assert_eq!(aggregate.closed_runtime_intervals.len(), 1);
        let mutation = aggregate.compile_mutation(
            "workspace",
            IdempotencyRecord {
                key: "terminal".into(),
                payload_hash: "same".into(),
            },
            vec![],
            20,
        );
        assert_eq!(mutation.lifecycle_facts.len(), 1);
        assert_eq!(
            mutation.lifecycle_facts[0].event_type,
            "session.runtime_interval_closed"
        );
        assert_eq!(mutation.validate().unwrap(), SessionRevision(5));
    }
}

#[test]
fn physical_readiness_promotes_admitted_activity_as_one_valid_root() {
    for admitted in [false, true] {
        let mut aggregate = root(SessionExecutionState::Preparing, 4);
        if admitted {
            aggregate.activity().open(None, true, false, 10, 7, None);
        }
        assert_eq!(
            aggregate.activity().realization_ready(20).unwrap(),
            !admitted
        );
        assert_eq!(
            aggregate.execution,
            if admitted {
                SessionExecutionState::Running
            } else {
                SessionExecutionState::Idle
            }
        );
        assert_eq!(aggregate.running_interval.is_some(), admitted);
        let mutation = aggregate.compile_mutation(
            "workspace",
            IdempotencyRecord {
                key: "ready".into(),
                payload_hash: "same".into(),
            },
            vec![],
            20,
        );
        assert!(mutation.validate().is_ok());
    }
}

#[test]
fn root_and_projection_renewal_is_atomic_and_terminal_claims_retain_frozen_projections() {
    use {
        awaken_agent_contract::McpTarget,
        awaken_session_configuration_contract::mcp::{
            McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim, SessionMcpAttachmentSet,
        },
    };
    let mcp = |foreign: bool| {
        let mut set = SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "tool".into(),
                target: McpTarget::parse_http("https://tool.test/mcp").unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
        let generation = set.attachments[0].generation.clone();
        set.claim_realization(
            &generation,
            McpRealizationClaim {
                realization_id: "realization".into(),
                runtime_incarnation: if foreign { "foreign" } else { "incarnation" }.into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: 10,
                stage_idempotency_key: "stage".into(),
            },
        )
        .unwrap();
        set.activate(&generation, "realization").unwrap();
        set.acknowledge_publication(&generation, "realization")
            .unwrap();
        set
    };
    for terminal in [false, true] {
        for foreign in [false, true] {
            let mut dto = root(SessionExecutionState::Idle, 4).into_snapshot();
            dto.mcp = mcp(foreign);
            dto.realization = Some(lease(Epoch::FIRST, 10));
            let mut aggregate = SessionAggregate::try_from(dto).unwrap();
            if terminal {
                aggregate.lifecycle().archive("archive", 15).unwrap();
            }
            let before = aggregate.clone();
            let result = aggregate.assignment().renew(lease(Epoch::FIRST, 20));
            if foreign && !terminal {
                assert!(result.is_err());
                assert_eq!(aggregate, before);
            } else {
                result.unwrap();
                assert_eq!(aggregate.realization, Some(lease(Epoch::FIRST, 20)));
                if terminal {
                    assert_eq!(aggregate.mcp, before.mcp);
                } else {
                    let after = aggregate.mcp.attachments[0].realization.as_ref().unwrap();
                    assert_eq!(after.lease_expires_at_unix_ms, 20);
                    assert_eq!(after.stage_idempotency_key, "stage");
                    assert!(aggregate.mcp.attachments[0].publication_acknowledged);
                }
            }
        }
    }
}

#[test]
fn standalone_execution_transition_cannot_publish_idle_with_active_activity() {
    // A physical readiness command must publish Running and its interval
    // together. Both standalone APIs reject the transient Idle state and
    // preserve every field when an activity is already admitted.
    for execution in [
        SessionExecutionState::Preparing,
        SessionExecutionState::Running,
    ] {
        let mut dto = root(execution, 4).into_snapshot();
        dto.activity_epoch = 1;
        dto.active_activity_epochs.insert(1);
        let mut root = SessionAggregate::try_from(dto).unwrap();
        let before = root.clone();
        assert!(
            root.lifecycle()
                .transition_execution(SessionExecutionState::Idle)
                .is_err()
        );
        assert_eq!(root, before);
        assert!(
            root.lifecycle()
                .transition_execution_at(SessionExecutionState::Idle, 10)
                .is_err()
        );
        assert_eq!(root, before);
    }
}

#[test]
fn configuration_capability_requires_idle_and_preserves_frozen_tool_policy() {
    // Mutability is granted once for the exclusive facet borrow. A terminal
    // or busy root cannot obtain it; a Frozen root may edit descriptive fields
    // but cannot replace its authoritative tool policy.
    for execution in [
        SessionExecutionState::Preparing,
        SessionExecutionState::Activating,
        SessionExecutionState::Running,
        SessionExecutionState::Terminated,
        SessionExecutionState::ActivationFailed,
    ] {
        let mut root = root(execution, 4);
        let before = root.clone();
        assert!(root.configuration().is_err());
        assert_eq!(root, before);
        assert!(root.budget().update_limit(None).is_err());
        assert_eq!(root, before);
    }
    let mut root = root(SessionExecutionState::Idle, 4);
    root.configuration()
        .unwrap()
        .set_title(Some("allowed".into()));
    root.configuration()
        .unwrap()
        .set_tools(Default::default())
        .unwrap();
    let mut dto = root.into_snapshot();
    let crate::state::SessionBaselineState::Frozen(ref mut baseline) = dto.baseline else {
        unreachable!()
    };
    baseline.mutation_policy = crate::state::SessionMutationPolicy::Frozen;
    let mut root = SessionAggregate::try_from(dto).unwrap();
    let before = root.clone();
    assert_eq!(
        root.configuration().unwrap().set_tools(Default::default()),
        Err(SessionAggregateCommandError::FrozenTools)
    );
    assert_eq!(root, before);
    root.configuration().unwrap().set_title(None);
    assert!(root.title.is_none());
    assert_eq!(root.tools, before.tools);
}

#[test]
fn uncounted_failures_count_only_failed_recovery_and_reset_on_counted_progress() {
    // Causes: C1 execution (counted Preparing/Activating vs uncounted Idle,
    // Running, Rescheduling); C2 the failure is a not-ready deferral.
    // Effects: E1 count + 1; E2 unchanged.
    // | Rule | C1        | C2    | effect |
    // | U1   | uncounted | false | E1     |
    // | U2   | uncounted | true  | E2     |
    // | U3   | counted   | any   | E2 (the claim already counted) |
    // Resets: R1 published realization readiness clears the count; R2 a counted
    // initial claim clears it with the other failure coordinates; R3 the
    // historical wire omits a zero count and decodes absence as zero.
    use SessionExecutionState::{Activating, Idle, Preparing, Rescheduling, Running};
    for (rule, execution, not_ready, expected) in [
        ("U1 idle", Idle, false, 1),
        ("U1 running", Running, false, 1),
        ("U1 rescheduling", Rescheduling, false, 1),
        ("U2 idle", Idle, true, 0),
        ("U2 running", Running, true, 0),
        ("U3 preparing", Preparing, false, 0),
        ("U3 activating", Activating, true, 0),
    ] {
        let mut aggregate = root(execution, 1);
        let counted = aggregate.assignment().note_uncounted_failure(not_ready);
        assert_eq!(counted, Ok(expected), "{rule}");
        assert_eq!(
            aggregate
                .snapshot()
                .realization_progress()
                .uncounted_failures(),
            expected,
            "{rule}"
        );
    }

    let mut running = root(Running, 1);
    for expected in 1..=3 {
        assert_eq!(
            running.assignment().note_uncounted_failure(false),
            Ok(expected)
        );
    }
    running.activity().realization_ready(20).unwrap();
    assert_eq!(
        running
            .snapshot()
            .realization_progress()
            .uncounted_failures(),
        0,
        "R1"
    );

    let mut preparing = root(Idle, 1);
    preparing
        .assignment()
        .note_uncounted_failure(false)
        .unwrap();
    let wire = serde_json::to_value(preparing.snapshot()).unwrap();
    assert_eq!(
        wire["realization_progress"]["uncounted_failures"], 1,
        "a non-zero count is persisted"
    );
    let mut dto = serde_json::from_value::<PersistedSession>(wire).unwrap();
    assert_eq!(
        dto.realization_progress().uncounted_failures(),
        1,
        "round-trip"
    );
    dto.execution = Preparing;
    let mut preparing = SessionAggregate::try_from(dto).unwrap();
    preparing
        .assignment()
        .claim(
            crate::state::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker:1:boot".into(),
                epoch: 1,
                expires_at_unix_ms: 500,
            },
            true,
        )
        .unwrap();
    assert_eq!(
        preparing
            .snapshot()
            .realization_progress()
            .uncounted_failures(),
        0,
        "R2"
    );
    let historical = serde_json::to_value(root(Running, 1).snapshot()).unwrap();
    assert!(
        historical["realization_progress"]
            .get("uncounted_failures")
            .is_none(),
        "R3 zero is omitted"
    );
}
