//! Real signed TCP transport: typed faults survive server status and client decode.
use super::*;
use awaken_session_execution_contract::{
    RenewSessionRealization, SessionRealizationControlDisposition as D,
    SessionRealizationControlFailure as C,
};

pub(super) async fn assert_renewal_failure_classes(
    client: &WorkerControlClient,
    control: &RecordingSessionControl,
    identity: &WorkerIdentity,
    renewal: RenewSessionRealization,
) {
    // The unit HTTP table verifies golden status/envelopes; this complementary
    // socket test catches a client that flattens 400/404/409/500 into retryable
    // Unavailable. Every request is signed and passes the real lease fences.
    let before = control.renewals.lock().unwrap().len();
    let detail = "offline corrupt conflict not found";
    for (cause, disposition, owner_lost) in [
        (C::NotFound, D::Terminal, true),
        (C::NotReady, D::NotReady, false),
        (C::Retired, D::Terminal, true),
        (C::Terminal, D::Terminal, true),
        (C::StaleOwnership, D::Retryable, true),
        (C::Conflict, D::Retryable, false),
        (C::Invalid(detail.into()), D::Terminal, true),
        (C::Unavailable(detail.into()), D::Retryable, false),
        (C::Internal(detail.into()), D::Terminal, true),
    ] {
        *control.renewal_failure.lock().unwrap() = Some(cause.clone());
        let error = client
            .renew_session_realization(identity, renewal.clone())
            .await
            .unwrap_err();
        assert_eq!(error, cause, "exact typed cause over HTTP");
        assert_eq!(error.disposition(), disposition);
        assert_eq!(
            error.proves_current_realization_cannot_continue(),
            owner_lost
        );
    }
    *control.renewal_failure.lock().unwrap() = None;
    assert_eq!(
        control.renewals.lock().unwrap().len(),
        before + 9,
        "one Control call per request, no transport retry"
    );
}
