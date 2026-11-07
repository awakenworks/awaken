//! Decision table for `SessionAssignmentOperations::begin`: lease grant and
//! reuse, Resource-generation admission and the MCP claim selection.
use super::*;
use crate::state::{
    SessionAggregate, SessionEnvironmentRejection, SessionRealizationAssignment,
    SessionRealizationBegin, SessionRealizationBeginRejection, SessionRealizationBeginRequest,
    SessionRealizationLease, SessionResourceManifest, session_realization_assignment,
};
use awaken_lease_contract::fence::{Epoch, EpochIntegrityViolation};
use awaken_session_configuration_contract::mcp::{
    McpAttachmentDraft, McpAttachmentOrigin, McpAttachmentState, McpRealizationClaim,
    SessionMcpAttachmentSet,
};

const WORKSPACE: &str = "workspace";
const NOW: u64 = 100;

fn root(execution: SessionExecutionState) -> SessionAggregate {
    let fixture = mutation_tests::session("begin", SessionRevision(4));
    let crate::state::SessionBaselineState::Preparing(intent) = fixture.baseline else {
        unreachable!()
    };
    let mut dto = PersistedSession::frozen_with_budget(
        "begin",
        intent.finalize().unwrap().baseline,
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    dto.revision = SessionRevision(4);
    dto.execution = execution;
    SessionAggregate::try_from(dto).unwrap()
}

fn with(
    execution: SessionExecutionState,
    lease: Option<SessionRealizationLease>,
    mcp: SessionMcpAttachmentSet,
) -> SessionAggregate {
    let mut dto = root(execution).into_snapshot();
    dto.realization = lease;
    dto.mcp = mcp;
    SessionAggregate::try_from(dto).unwrap()
}

fn lease(owner: &str, incarnation: &str, epoch: Epoch, expires: u64) -> SessionRealizationLease {
    SessionRealizationLease {
        owner: owner.into(),
        runtime_incarnation: incarnation.into(),
        epoch: epoch.get(),
        expires_at_unix_ms: expires,
    }
}

fn nth(value: u64) -> Epoch {
    Epoch::from_persisted(value).unwrap()
}

/// The same lease as persisted with an untrustworthy zero generation.
fn zero_epoch(mut lease: SessionRealizationLease) -> SessionRealizationLease {
    lease.epoch = 0;
    lease
}

fn request<'a>(
    owner: &'static str,
    incarnation: &'static str,
    reassign: bool,
    expected: Option<&'a SessionResourceManifest>,
) -> SessionRealizationBeginRequest<'a> {
    SessionRealizationBeginRequest::new(WORKSPACE, owner, incarnation, 500, reassign, expected)
}

fn draft(name: &str) -> McpAttachmentDraft {
    McpAttachmentDraft {
        name: name.into(),
        target: awaken_agent_contract::McpTarget::parse_http(format!("https://{name}.test/mcp"))
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        origin: McpAttachmentOrigin::Session,
    }
}

/// Three attachments: Requested, Realizing and Active under `old`.
fn attachments(old: &SessionRealizationLease) -> SessionMcpAttachmentSet {
    let mut set =
        SessionMcpAttachmentSet::from_initial(vec![draft("a"), draft("b"), draft("c")], None)
            .unwrap();
    for (index, id) in [(1, "realizing"), (2, "active")] {
        let generation = set.attachments[index].generation.clone();
        set.claim_realization(
            &generation,
            McpRealizationClaim {
                realization_id: id.into(),
                runtime_incarnation: old.runtime_incarnation.clone(),
                lease_epoch: old.epoch,
                lease_expires_at_unix_ms: old.expires_at_unix_ms,
                stage_idempotency_key: id.into(),
            },
        )
        .unwrap();
    }
    let active = set.attachments[2].generation.clone();
    set.activate(&active, "active").unwrap();
    set
}

fn requested_only() -> SessionMcpAttachmentSet {
    SessionMcpAttachmentSet::from_initial(vec![draft("a")], None).unwrap()
}

