//! Decision table of the aggregate → Environment facts projection: every fact
//! the transition function reads is derived once from the root, the caller's
//! asserted lease and `now`.
use super::*;
use crate::{
    RealizationLeaseStanding, SandboxCheckpointRef, SandboxGeneration, SessionEnvironmentEvent,
    SessionEnvironmentInstruction, SessionEnvironmentOperation, SessionEnvironmentRejection,
    SessionEnvironmentState, SessionEnvironmentStep, SessionTerminality, SuspendPhase,
};

const NOW: u64 = 100_000;

fn root(policy: Option<crate::EnvironmentIdleRetentionPolicy>) -> PersistedSession {
    let mut session = mutation_tests::session("facts", SessionRevision(1));
    let crate::SessionBaselineState::Preparing(mut intent) = session.baseline.clone() else {
        unreachable!("fixture starts Preparing")
    };
    if let Some(policy) = policy {
        intent.control.environment.idle_retention = policy;
    }
    session.baseline =
        crate::SessionBaselineState::Frozen(intent.finalize().expect("fixture baseline").baseline);
    session
}

fn checkpoint_and_release() -> crate::EnvironmentIdleRetentionPolicy {
    crate::EnvironmentIdleRetentionPolicy {
        mode: crate::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 10,
        retention_secs: 1_000,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 4_096,
        max_checkpoint_duration_secs: 30,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    }
}

fn lease(expires_at_unix_ms: u64) -> crate::SessionRealizationLease {
    crate::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "runtime-a".into(),
        epoch: 2,
        expires_at_unix_ms,
    }
}

fn superseded_lease() -> crate::SessionRealizationLease {
    crate::SessionRealizationLease {
        epoch: 1,
        ..lease(NOW + 1)
    }
}

fn facts(
    session: &PersistedSession,
    now_unix_ms: u64,
    asserted: Option<&crate::SessionRealizationLease>,
) -> crate::SessionEnvironmentFacts {
    crate::SessionEnvironmentFacts::project(session, now_unix_ms, asserted)
}

fn generation(expires_at_unix_ms: u64) -> SandboxGeneration {
    SandboxGeneration::new(
        &crate::SessionId::from("facts"),
        1,
        expires_at_unix_ms,
        "config",
        "image",
    )
}

