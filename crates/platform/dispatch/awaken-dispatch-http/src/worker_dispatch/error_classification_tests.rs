// @formal surface exclude=test_only
// @formal owner crates/platform/dispatch/awaken-dispatch-http/src/worker_dispatch.rs
// @formal reason: cfg(test) only; verifies typed Control wire faults, exact status and envelope.
use super::*;
use awaken_session_execution_contract::SessionRealizationControlFailure as C;

#[test]
fn realization_http_preserves_each_cause_status_and_wire_shape() {
    // All nine causes have independent status and golden wire expectations.
    // Misleading identical prose cannot decide status or retry disposition.
    let detail = "offline corrupt conflict not found";
    for (cause, expected, wire) in [
        (
            C::NotFound,
            StatusCode::NOT_FOUND,
            json!({"kind": "not_found"}),
        ),
        (
            C::NotReady,
            StatusCode::CONFLICT,
            json!({"kind": "not_ready"}),
        ),
        (C::Retired, StatusCode::CONFLICT, json!({"kind": "retired"})),
        (
            C::Terminal,
            StatusCode::CONFLICT,
            json!({"kind": "terminal"}),
        ),
        (
            C::StaleOwnership,
            StatusCode::CONFLICT,
            json!({"kind": "stale_ownership"}),
        ),
        (
            C::Conflict,
            StatusCode::CONFLICT,
            json!({"kind": "conflict"}),
        ),
        (
            C::Invalid(detail.into()),
            StatusCode::BAD_REQUEST,
            json!({"kind": "invalid", "detail": detail}),
        ),
        (
            C::Unavailable(detail.into()),
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"kind": "unavailable", "detail": detail}),
        ),
        (
            C::Internal(detail.into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"kind": "internal", "detail": detail}),
        ),
    ] {
        let message = cause.to_string();
        let (status, Json(value)) =
            respond_realization(Err(RealizationHttpError::Control(cause.clone())));
        assert_eq!(status, expected, "{cause:?}");
        assert_eq!(value, json!({"error": message, "realization_error": wire}));
        assert_eq!(
            serde_json::from_value::<C>(value["realization_error"].clone()).unwrap(),
            cause
        );
    }
    let success = json!({"original": [1, "data"]});
    let (status, Json(value)) = respond_realization(Ok(success.clone()));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value, success);
}