/// The claim law keeps an Active generation Active; Requested and Realizing
/// realize under the claiming lease.
fn assert_claimed_by(root: &SessionAggregate, index: usize, lease: &SessionRealizationLease) {
    let attachment = &root.mcp.attachments[index];
    let state = if index == 2 {
        McpAttachmentState::Active
    } else {
        McpAttachmentState::Realizing
    };
    assert_eq!(attachment.state, state, "{index}");
    let claim = attachment.realization.as_ref().unwrap();
    let generation = &attachment.generation;
    assert_eq!(
        claim.realization_id,
        crate::state::stable_fingerprint(&(
            &root.session_id,
            generation,
            &lease.runtime_incarnation,
            lease.epoch,
        ))
    );
    assert_eq!(
        claim.stage_idempotency_key,
        format!(
            "stage:begin:{}:{}:{}",
            generation.attachment_id().0,
            generation.version(),
            lease.epoch
        )
    );
    assert_eq!(claim.runtime_incarnation, lease.runtime_incarnation);
    assert_eq!(claim.lease_epoch, lease.epoch);
    assert_eq!(claim.lease_expires_at_unix_ms, lease.expires_at_unix_ms);
}

#[test]
fn begin_refusals_change_nothing() {
    let live_foreign = lease("other", "other:1", nth(3), 1_000);
    let resources = crate::state::ResolvedSessionResources::default();
    let foreign_workspace = SessionResourceManifest::at_revision("elsewhere", 0, resources.clone());
    let unretained = SessionResourceManifest::at_revision(WORKSPACE, 7, resources);
    let mut archived = root(SessionExecutionState::Idle);
    archived.lifecycle().archive("archived", 1).unwrap();
    let mut hibernated = root(SessionExecutionState::Idle).into_snapshot();
    hibernated.environment = crate::state::SessionEnvironmentState::Hibernated {
        checkpoint: crate::state::SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "awaken-fs-tar-v1".into(),
            digest: "digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "config".into(),
            base_image_fingerprint: "image".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "suspend".into(),
        },
        generation: crate::state::SandboxGeneration::new(
            &crate::state::SessionId::from("begin"),
            1,
            u64::MAX,
            "config",
            "image",
        ),
    };
    let hibernated = SessionAggregate::try_from(hibernated).unwrap();
    let idle = |lease| with(SessionExecutionState::Idle, lease, requested_only());
    let rows: [(&str, SessionAggregate, _, SessionRealizationBeginRejection); 10] = [
        (
            "B1 terminal",
            archived,
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::Environment(SessionEnvironmentRejection::Terminal),
        ),
        (
            "B1 continuation",
            hibernated,
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::Environment(
                SessionEnvironmentRejection::ContinuationInProgress,
            ),
        ),
        (
            "B2 Workspace",
            idle(None),
            request("owner", "owner:1", false, Some(&foreign_workspace)),
            SessionRealizationBeginRejection::ResourceWorkspaceMismatch,
        ),
        (
            "B3 generation",
            idle(None),
            request("owner", "owner:1", false, Some(&unretained)),
            SessionRealizationBeginRejection::ResourceGenerationNotRetained,
        ),
        (
            "B6 live foreign owner",
            idle(Some(live_foreign.clone())),
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::StaleOwnership,
        ),
        (
            "B6 exhausted after expiry",
            idle(Some(lease("owner", "owner:1", nth(u64::MAX), NOW))),
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::Exhausted),
        ),
        (
            "B6 exhausted on reassignment",
            idle(Some(lease("other", "other:1", nth(u64::MAX), 1_000))),
            request("owner", "owner:1", true, None),
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::Exhausted),
        ),
        (
            "B6 persisted zero, expired",
            idle(Some(zero_epoch(lease("owner", "owner:1", nth(1), NOW)))),
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::ZeroGranted),
        ),
        (
            "B6 persisted zero, live exact owner (no reuse)",
            idle(Some(zero_epoch(lease("owner", "owner:1", nth(1), 1_000)))),
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::ZeroGranted),
        ),
        (
            "B6 persisted zero, live foreign owner (not stale ownership)",
            idle(Some(zero_epoch(live_foreign.clone()))),
            request("owner", "owner:1", false, None),
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::ZeroGranted),
        ),
    ];
    for (row, mut aggregate, request, rejection) in rows {
        let before = aggregate.clone();
        assert_eq!(
            aggregate.assignment().begin(request, NOW),
            Err(rejection),
            "{row}"
        );
        assert_eq!(aggregate, before, "{row}");
    }
}