fn checkpoint(expires_at_unix_ms: u64) -> SandboxCheckpointRef {
    SandboxCheckpointRef {
        id: "checkpoint".into(),
        format: "awaken-fs-tar-v1".into(),
        digest: "digest".into(),
        size_bytes: 1,
        created_at_unix_ms: 1,
        expires_at_unix_ms,
        environment_fingerprint: "config".into(),
        base_image_fingerprint: "image".into(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: "suspend".into(),
    }
}

fn resident(idle_since_unix_ms: Option<u64>, generated: bool) -> SessionEnvironmentState {
    SessionEnvironmentState::Resident {
        binding: "binding".into(),
        effect_id: generated.then(|| "create".into()),
        generation: generated.then(|| generation(u64::MAX)),
        idle_since_unix_ms,
    }
}

#[test]
fn terminality_follows_disposition_and_the_cleanup_join() {
    // T1 active => Active; T2 archived (cleanup Fenced) => CleanupJoining;
    // T3 archived with frozen cleanup targets => Terminal.
    let active = root(None);
    let mut fenced = root(None);
    fenced.archive("2026-10-17T00:00:00Z", 0).unwrap();
    let mut requested = fenced.clone();
    requested.freeze_terminal_cleanup_targets([], 0, 0).unwrap();
    for (rule, session, expected) in [
        ("T1", &active, SessionTerminality::Active),
        ("T2", &fenced, SessionTerminality::CleanupJoining),
        ("T3", &requested, SessionTerminality::Terminal),
    ] {
        assert_eq!(facts(session, NOW, None).terminality, expected, "{rule}");
    }
}

#[test]
fn lease_standing_compares_the_asserted_lease_with_the_root() {
    // L1 no lease anywhere => Unfenced; L2 root lease, event acts under it =>
    // Fenced; L3 asserted == live root lease => Fenced; L4 asserted older
    // epoch => Stale; L5 asserted lease expired at now => Stale; L6 asserted
    // without a root lease => Stale.
    let mut leased = root(None);
    leased.realization = Some(lease(NOW + 1));
    let unleased = root(None);
    for (rule, session, asserted, expected) in [
        ("L1", &unleased, None, RealizationLeaseStanding::Unfenced),
        ("L2", &leased, None, RealizationLeaseStanding::Fenced),
        (
            "L3",
            &leased,
            Some(lease(NOW + 1)),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "L4",
            &leased,
            Some(superseded_lease()),
            RealizationLeaseStanding::Stale,
        ),
        (
            "L6",
            &unleased,
            Some(lease(NOW + 1)),
            RealizationLeaseStanding::Stale,
        ),
    ] {
        assert_eq!(
            facts(session, NOW, asserted.as_ref()).lease,
            expected,
            "{rule}"
        );
    }
    let mut expired = root(None);
    expired.realization = Some(lease(NOW));
    assert_eq!(
        facts(&expired, NOW, Some(&lease(NOW))).lease,
        RealizationLeaseStanding::Stale,
        "L5"
    );
}

#[test]
fn suspend_due_needs_policy_idle_edge_generation_and_deadline() {
    // D1 checkpoint-and-release + generated + idle edge at deadline => due;
    // D2 one millisecond before the deadline => not due; D3 no idle edge;
    // D4 default retention policy; D5 legacy binding without generation is
    // neither generated nor due.
    let at = |policy, environment| {
        let mut session = root(policy);
        session.environment = environment;
        facts(&session, NOW, None)
    };
    let deadline_start = NOW - 10_000;
    let due = at(
        Some(checkpoint_and_release()),
        resident(Some(deadline_start), true),
    );
    assert!(due.suspend_due && due.resident_generated, "D1");
    let early = at(
        Some(checkpoint_and_release()),
        resident(Some(deadline_start + 1), true),
    );
    assert!(!early.suspend_due, "D2");
    assert!(
        !at(Some(checkpoint_and_release()), resident(None, true)).suspend_due,
        "D3"
    );
    assert!(
        !at(None, resident(Some(deadline_start), true)).suspend_due,
        "D4"
    );
    let legacy = at(
        Some(checkpoint_and_release()),
        resident(Some(deadline_start), false),
    );
    assert!(!legacy.suspend_due && !legacy.resident_generated, "D5");
}

#[test]
fn continuation_facts_project_evidence_and_expiry() {
    // E1 Suspending evidence consistency is the state's own predicate;
    // E2 generation and checkpoint liveness are evaluated at now; E3 the step
    // is the state's projection; E4 execution and activity facts are copied.
    let operation = SessionEnvironmentOperation {
        effect_id: "suspend".into(),
        activity_epoch: 1,
        started_at_unix_ms: 1,
        realization: None,
    };
    let mut session = root(None);
    session.environment = SessionEnvironmentState::Suspending {
        operation,
        source_effect_id: Box::new("create".into()),
        source_binding: "binding".into(),
        generation: generation(NOW),
        suspend_phase: SuspendPhase::ReadyToDispose,
        checkpoint: None,
        source_release_preparation: None,
    };
    let suspending = facts(&session, NOW, None);
    assert_eq!(
        suspending.step,
        SessionEnvironmentStep::Suspending(SuspendPhase::ReadyToDispose),
        "E3"
    );
    assert!(!suspending.suspend_evidence_consistent, "E1");
    assert!(!suspending.generation_live, "E2 generation");

    session.environment = SessionEnvironmentState::Hibernated {
        checkpoint: checkpoint(NOW + 1),
        generation: generation(NOW + 1),
    };
    let live = facts(&session, NOW, None);
    assert!(live.checkpoint_live && live.generation_live, "E2 live");
    assert!(
        !facts(&session, NOW + 1, None).checkpoint_live,
        "E2 expired"
    );

    session.execution = SessionExecutionState::Running;
    session.active_activity_epochs.insert(1);
    let busy = facts(&session, NOW, None);
    assert!(!busy.execution_idle && busy.activities_open, "E4");
}

#[test]
fn transition_decides_over_the_projected_aggregate_facts() {
    // A1 a due Resident decides the suspend intent with its drain effect;
    // A2 a terminal root refuses the same command; A3 the realization gate is
    // closed while Hibernated.
    let mut session = root(Some(checkpoint_and_release()));
    session.environment = resident(Some(NOW - 10_000), true);
    let decision = crate::transition_environment(
        facts(&session, NOW, None),
        SessionEnvironmentEvent::AdvanceContinuation,
    )
    .unwrap();
    assert_eq!(
        decision.next(),
        SessionEnvironmentStep::Suspending(SuspendPhase::Quiescing),
        "A1"
    );
    assert_eq!(
        decision.instruction(),
        Some(SessionEnvironmentInstruction::Quiesce),
        "A1"
    );

    let mut terminal = session.clone();
    terminal.archive("2026-10-17T00:00:00Z", 0).unwrap();
    assert_eq!(
        crate::transition_environment(
            facts(&terminal, NOW, None),
            SessionEnvironmentEvent::AdvanceContinuation
        ),
        Err(SessionEnvironmentRejection::Terminal),
        "A2"
    );

    session.environment = SessionEnvironmentState::Hibernated {
        checkpoint: checkpoint(u64::MAX),
        generation: generation(u64::MAX),
    };
    assert!(!session.environment.admits_realization_effects(), "A3");
    assert_eq!(
        crate::transition_environment(
            facts(&session, NOW, None),
            SessionEnvironmentEvent::BeginRealization
        ),
        Err(SessionEnvironmentRejection::ContinuationInProgress),
        "A3"
    );
}

fn placed(
    runtime_placement: crate::SessionRuntimePlacement,
    policy: crate::EnvironmentIdleRetentionPolicy,
) -> PersistedSession {
    let mut session = root(Some(policy));
    let crate::SessionBaselineState::Frozen(baseline) = &mut session.baseline else {
        unreachable!("fixture baseline is frozen")
    };
    baseline.runtime_placement = runtime_placement;
    session
}

#[test]
fn continuation_topology_follows_the_frozen_placement_and_retention() {
    // P1 Coordinator-local placement + checkpoint-and-release => supported;
    // P2 external Worker + checkpoint-and-release => unsupported (no
    // claim-fenced continuation transport); P3 external Worker + default
    // retention => supported; P4 no frozen baseline => supported (the guard
    // reads only a frozen topology).
    let local = placed(
        crate::SessionRuntimePlacement::Local,
        checkpoint_and_release(),
    );
    let external = placed(
        crate::SessionRuntimePlacement::Worker,
        checkpoint_and_release(),
    );
    let external_default = placed(
        crate::SessionRuntimePlacement::Worker,
        crate::EnvironmentIdleRetentionPolicy::default(),
    );
    let preparing = mutation_tests::session("facts", SessionRevision(1));
    for (rule, session, expected) in [
        ("P1", &local, true),
        ("P2", &external, false),
        ("P3", &external_default, true),
        ("P4", &preparing, true),
    ] {
        assert_eq!(
            facts(session, NOW, None).continuation_topology_supported,
            expected,
            "{rule}"
        );
    }
}

fn memory_input(binding_id: &str) -> crate::ResolvedInput {
    let memory_store_id = awaken_resource_contract::MemoryStoreId::from("facts-memory");
    crate::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from(binding_id),
        source: crate::ResolvedInputSource::MemoryStore {
            memory_store_id: memory_store_id.clone(),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id,
                version: awaken_resource_contract::ConfigVersion(1),
                retention_policy: Default::default(),
            },
        },
        mount_path: format!("/memory/{binding_id}"),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    }
}

