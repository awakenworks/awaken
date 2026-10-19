use super::*;
use awaken_session_realization_contract::{
    RenewSessionRealization, SessionRealizationControlFailure as ControlError,
    SessionRealizationLeaseReceipt,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct ReportingControl {
    delay: Duration,
    fail: bool,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    entered: tokio::sync::Notify,
    reports: Mutex<Vec<FailSessionRealization>>,
}

#[async_trait::async_trait]
impl SessionRealizationControl for ReportingControl {
    async fn begin_session_realization(
        &self,
        _: awaken_session_realization_contract::BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, ControlError> {
        unreachable!("reporting never begins")
    }
    async fn renew_session_realization(
        &self,
        _: RenewSessionRealization,
    ) -> Result<SessionRealizationLeaseReceipt, ControlError> {
        unreachable!("reporting never renews")
    }
    async fn activate_session_realization(
        &self,
        _: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, ControlError> {
        unreachable!("a failed effect never activates")
    }
    async fn acknowledge_session_realization(
        &self,
        _: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, ControlError> {
        unreachable!("a failed effect never acknowledges")
    }
    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), ControlError> {
        self.reports.lock().unwrap().push(command);
        self.entered.notify_one();
        if let Some(gate) = &self.gate {
            let _permit = gate.acquire().await.unwrap();
        }
        tokio::time::sleep(self.delay).await;
        if self.fail {
            Err(ControlError::Unavailable("report failed".into()))
        } else {
            Ok(())
        }
    }
}

struct InvalidatingProjection {
    delay: Duration,
    fail: bool,
    invalidated: Mutex<Vec<(SessionId, SessionRealizationLease)>>,
}

#[async_trait::async_trait]
impl SessionProjectionSynchronizer for InvalidatingProjection {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        SessionRealizationEffectFailure,
    > {
        unreachable!("reporting samples no new authority")
    }
    async fn synchronize_session_projection(
        &self,
        _: &SessionId,
        _: &awaken_session_realization_contract::FrozenSessionProjection,
        _: &awaken_session_realization_contract::ObservedSessionRealizationLease,
        _: bool,
    ) -> Result<(), SessionRealizationEffectFailure> {
        unreachable!("reporting starts no new effect")
    }
    async fn invalidate_session_realization_lease(
        &self,
        session: &SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<(), SessionRealizationEffectFailure> {
        self.invalidated
            .lock()
            .unwrap()
            .push((session.clone(), lease.clone()));
        tokio::time::sleep(self.delay).await;
        if self.fail {
            Err(RunError::internal("invalidation failed").into())
        } else {
            Ok(())
        }
    }
}

fn lease() -> SessionRealizationLease {
    SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "boot".into(),
        epoch: 3,
        expires_at_unix_ms: 10_000,
    }
}

#[tokio::test(start_paused = true)]
async fn original_failure_survives_reporting_and_only_its_hint_pays_the_wait() {
    // Cause/effect decision table D1-D4. C1 original Provider hint is absent,
    // live or already zero; C2 reporting+invalidation wait is below/equal/above
    // the hint; C3 either callback succeeds or fails. D1=None=>None;
    // D2=Some(h)+wait<h=>Some(h-wait); D3=wait>=h=>Some(0); D4 every C3
    // preserves the original error and exact report/lease, never replacing it
    // with a Control failure. One existing helper owns all three effect arms.
    for hint in [None, Some(Duration::ZERO), Some(Duration::from_secs(5))] {
        for (report_delay, invalidate_delay) in [(0, 0), (2, 1), (2, 3), (3, 3)] {
            for (report_fails, invalidate_fails) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let control = ReportingControl {
                    delay: Duration::from_secs(report_delay),
                    fail: report_fails,
                    gate: None,
                    entered: tokio::sync::Notify::new(),
                    reports: Mutex::new(Vec::new()),
                };
                let projections = InvalidatingProjection {
                    delay: Duration::from_secs(invalidate_delay),
                    fail: invalidate_fails,
                    invalidated: Mutex::new(Vec::new()),
                };
                let session = SessionId::from("session");
                let source = awaken_agent_contract::agent::run::Id("run".into());
                let original = RunError::unavailable_classified(
                    awaken_session_realization_contract::ENVIRONMENT_PROVISIONING_CODE,
                    "original effect",
                );
                let reporting = awaken_time::mono::Stopwatch::start();
                let result = fail_realization_effect(
                    &session,
                    Some(&source),
                    &control,
                    &projections,
                    &lease(),
                    Some(7),
                    SessionRealizationEffectFailure::new(original, hint),
                )
                .await;
                let SessionRealizationDriveError::Effect(effect) = result else {
                    panic!("D4 must retain original effect");
                };
                assert_eq!(
                    effect.retry_after(),
                    hint.map(|h| h.saturating_sub(reporting.elapsed())),
                    "D1-D3"
                );
                assert_eq!(
                    effect.run_error().message,
                    "original effect",
                    "D4 original detail"
                );
                assert_eq!(
                    effect.run_error().code,
                    awaken_session_realization_contract::ENVIRONMENT_PROVISIONING_CODE,
                    "D4 original code"
                );
                let reports = control.reports.lock().unwrap();
                assert_eq!(reports.len(), 1, "D4 one original report");
                assert_eq!(reports[0].session_id, session, "D4");
                assert_eq!(reports[0].lease, lease(), "D4");
                assert_eq!(reports[0].prepared_resource_revision, Some(7), "D4");
                assert_eq!(reports[0].source_run_id.as_ref(), Some(&source), "D4");
                assert_eq!(
                    (reports[0].retryable, reports[0].not_ready),
                    (true, true),
                    "D4 classification unchanged"
                );
                assert_eq!(
                    projections.invalidated.lock().unwrap().as_slice(),
                    [(session, lease())],
                    "D4 exact invalidation"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_failure_reporting_drops_the_original_wait_without_detached_effects() {
    // D5: Control's original failure request is in flight, then its caller is
    // cancelled. Effect: the owned reporting future is dropped; no detached
    // invalidation or new effect happens after the caller joins cancellation,
    // even if the original gate later opens. Durable recovery remains the
    // original Control/lease protocol, not a local completion claim.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let control = Arc::new(ReportingControl {
        delay: Duration::ZERO,
        fail: false,
        gate: Some(gate.clone()),
        entered: tokio::sync::Notify::new(),
        reports: Mutex::new(Vec::new()),
    });
    let projections = Arc::new(InvalidatingProjection {
        delay: Duration::ZERO,
        fail: false,
        invalidated: Mutex::new(Vec::new()),
    });
    let task_control = control.clone();
    let task_projection = projections.clone();
    let task = tokio::spawn(async move {
        fail_realization_effect(
            &SessionId::from("session"),
            None,
            task_control.as_ref(),
            task_projection.as_ref(),
            &lease(),
            None,
            SessionRealizationEffectFailure::new(
                RunError::unavailable("original"),
                Some(Duration::from_secs(5)),
            ),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), control.entered.notified())
        .await
        .expect("D5 original request enters its gate");
    task.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("D5 cancellation joins")
            .unwrap_err()
            .is_cancelled()
    );
    gate.add_permits(1);
    tokio::task::yield_now().await;
    assert_eq!(
        control.reports.lock().unwrap().len(),
        1,
        "D5 original attempt retained"
    );
    assert!(
        projections.invalidated.lock().unwrap().is_empty(),
        "D5 no detached invalidation"
    );
}