#[test]
fn begin_reuses_a_live_lease_and_claims_only_requested_generations() {
    let current = lease("owner", "owner:1", nth(3), 1_000);

    // B7: nothing to claim, nothing to commit.
    let mut quiet = with(
        SessionExecutionState::Idle,
        Some(current.clone()),
        SessionMcpAttachmentSet::default(),
    );
    let before = quiet.clone();
    assert_eq!(
        quiet
            .assignment()
            .begin(request("owner", "owner:1", false, None), NOW),
        Ok(SessionRealizationBegin::Unchanged {
            activate_pending_resources: true
        }),
        "B7"
    );
    assert_eq!(quiet, before, "B7");

    // B8: the same lease claims the Requested generation only.
    let mut aggregate = with(
        SessionExecutionState::Preparing,
        Some(current.clone()),
        attachments(&current),
    );
    let before = aggregate.clone();
    assert_eq!(
        aggregate
            .assignment()
            .begin(request("owner", "owner:1", false, None), NOW),
        Ok(SessionRealizationBegin::Claimed {
            assigned: false,
            activate_pending_resources: true
        }),
        "B8"
    );
    assert_eq!(
        aggregate.realization,
        Some(current.clone()),
        "B8 lease kept"
    );
    assert_eq!(
        aggregate.realization_progress.attempts, before.realization_progress.attempts,
        "B11 reuse never counts"
    );
    assert_claimed_by(&aggregate, 0, &current);
    assert_eq!(
        aggregate.mcp.attachments[1..],
        before.mcp.attachments[1..],
        "B8"
    );
}

#[test]
fn begin_assigns_a_successor_and_reclaims_every_live_generation() {
    let old = lease("owner", "owner:1", nth(3), 1_000);
    let rows = [
        // (row, current, target incarnation, reassign, execution, epoch, counted)
        (
            "B9 first",
            None,
            "owner:1",
            false,
            SessionExecutionState::Preparing,
            1,
            true,
        ),
        (
            "B9 expired",
            Some(lease("owner", "owner:1", nth(3), NOW)),
            "owner:1",
            false,
            SessionExecutionState::Activating,
            4,
            true,
        ),
        (
            "B9 restart",
            Some(old.clone()),
            "owner:2",
            false,
            SessionExecutionState::Idle,
            4,
            false,
        ),
        (
            "B9 reassign",
            Some(lease("other", "other:1", nth(3), 1_000)),
            "owner:1",
            true,
            SessionExecutionState::Running,
            4,
            false,
        ),
        (
            "B9 same-owner reassign",
            Some(old.clone()),
            "owner:1",
            true,
            SessionExecutionState::Idle,
            4,
            false,
        ),
    ];
    for (row, current, incarnation, reassign, execution, epoch, counted) in rows {
        let mut aggregate = with(execution, current, attachments(&old));
        let before = aggregate.clone();
        let target = request("owner", incarnation, reassign, None);
        assert_eq!(
            aggregate.assignment().begin(target, NOW),
            Ok(SessionRealizationBegin::Claimed {
                assigned: true,
                activate_pending_resources: true
            }),
            "{row}"
        );
        let granted = lease("owner", incarnation, nth(epoch), 500);
        assert_eq!(aggregate.realization, Some(granted.clone()), "{row}");
        for index in 0..3 {
            assert_claimed_by(&aggregate, index, &granted);
        }
        assert_eq!(
            aggregate.realization_progress.attempts,
            before.realization_progress.attempts + u32::from(counted),
            "{row} B11"
        );
    }
}