#[test]
fn suspend_memory_admission_reads_the_resident_binding_evidence() {
    // M1 Resident without Memory inputs => admitted; M2 a pending Memory
    // generation => not admitted (no single input/evidence join); M3 active
    // Memory over a binding that is not a Sandbox handle => not admitted;
    // M4 a non-Resident step never admits a suspend.
    let mut session = root(Some(checkpoint_and_release()));
    session.environment = resident(Some(NOW - 10_000), true);
    assert!(facts(&session, NOW, None).suspend_memory_admitted, "M1");

    let mut pending = session.clone();
    pending.resources.pending = Some(
        crate::ResolvedSessionResources::try_new(vec![memory_input("pending")], Vec::new())
            .unwrap(),
    );
    assert!(!facts(&pending, NOW, None).suspend_memory_admitted, "M2");

    let mut unparsable = session.clone();
    unparsable.resources = crate::SessionResourceState::from_active(
        crate::ResolvedSessionResources::try_new(vec![memory_input("active")], Vec::new()).unwrap(),
    );
    assert!(!facts(&unparsable, NOW, None).suspend_memory_admitted, "M3");

    session.environment = SessionEnvironmentState::Unmaterialized;
    assert!(!facts(&session, NOW, None).suspend_memory_admitted, "M4");
}

