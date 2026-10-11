//! Independent state/authority decision tables for the controlled working root.
use super::*;
use crate::{SessionActivityAdmissionPlan, SessionAggregate, SessionAggregateCommandError};
use awaken_lease_contract::fence::Epoch;
use awaken_session_budget_contract::SessionBudgetState;

fn root(execution: SessionExecutionState, revision: u64) -> SessionAggregate {
    let fixture = mutation_tests::session("controlled", SessionRevision(revision));
    let crate::SessionBaselineState::Preparing(intent) = fixture.baseline else {
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

#[test]
fn terminal_cleanup_affinity_distinguishes_owner_and_runtime_incarnation() {
    // Cause graph: C1 logical owner exact; C2 Runtime incarnation exact;
    // C3 prior lease live; C4 physical Environment; C5 child/non-pristine
    // cleanup set. The original same-process continuation is C1&&C2; a
    // Worker restart is not that continuation merely because C1 still holds.
    //
    // | Rule | incumbent | C1+C2 | C3 | C4/C5 | effect |
    // | A1 | absent | - | - | - | original first-placement Unchanged |
    // | A2 | present | both exact | either | either | Unchanged |
    // | A3 | present | either changed | expired | neither | require original archive receipt |
    // | A4 | present | either changed | live or physical or non-pristine | Retained |
    // All 32 present combinations are covered. The pure query reads no
    // receipt, changes no root and grants no physical takeover authority.
    use crate::{
        SessionTerminalCleanupAffinity as Affinity, session_terminal_cleanup_affinity_for,
    };
    let mut initial = root(SessionExecutionState::Terminated, 4).into_snapshot();
    assert_eq!(
        session_terminal_cleanup_affinity_for(&initial, "worker-a", "worker-a:2:boot-new", 10),
        Affinity::Unchanged,
        "A1 no historical affinity; physical placement still has its application gate",
    );
    assert!(initial.terminal_cleanup.request(&initial.session_id));
    for same_owner in [false, true] {
        for same_runtime in [false, true] {
            for live in [false, true] {
                for physical in [false, true] {
                    for child in [false, true] {
                        let mut session = initial.clone();
                        session
                            .terminal_cleanup
                            .freeze_targets(
                                &session.session_id,
                                if child {
                                    vec!["child".into()]
                                } else {
                                    Vec::new()
                                },
                                0,
                                0,
                            )
                            .unwrap();
                        session.realization = Some(crate::SessionRealizationLease {
                            owner: "worker-a".into(),
                            runtime_incarnation: "worker-a:1:boot-old".into(),
                            epoch: 4,
                            expires_at_unix_ms: if live { u64::MAX } else { 1 },
                        });
                        if physical {
                            session.environment = crate::SessionEnvironmentState::Resident {
                                binding: "original-binding".into(),
                                effect_id: None,
                                generation: None,
                                idle_since_unix_ms: None,
                            };
                        }
                        let before = session.clone();
                        let expected = if same_owner && same_runtime {
                            Affinity::Unchanged
                        } else if !live && !physical && !child {
                            Affinity::RequiresUnattemptedArchiveReceipt
                        } else {
                            Affinity::Retained
                        };
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
                            "A2-A4 owner={same_owner} runtime={same_runtime} live={live} physical={physical} child={child}",
                        );
                        assert_eq!(session, before, "A1-A4 query never mutates root");
                    }
                }
            }
        }
    }
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
                let actual = aggregate.activity().open(exact, external, false, 10, 7);
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
        archived.activity().open(None, true, true, 20, 7),
        SessionActivityAdmissionPlan::Terminal
    );
    assert_eq!(archived, before);
}

#[test]
fn final_activity_closes_interval_and_compiles_one_intrinsic_fact() {
    let mut aggregate = root(SessionExecutionState::Idle, 4);
    assert_eq!(
        aggregate.activity().open(None, false, false, 10, 7),
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
    aggregate.activity().open(None, false, false, 10, 0);
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

fn lease(epoch: Epoch, expiry: u64) -> crate::SessionRealizationLease {
    crate::SessionRealizationLease {
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
    let id = awaken_session_event_contract::session_event_batch_id(
        &aggregate.session_id,
        SessionRevision(5),
    )
    .unwrap();
    let batch = crate::SessionEventBatch::compile(
        &aggregate.session_id,
        &id,
        vec![crate::SessionEventInput::UserMessage {
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
    let anchor = crate::SessionEventProjectionAnchor {
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
                crate::SessionEventProjectionAnchor {
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

fn budget_snapshot() -> awaken_session_budget_contract::ManagedListPriceSnapshot {
    awaken_session_budget_contract::ManagedListPriceSnapshot {
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
fn budget_reconciliation_rejects_rewinds_and_missing_prices_atomically() {
    use awaken_session_budget_contract::{ManagedBudgetUsageCursor, ManagedModelUsageCursor};
    let mut dto = root(SessionExecutionState::Idle, 4).into_snapshot();
    dto.budget = SessionBudgetState::active(10, budget_snapshot());
    let mut aggregate = SessionAggregate::try_from(dto).unwrap();
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
                    .open(None, false, continue_await, 10, 0),
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
        aggregate.activity().open(None, false, false, 10, 7);
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
            aggregate.activity().open(None, true, false, 10, 7);
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
    use awaken_session_mcp_contract::{
        McpAttachmentDraft, McpAttachmentOrigin, McpRealizationClaim, McpTarget,
        SessionMcpAttachmentSet,
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
    let crate::SessionBaselineState::Frozen(ref mut baseline) = dto.baseline else {
        unreachable!()
    };
    baseline.mutation_policy = crate::SessionMutationPolicy::Frozen;
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