#[test]
fn begin_selects_the_claimed_resource_generation() {
    let input = crate::state::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("memory"),
        source: crate::state::ResolvedInputSource::MemoryStore {
            memory_store_id: "store".into(),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: "store".into(),
                version: awaken_resource_contract::ConfigVersion(1),
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/memory".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let desired = crate::state::ResolvedSessionResources::try_new(vec![input], Vec::new()).unwrap();
    let mut pending = root(SessionExecutionState::Idle);
    let revision = pending.resources().prepare(desired.clone()).unwrap();
    let active = SessionResourceManifest::at_revision(
        WORKSPACE,
        pending.resources.active_revision(),
        pending.resources.active.clone(),
    );
    let staged = SessionResourceManifest::at_revision(WORKSPACE, revision, desired);
    // (row, expected, activate pending)
    for (row, expected, activate) in [
        ("B4 current desired state", None, true),
        ("B4 claimed pending generation", Some(&staged), true),
        ("B5 claimed active generation", Some(&active), false),
    ] {
        let mut aggregate = pending.clone();
        assert_eq!(
            aggregate
                .assignment()
                .begin(request("owner", "owner:1", false, expected), NOW),
            Ok(SessionRealizationBegin::Claimed {
                assigned: true,
                activate_pending_resources: activate
            }),
            "{row}"
        );
        // B10: only a selected pending generation starts its attempt.
        assert_eq!(
            aggregate.resources != pending.resources,
            activate,
            "{row} B10"
        );
    }
}

#[test]
fn assignment_follows_the_owner_incarnation_epoch_decision_table() {
    // Cause/effect graph: C1 a current fence exists and is live; C2 owner
    // matches; C3 Runtime incarnation matches; C4 authenticated topology
    // explicitly requests reassignment; C5 epoch can advance.
    // Effects: E1 reuse only the exact live owner/incarnation; E2 reject a
    // live foreign owner; E3 every new assignment mints exactly successor
    // epoch (or 1 from absence); E4 an untrustworthy persisted generation
    // (exhausted where a successor is minted, or zero on any existing lease)
    // fails closed as a permanent lease-integrity refusal.
    //
    // | Rule | Live | Owner | Incarnation | Reassign | Effect |
    // |---|---|---|---|---|---|
    // | R1 | yes | same | same | no | E1 reuse |
    // | R2 | yes | other | any | no | E2 stale |
    // | R3 | no | any | any | any | E3 successor |
    // | R4 | yes | same | other | no | E3 successor |
    // | R5 | yes | other | any | yes | E3 successor |
    // | R6 | assignment at u64::MAX | any | any | any | E4 Exhausted |
    // | R7 | yes | same | same | yes | E3 successor |
    // | R8 | persisted epoch 0 | any | any | any | E4 ZeroGranted |
    // R7 models an approval continuation after normal Work retirement in
    // the SAME Worker process. Its local predecessor epoch may already be
    // closed, so authenticated reassignment cannot reuse that epoch. This
    // differs from R1 ordinary application replay without a claim grant.
    // R7 no longer reads a persisted 0 as "never granted" (previously it
    // assigned epoch 1 over it): a lease that exists at epoch 0 is R8.
    assert_eq!(
        session_realization_assignment(Some(7), true, true, true, false),
        SessionRealizationAssignment::Reuse,
        "R1/E1"
    );
    assert_eq!(
        session_realization_assignment(Some(7), true, false, false, false),
        SessionRealizationAssignment::StaleOwnership,
        "R2/E2"
    );
    assert_eq!(
        session_realization_assignment(Some(7), false, false, false, false),
        SessionRealizationAssignment::Assign { epoch: 8 },
        "R3/E3"
    );
    assert_eq!(
        session_realization_assignment(Some(7), true, true, false, false),
        SessionRealizationAssignment::Assign { epoch: 8 },
        "R4/E3"
    );
    assert_eq!(
        session_realization_assignment(Some(7), true, false, false, true),
        SessionRealizationAssignment::Assign { epoch: 8 },
        "R5/E3"
    );
    assert_eq!(
        session_realization_assignment(Some(u64::MAX), false, true, true, false),
        SessionRealizationAssignment::LeaseIntegrity(EpochIntegrityViolation::Exhausted),
        "R6/E4"
    );
    for epoch in [7, u64::MAX - 1, u64::MAX] {
        assert_eq!(
            session_realization_assignment(Some(epoch), true, true, true, true),
            epoch.checked_add(1).map_or(
                SessionRealizationAssignment::LeaseIntegrity(EpochIntegrityViolation::Exhausted),
                |epoch| SessionRealizationAssignment::Assign { epoch },
            ),
            "R7/E3; R6/E4 at exhaustion"
        );
    }
    for live in [false, true] {
        for same_owner in [false, true] {
            for same_incarnation in [false, true] {
                for reassign in [false, true] {
                    assert_eq!(
                        session_realization_assignment(
                            Some(0),
                            live,
                            same_owner,
                            same_incarnation,
                            reassign
                        ),
                        SessionRealizationAssignment::LeaseIntegrity(
                            EpochIntegrityViolation::ZeroGranted
                        ),
                        "R8/E4 live={live} owner={same_owner} \
                         incarnation={same_incarnation} reassign={reassign}"
                    );
                }
            }
        }
    }
    // A Reuse of an exhausted live lease mints nothing, so it stays Reuse.
    assert_eq!(
        session_realization_assignment(Some(u64::MAX), true, true, true, false),
        SessionRealizationAssignment::Reuse,
        "R1 at u64::MAX"
    );
}

#[test]
fn reassign_installs_the_successor_generation_or_refuses_unchanged() {
    // | Rule | current lease                     | claimant      | effect                                  |
    // |------|-----------------------------------|---------------|-----------------------------------------|
    // | R1   | none                              | owner:1       | epoch 1                                 |
    // | R2   | live foreign, epoch 3             | owner:1       | epoch 4, foreign lease superseded       |
    // | R2   | expired same owner, epoch 3       | owner:1       | epoch 4 (never reuses the generation)   |
    // | R3   | epoch u64::MAX / persisted 0      | owner:1       | LeaseIntegrity, root unchanged          |
    // | R4   | none                              | blank owner   | Assignment(InvalidLeaseGeneration)      |
    // Every installed lease is compensable exactly once by restore_predecessor.
    let idle = |lease| with(SessionExecutionState::Idle, lease, requested_only());
    let granted: [(&str, Option<SessionRealizationLease>, u64); 3] = [
        ("R1 never leased", None, 1),
        (
            "R2 live foreign",
            Some(lease("other", "other:1", nth(3), 1_000)),
            4,
        ),
        (
            "R2 expired same owner",
            Some(lease("owner", "owner:1", nth(3), NOW)),
            4,
        ),
    ];
    for (row, predecessor, epoch) in granted {
        let mut aggregate = idle(predecessor.clone());
        aggregate.lifecycle().archive("archived", 1).unwrap();
        aggregate
            .assignment()
            .record_failure("prior failure".into(), 7, None);
        let before = aggregate.clone();
        let installed = aggregate
            .assignment()
            .reassign("owner", "owner:1", 900)
            .unwrap_or_else(|error| panic!("{row}: {error}"));
        assert_eq!(
            installed,
            lease("owner", "owner:1", nth(epoch), 900),
            "{row}"
        );
        assert_eq!(aggregate.realization.as_ref(), Some(&installed), "{row}");
        assert_eq!(
            aggregate.realization_progress, before.realization_progress,
            "{row}: reassignment counts no initial attempt"
        );
        aggregate
            .assignment()
            .restore_predecessor(&installed, predecessor)
            .unwrap_or_else(|error| panic!("{row}: {error}"));
        assert_eq!(aggregate, before, "{row}: exact compensation");
    }
    let mut zero = lease("owner", "owner:1", nth(1), NOW);
    zero.epoch = 0;
    let refused: [(&str, Option<SessionRealizationLease>, &str, _); 3] = [
        (
            "R3 exhausted",
            Some(lease("other", "other:1", nth(u64::MAX), NOW)),
            "owner",
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::Exhausted),
        ),
        (
            "R3 persisted zero",
            Some(zero),
            "owner",
            SessionRealizationBeginRejection::LeaseIntegrity(EpochIntegrityViolation::ZeroGranted),
        ),
        (
            "R4 blank owner",
            None,
            " ",
            SessionRealizationBeginRejection::Assignment(
                crate::state::SessionAggregateCommandError::InvalidLeaseGeneration,
            ),
        ),
    ];
    for (row, current, owner, rejection) in refused {
        let mut aggregate = idle(current);
        let before = aggregate.clone();
        assert_eq!(
            aggregate.assignment().reassign(owner, "owner:1", 900),
            Err(rejection),
            "{row}"
        );
        assert_eq!(aggregate, before, "{row}");
    }
}