#[test]
fn continuation_facts_assert_the_lease_the_next_effect_presents() {
    // C1 Quiescing/Uploading/Restoring assert their operation's captured lease:
    // the same live lease is Fenced, a superseded one is Stale; C2 a legacy
    // operation without a lease acts under the root lease (Fenced); C3
    // ReadyToDispose and Disposing act under the root lease, so a higher-epoch
    // successor still finishes the release; C4 Resident and Hibernated act
    // under the root lease.
    let operation = |realization| SessionEnvironmentOperation {
        effect_id: "suspend".into(),
        activity_epoch: 1,
        started_at_unix_ms: 1,
        realization,
    };
    let suspending = |phase, realization| SessionEnvironmentState::Suspending {
        operation: operation(realization),
        source_effect_id: Box::new("create".into()),
        source_binding: "binding".into(),
        generation: generation(u64::MAX),
        suspend_phase: phase,
        checkpoint: None,
        source_release_preparation: None,
    };
    let restoring = |realization| SessionEnvironmentState::Restoring {
        operation: operation(realization),
        checkpoint: checkpoint(u64::MAX),
        generation: generation(u64::MAX),
    };
    let current = lease(NOW + 1);
    let superseded = superseded_lease();
    for (rule, environment, expected) in [
        (
            "C1 quiescing current",
            suspending(SuspendPhase::Quiescing, Some(current.clone())),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C1 quiescing superseded",
            suspending(SuspendPhase::Quiescing, Some(superseded.clone())),
            RealizationLeaseStanding::Stale,
        ),
        (
            "C1 uploading superseded",
            suspending(SuspendPhase::Uploading, Some(superseded.clone())),
            RealizationLeaseStanding::Stale,
        ),
        (
            "C1 restoring superseded",
            restoring(Some(superseded.clone())),
            RealizationLeaseStanding::Stale,
        ),
        (
            "C1 restoring current",
            restoring(Some(current.clone())),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C2 legacy unfenced operation",
            suspending(SuspendPhase::Uploading, None),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C3 ready to dispose after failover",
            suspending(SuspendPhase::ReadyToDispose, Some(superseded.clone())),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C3 disposing after failover",
            suspending(SuspendPhase::Disposing, Some(superseded.clone())),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C4 resident",
            resident(Some(1), true),
            RealizationLeaseStanding::Fenced,
        ),
        (
            "C4 hibernated",
            SessionEnvironmentState::Hibernated {
                checkpoint: checkpoint(u64::MAX),
                generation: generation(u64::MAX),
            },
            RealizationLeaseStanding::Fenced,
        ),
    ] {
        let mut session = root(Some(checkpoint_and_release()));
        session.realization = Some(current.clone());
        session.environment = environment;
        assert_eq!(
            crate::SessionEnvironmentFacts::project_continuation(&session, NOW).lease,
            expected,
            "{rule}"
        );
    }
}

