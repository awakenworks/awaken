//! Decision tables of the terminal cleanup recovery (R1–R9 per Session, S1–S7
//! per sweep). The control, the realization clock and the process-local
//! projections are recording fakes; the effects are the neutral role, which no
//! row reaches, and the cleanup role only installs and acknowledges.

use super::*;
use crate::terminal_cleanup_driver::tests::{lease, work};
use awaken_session_execution_contract::NeutralRuntime;
use awaken_session_realization_contract::{
    SessionTerminalCleanupAssignment, SessionTerminalCleanupWork,
};
use awaken_session_state_contract::SessionTerminalCleanupAction;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

type WorkReply = Result<Option<SessionTerminalCleanupWork>, SessionRealizationControlFailure>;
type ClaimReply =
    Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure>;

#[derive(Default)]
struct ScriptedControl {
    claims: Mutex<VecDeque<ClaimReply>>,
    /// Replies to `terminal_cleanup_work`; an empty script answers `Ok(None)`.
    work: Mutex<VecDeque<WorkReply>>,
    events: Mutex<Vec<String>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl ScriptedControl {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalCleanupQueue for ScriptedControl {
    async fn claim_next_terminal_cleanup(
        &self,
        _target: SessionRealizationTarget,
        after_session_id: Option<&SessionId>,
    ) -> ClaimReply {
        self.events.lock().unwrap().push(format!(
            "claim-after:{}",
            after_session_id.map_or("-", SessionId::as_str)
        ));
        self.claims.lock().unwrap().pop_front().unwrap_or(Ok(None))
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) -> WorkReply {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        // Yield so sibling reconciliations of one sweep overlap here.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.events
            .lock()
            .unwrap()
            .push(format!("work:{session_id}"));
        self.work.lock().unwrap().pop_front().unwrap_or(Ok(None))
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalPreparationControl for ScriptedControl {
    async fn authorize_terminal_cleanup_effect(
        &self,
        _effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        SessionRealizationControlFailure,
    > {
        unreachable!("recovery rows never prepare")
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        _lease: &SessionRealizationLease,
        _preparation: awaken_session_state_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        unreachable!("recovery rows never prepare")
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalDisposalControl for ScriptedControl {
    async fn authorize_terminal_cleanup_disposal(
        &self,
        _effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        unreachable!("recovery rows never dispose")
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        _lease: &SessionRealizationLease,
        _receipt: awaken_session_state_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        unreachable!("recovery rows never dispose")
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
    for ScriptedControl
{
    async fn terminal_repository_publication_command(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        self.events
            .lock()
            .unwrap()
            .push("project-publication".into());
        Ok(None)
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
        _receipt: awaken_session_state_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        unreachable!("recovery rows never publish")
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
        _rejection: awaken_session_state_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        unreachable!("recovery rows never publish")
    }
}

/// A realization role whose only behaviour is a working authority clock, so
/// the terminal driver can poll Control.
struct ClockedRealization;

#[async_trait::async_trait]
impl RealizationRuntime for ClockedRealization {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        Ok(
            awaken_lease_contract::authority::AuthorityProofStarted::from_clock_sample(
                awaken_time::mono::now(),
                std::time::Duration::ZERO,
            ),
        )
    }

    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, RunError> {
        NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        NeutralRuntime
            .install_session_projection(thread, projection, mode)
            .await
    }

    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &SessionRealizationLease,
    ) -> Result<(), RunError> {
        NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

/// A terminal cleanup role that installs every assignment and acknowledges
/// silently; no recovery row prepares.
struct InstallingCleanup;

#[async_trait::async_trait]
impl TerminalCleanupRuntime for InstallingCleanup {
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        _effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        _authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        unreachable!("recovery rows never prepare")
    }

    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        _effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
    }

    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        _effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
    }

    async fn acknowledge_completed_terminal_cleanup(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) {
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

/// Recording process-local projections. The origin is the Session id it was
/// captured for.
#[derive(Default)]
struct RecordingProjections {
    origin_unreadable: bool,
    no_origin: bool,
    completed_retired: bool,
    retire_fails: bool,
    revoke_fails: bool,
    events: Mutex<Vec<String>>,
}

impl RecordingProjections {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn push(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
}

#[async_trait::async_trait]
impl TerminalRecoveryProjections for RecordingProjections {
    type Origin = String;

    fn capture_terminal_origin(&self, session_id: &SessionId) -> Result<Option<String>, RunError> {
        if self.origin_unreadable {
            return Err(RunError::unavailable("foreground record poisoned"));
        }
        self.push(format!("capture:{session_id}"));
        Ok((!self.no_origin).then(|| session_id.to_string()))
    }

    async fn retire_completed_terminal_projection(
        &self,
        session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) -> Result<bool, RunError> {
        if self.retire_fails {
            return Err(RunError::unavailable("installed Sessions poisoned"));
        }
        self.push(format!("retire-completed:{session_id}"));
        Ok(self.completed_retired)
    }

    fn interrupt_origin_runs(&self, origin: &String) {
        self.push(format!("interrupt:{origin}"));
    }

    async fn retire_origin_generation(
        &self,
        session_id: &SessionId,
        _lease: &SessionRealizationLease,
        origin: &String,
    ) -> Result<(), RunError> {
        self.push(format!("retire-origin:{session_id}:{origin}"));
        Ok(())
    }

    async fn revoke_settled_projection(&self, session_id: &SessionId) -> Result<(), RunError> {
        self.push(format!("revoke:{session_id}"));
        if self.revoke_fails {
            Err(RunError::unavailable("revocation pending"))
        } else {
            Ok(())
        }
    }
}

fn participants<'a>(
    control: &'a ScriptedControl,
    projections: &'a RecordingProjections,
) -> TerminalRecoveryParticipants<'a, RecordingProjections> {
    TerminalRecoveryParticipants {
        control,
        realization: &ClockedRealization,
        effects: &NeutralRuntime,
        cleanup: &InstallingCleanup,
        projections,
    }
}

fn session() -> SessionId {
    SessionId::from("session")
}

fn target() -> SessionRealizationTarget {
    SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: "worker/boot".into(),
        lease_expires_at_unix_ms: 10_000,
        reassign_existing_lease: false,
    }
}

fn assignment(session_id: &str) -> SessionTerminalCleanupAssignment {
    let mut assignment = work(SessionTerminalCleanupAction::Waiting, &lease()).assignment;
    assignment.session_id = session_id.into();
    assignment
}

fn scripted(work: Vec<WorkReply>) -> ScriptedControl {
    ScriptedControl {
        work: Mutex::new(work.into()),
        ..ScriptedControl::default()
    }
}

#[tokio::test]
async fn r1_reported_work_is_driven_through_the_terminal_driver() {
    // R1: Control reports work -> the terminal driver re-reads and drives it
    // (here a Waiting action without publication, then completion) and no
    // process-local retirement is decided by the recovery itself.
    let control = scripted(vec![
        Ok(Some(work(SessionTerminalCleanupAction::Waiting, &lease()))),
        Ok(Some(work(SessionTerminalCleanupAction::Waiting, &lease()))),
    ]);
    let projections = RecordingProjections::default();
    assert_eq!(
        reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
            .await
            .unwrap(),
        TerminalReconciliation::Fenced
    );
    assert_eq!(
        control.events(),
        ["work:session", "work:session", "project-publication"],
        "R1 the driver owns the re-read and the effect order"
    );
    assert_eq!(
        projections.events(),
        ["capture:session"],
        "R1 no local effect"
    );
}

#[tokio::test]
async fn r2_r3_no_work_retires_a_completed_projection_or_reports_settlement() {
    // R2: no work and a completed projection of this generation was retired
    // -> Fenced. R3: no work and nothing retired -> SettledElsewhere.
    for (rule, completed_retired, expected) in [
        ("R2", true, TerminalReconciliation::Fenced),
        ("R3", false, TerminalReconciliation::SettledElsewhere),
    ] {
        let control = scripted(vec![Ok(None)]);
        let projections = RecordingProjections {
            completed_retired,
            ..RecordingProjections::default()
        };
        assert_eq!(
            reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
                .await
                .unwrap(),
            expected,
            "{rule}"
        );
        assert_eq!(
            projections.events(),
            ["capture:session", "retire-completed:session"],
            "{rule} no Run is signalled"
        );
    }
}

#[tokio::test]
async fn t1_t3_terminal_absence_signals_only_the_captured_generation() {
    // T1: NotFound and a completed projection retired -> signal the Run
    // controls captured before the lookup. T2: nothing retired and an origin
    // was captured -> retire that origin's generation (the port signals only
    // at its own close). T3: nothing retired and nothing captured -> no
    // effect. Every row is Fenced and never interrupts by Session id.
    for (rule, completed_retired, no_origin, expected_events) in [
        (
            "T1",
            true,
            false,
            vec![
                "capture:session",
                "retire-completed:session",
                "interrupt:session",
            ],
        ),
        (
            "T2",
            false,
            false,
            vec![
                "capture:session",
                "retire-completed:session",
                "retire-origin:session:session",
            ],
        ),
        (
            "T3",
            false,
            true,
            vec!["capture:session", "retire-completed:session"],
        ),
    ] {
        let control = scripted(vec![Err(SessionRealizationControlFailure::NotFound)]);
        let projections = RecordingProjections {
            completed_retired,
            no_origin,
            ..RecordingProjections::default()
        };
        assert_eq!(
            reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
                .await
                .unwrap(),
            TerminalReconciliation::Fenced,
            "{rule}"
        );
        assert_eq!(projections.events(), expected_events, "{rule}");
    }
}

#[tokio::test]
async fn r7_r9_failures_refuse_without_a_local_effect() {
    // R7: another Control failure -> Control error and no local effect.
    // R8: an unreadable origin refuses before any Control I/O.
    // R9: a failed completed-projection retirement is a Projection error.
    let control = scripted(vec![Err(SessionRealizationControlFailure::Unavailable(
        "control down".into(),
    ))]);
    let projections = RecordingProjections::default();
    let error =
        reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(
        matches!(
            error,
            TerminalRecoveryError::Control(SessionRealizationControlFailure::Unavailable(_))
        ),
        "R7 {error}"
    );
    assert_eq!(projections.events(), ["capture:session"], "R7");

    let control = scripted(vec![Ok(None)]);
    let projections = RecordingProjections {
        origin_unreadable: true,
        ..RecordingProjections::default()
    };
    let error =
        reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(matches!(error, TerminalRecoveryError::Projection(_)), "R8");
    assert!(control.events().is_empty(), "R8 no Control I/O");

    let control = scripted(vec![Err(SessionRealizationControlFailure::NotFound)]);
    let projections = RecordingProjections {
        retire_fails: true,
        ..RecordingProjections::default()
    };
    let error =
        reconcile_terminal_cleanup(&session(), &lease(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(matches!(error, TerminalRecoveryError::Projection(_)), "R9");
    assert_eq!(projections.events(), ["capture:session"], "R9 no signal");
}

fn claims(ids: &[&str]) -> Mutex<VecDeque<ClaimReply>> {
    Mutex::new(
        ids.iter()
            .map(|id| Ok(Some(assignment(id))))
            .collect::<VecDeque<_>>(),
    )
}

#[tokio::test]
async fn s1_s3_a_sweep_reconciles_every_claim_within_its_bounds() {
    // S1: claims until Control has none, each reconciled, the count returned
    // and the cursor advanced. S3: at most four reconciliations in flight.
    let ids: Vec<String> = (0..10).map(|index| format!("session-{index:02}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let control = ScriptedControl {
        claims: claims(&id_refs),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections {
        completed_retired: true,
        ..RecordingProjections::default()
    };
    assert_eq!(
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap(),
        10,
        "S1"
    );
    let events = control.events();
    assert!(
        events.contains(&"claim-after:-".to_string()),
        "S1 first claim"
    );
    assert!(
        events.contains(&"claim-after:session-09".to_string()),
        "S1 the cursor advances to the last claim"
    );
    for id in &ids {
        assert!(events.contains(&format!("work:{id}")), "S1 {id} reconciled");
    }
    assert_eq!(
        control.max_in_flight.load(Ordering::SeqCst),
        MAX_CONCURRENT_TERMINAL_RECOVERIES,
        "S3"
    );
}

#[tokio::test]
async fn s2_one_sweep_claims_at_most_its_bound() {
    // S2: more assignments than the sweep bound -> only the bound is claimed.
    let ids: Vec<String> = (0..MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP + 6)
        .map(|index| format!("session-{index:03}"))
        .collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let control = ScriptedControl {
        claims: claims(&id_refs),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections {
        completed_retired: true,
        ..RecordingProjections::default()
    };
    assert_eq!(
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap(),
        MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP,
        "S2"
    );
    let claimed = control
        .events()
        .iter()
        .filter(|event| event.starts_with("claim-after:"))
        .count();
    assert_eq!(
        claimed, MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP,
        "S2 no extra claim"
    );
}

#[tokio::test]
async fn s4_s5_a_stalled_cursor_or_failed_claim_stops_claiming_but_finishes_claimed_work() {
    // S4: the cursor does not advance -> claiming stops with
    // ClaimCursorStalled, the repeated assignment is never driven twice and
    // the claimed one still finishes. S5: a claim fails -> Claim error, the
    // claimed work still finishes.
    let control = ScriptedControl {
        claims: claims(&["session-b", "session-a"]),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections {
        completed_retired: true,
        ..RecordingProjections::default()
    };
    let error =
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(error.session_id.is_none(), "S4");
    assert!(
        matches!(error.error, TerminalRecoveryError::ClaimCursorStalled),
        "S4 {error}"
    );
    let events = control.events();
    assert!(
        events.contains(&"work:session-b".to_string()),
        "S4 claimed work"
    );
    assert!(
        !events.contains(&"work:session-a".to_string()),
        "S4 not driven"
    );

    let control = ScriptedControl {
        claims: Mutex::new(VecDeque::from([
            Ok(Some(assignment("session-a"))),
            Err(SessionRealizationControlFailure::Unavailable(
                "claim down".into(),
            )),
        ])),
        ..ScriptedControl::default()
    };
    let error =
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(
        matches!(error.error, TerminalRecoveryError::Claim(_)) && error.session_id.is_none(),
        "S5 {error}"
    );
    assert!(
        control.events().contains(&"work:session-a".to_string()),
        "S5 claimed work"
    );
}

#[tokio::test]
async fn s6_s7_settled_assignments_are_revoked_and_the_first_failure_is_kept() {
    // S6: another actor settled the assignment -> its projection is revoked.
    // S7: one Session fails -> every claimed Session still runs and the first
    // failure is returned with its Session.
    let control = ScriptedControl {
        claims: claims(&["session-a", "session-b"]),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections::default();
    assert_eq!(
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap(),
        2
    );
    let events = projections.events();
    assert!(events.contains(&"revoke:session-a".to_string()), "S6");
    assert!(events.contains(&"revoke:session-b".to_string()), "S6");

    let control = ScriptedControl {
        claims: claims(&["session-a", "session-b"]),
        work: Mutex::new(VecDeque::from([
            Err(SessionRealizationControlFailure::Unavailable(
                "control down".into(),
            )),
            Err(SessionRealizationControlFailure::Unavailable(
                "control down".into(),
            )),
        ])),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections::default();
    let error =
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert!(
        error.session_id.is_some(),
        "S7 the failure keeps its Session"
    );
    assert!(
        error
            .to_string()
            .contains("terminal cleanup remained pending"),
        "S7 {error}"
    );
    let events = control.events();
    assert!(events.contains(&"work:session-a".to_string()), "S7");
    assert!(events.contains(&"work:session-b".to_string()), "S7");

    let control = ScriptedControl {
        claims: claims(&["session-a"]),
        ..ScriptedControl::default()
    };
    let projections = RecordingProjections {
        revoke_fails: true,
        ..RecordingProjections::default()
    };
    let error =
        recover_terminal_cleanup_assignments(target(), participants(&control, &projections))
            .await
            .unwrap_err();
    assert_eq!(
        error.session_id,
        Some(SessionId::from("session-a")),
        "S6 a failed revocation is that Session's failure"
    );
    assert!(matches!(error.error, TerminalRecoveryError::Projection(_)));
}