#[test]
fn an_untrustworthy_persisted_lease_generation_is_one_permanent_integrity_refusal() {
    // The aggregate classifies its persisted realization lease generation in
    // one place (`successor_epoch` and the assignment kernel), and every
    // operation that reads it refuses with the same permanent class.
    // Previously a persisted zero had four readings: `begin` assigned epoch 1
    // over it, `claim` returned InvalidLeaseGeneration (surfaced as
    // Unavailable), `restore_predecessor` returned InvalidPredecessor and
    // `reassign` returned EpochExhausted.
    //
    // | Rule | operation           | persisted lease        | effect                                   |
    // |------|---------------------|------------------------|------------------------------------------|
    // | I1   | begin               | epoch 0                | LeaseIntegrity(ZeroGranted), unchanged   |
    // | I2   | begin (assignment)  | epoch u64::MAX         | LeaseIntegrity(Exhausted), unchanged     |
    // | I3   | claim (successor)   | epoch 0                | LeaseIntegrity(ZeroGranted), unchanged   |
    // | I4   | claim (successor)   | epoch u64::MAX         | LeaseIntegrity(Exhausted), unchanged     |
    // | I5   | reassign            | epoch 0 / u64::MAX     | LeaseIntegrity(ZeroGranted / Exhausted)  |
    // | I6   | restore_predecessor | predecessor 0 / MAX    | LeaseIntegrity(ZeroGranted / Exhausted)  |
    // | I7   | begin               | no lease               | Assign epoch 1 (never granted)           |
    // | I8   | I1..I6 repeated     | same root              | the same refusal; nothing advances       |
    use crate::state::SessionAggregateCommandError as Command;
    let idle = |lease| with(SessionExecutionState::Idle, lease, requested_only());
    let zero = zero_epoch(lease("owner", "owner:1", nth(1), NOW));
    let top = lease("owner", "owner:1", nth(u64::MAX), NOW);
    for attempt in 0..3 {
        for (rule, current, violation) in [
            ("I1", zero.clone(), EpochIntegrityViolation::ZeroGranted),
            ("I2", top.clone(), EpochIntegrityViolation::Exhausted),
        ] {
            let mut aggregate = idle(Some(current));
            let before = aggregate.clone();
            assert_eq!(
                aggregate
                    .assignment()
                    .begin(request("owner", "owner:2", false, None), NOW),
                Err(SessionRealizationBeginRejection::LeaseIntegrity(violation)),
                "{rule} attempt {attempt}"
            );
            assert_eq!(aggregate, before, "{rule} attempt {attempt}");
        }
        for (rule, current, violation) in [
            ("I3", zero.clone(), EpochIntegrityViolation::ZeroGranted),
            ("I4", top.clone(), EpochIntegrityViolation::Exhausted),
        ] {
            let mut aggregate = idle(Some(current));
            let before = aggregate.clone();
            assert_eq!(
                aggregate
                    .assignment()
                    .claim(lease("other", "other:1", nth(1), 1_000), false),
                Err(Command::LeaseIntegrity(violation)),
                "{rule} attempt {attempt}"
            );
            assert_eq!(aggregate, before, "{rule} attempt {attempt}");
        }
        for (current, violation) in [
            (zero.clone(), EpochIntegrityViolation::ZeroGranted),
            (top.clone(), EpochIntegrityViolation::Exhausted),
        ] {
            let mut aggregate = idle(Some(current));
            let before = aggregate.clone();
            assert_eq!(
                aggregate.assignment().reassign("owner", "owner:1", 900),
                Err(SessionRealizationBeginRejection::LeaseIntegrity(violation)),
                "I5 attempt {attempt}"
            );
            assert_eq!(aggregate, before, "I5 attempt {attempt}");
        }
        let attempted = lease("owner", "owner:1", nth(1), 900);
        for (predecessor, violation) in [
            (zero.clone(), EpochIntegrityViolation::ZeroGranted),
            (top.clone(), EpochIntegrityViolation::Exhausted),
        ] {
            let mut aggregate = idle(Some(attempted.clone()));
            let before = aggregate.clone();
            assert_eq!(
                aggregate
                    .assignment()
                    .restore_predecessor(&attempted, Some(predecessor)),
                Err(Command::LeaseIntegrity(violation)),
                "I6 attempt {attempt}"
            );
            assert_eq!(aggregate, before, "I6 attempt {attempt}");
        }
    }
    let mut fresh = idle(None);
    assert!(matches!(
        fresh
            .assignment()
            .begin(request("owner", "owner:1", false, None), NOW),
        Ok(SessionRealizationBegin::Claimed { assigned: true, .. })
    ));
    assert_eq!(
        fresh.realization.as_ref().map(|lease| lease.epoch),
        Some(1),
        "I7"
    );
}