#[test]
fn aggregate_laws_apply_only_what_the_environment_decision_admits() {
    // Every aggregate wrapper decides before its data law, so the decision's
    // terminality and evidence rules hold on each path and a refusal leaves
    // the candidate unchanged.
    // W1-W4 a terminal Session refuses the quiescence, checkpoint, source
    // release and restore observations (they used to apply on terminal roots);
    // W5 it refuses the expired-checkpoint deletion; W6 an active expired
    // Hibernated retires to Unmaterialized; W7 a live checkpoint is refused;
    // W8 a restore intent over an expired checkpoint is CheckpointExpired;
    // W9 a terminal Session refuses the suspend intent; W10 Uploading that
    // already holds a checkpoint refuses its checkpoint as inconsistent.
    use crate::SessionEnvironmentTransitionError as Error;
    let operation = SessionEnvironmentOperation {
        effect_id: "suspend".into(),
        activity_epoch: 1,
        started_at_unix_ms: 1,
        realization: None,
    };
    let suspending = |phase, checkpointed: bool| SessionEnvironmentState::Suspending {
        operation: operation.clone(),
        source_effect_id: Box::new("create".into()),
        source_binding: "binding".into(),
        generation: generation(u64::MAX),
        suspend_phase: phase,
        checkpoint: checkpointed.then(|| checkpoint(u64::MAX)),
        source_release_preparation: None,
    };
    let aggregate = |environment, terminal: bool| {
        let mut session = root(Some(checkpoint_and_release()));
        session.environment = environment;
        if terminal {
            session.archive("2026-10-17T00:00:00Z", 0).unwrap();
        }
        crate::SessionAggregate::try_from(session).unwrap()
    };
    let terminal = Err(Error::Rejected(SessionEnvironmentRejection::Terminal));
    let quiesced = crate::QuiescenceReceipt {
        effect_id: "suspend".into(),
        generation_id: generation(u64::MAX).id().to_owned(),
        activity_epoch: 1,
        live_environment_effects: 0,
        mcp_generations: Vec::new(),
    };
    let checkpointed = crate::CheckpointReceipt {
        effect_id: "suspend".into(),
        generation_id: generation(u64::MAX).id().to_owned(),
        checkpoint: checkpoint(u64::MAX),
    };
    let released = crate::SourceDisposedReceipt {
        effect_id: "suspend".into(),
        generation_id: generation(u64::MAX).id().to_owned(),
        source_binding: "binding".into(),
        terminated: true,
    };
    let restored = crate::RestoreReceipt {
        effect_id: "restore".into(),
        generation_id: generation(u64::MAX).id().to_owned(),
        checkpoint_id: "checkpoint".into(),
        binding: "restored".into(),
    };

    let mut root_ = aggregate(suspending(SuspendPhase::Quiescing, false), true);
    let before = root_.clone();
    assert_eq!(
        root_.environment().record_quiescence(&quiesced, &[], NOW),
        terminal,
        "W1"
    );
    assert_eq!(root_, before, "W1 unchanged");
    let mut root_ = aggregate(suspending(SuspendPhase::Uploading, false), true);
    assert_eq!(
        root_.environment().record_checkpoint(&checkpointed, NOW),
        terminal,
        "W2"
    );
    let mut root_ = aggregate(suspending(SuspendPhase::Disposing, true), true);
    assert_eq!(
        root_.environment().complete_suspend(&released, NOW),
        terminal,
        "W3"
    );
    let restoring = SessionEnvironmentState::Restoring {
        operation: SessionEnvironmentOperation {
            effect_id: "restore".into(),
            ..operation.clone()
        },
        checkpoint: checkpoint(u64::MAX),
        generation: generation(u64::MAX),
    };
    let mut root_ = aggregate(restoring, true);
    assert_eq!(
        root_.environment().complete_restore(&restored, NOW),
        terminal,
        "W4"
    );

    let expired = || SessionEnvironmentState::Hibernated {
        checkpoint: checkpoint(NOW),
        generation: generation(u64::MAX),
    };
    let mut root_ = aggregate(expired(), true);
    assert_eq!(
        root_.environment().discard_expired_checkpoint(NOW),
        terminal,
        "W5"
    );
    let mut root_ = aggregate(expired(), false);
    assert_eq!(
        root_.environment().discard_expired_checkpoint(NOW),
        Ok(true),
        "W6"
    );
    assert_eq!(
        root_.environment.step(),
        SessionEnvironmentStep::Unmaterialized,
        "W6"
    );
    let mut root_ = aggregate(
        SessionEnvironmentState::Hibernated {
            checkpoint: checkpoint(u64::MAX),
            generation: generation(u64::MAX),
        },
        false,
    );
    assert_eq!(
        root_.environment().discard_expired_checkpoint(NOW),
        Err(Error::Rejected(SessionEnvironmentRejection::CheckpointLive)),
        "W7"
    );
    let mut root_ = aggregate(expired(), false);
    assert_eq!(
        root_
            .environment()
            .begin_restore("workspace", NOW)
            .map(|_| ()),
        Err(Error::CheckpointExpired),
        "W8"
    );
    let mut root_ = aggregate(resident(Some(NOW - 10_000), true), true);
    assert_eq!(
        root_
            .environment()
            .begin_suspend_at("workspace", NOW)
            .map(|_| ()),
        Err(Error::Rejected(SessionEnvironmentRejection::Terminal)),
        "W9"
    );
    let mut root_ = aggregate(suspending(SuspendPhase::Uploading, true), false);
    let before = root_.clone();
    assert_eq!(
        root_.environment().record_checkpoint(&checkpointed, NOW),
        Err(Error::Rejected(
            SessionEnvironmentRejection::InconsistentEvidence
        )),
        "W10"
    );
    assert_eq!(root_, before, "W10 unchanged");
}
