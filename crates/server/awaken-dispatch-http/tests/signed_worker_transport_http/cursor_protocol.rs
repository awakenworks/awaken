//! Cursor version and progression checks on the existing signed Worker route.

use super::*;

pub(super) async fn verify_claim_cursor_protocol(
    upstream: &WorkerUpstream,
    address: std::net::SocketAddr,
    identity: &WorkerIdentity,
    target: &awaken_session_realization_contract::SessionRealizationTarget,
    after: &SessionId,
    control: &Arc<RecordingSessionControl>,
) {
    // C0 excessive requested expiry, C1 current Worker plus an exclusive
    // cursor, C2 missing/wrong cursor version. R0 C0 -> cap to Registry proof;
    // R1 C1 -> only scheduling input reaches Control; R2 C2 -> 422/400 before
    // Control. Neither cursor nor version grants a lease.
    let mut over_expiry = target.clone();
    over_expiry.lease_expires_at_unix_ms = 40_001;
    WorkerControlClient::new(upstream.clone())
        .claim_next_terminal_cleanup(identity, over_expiry, None)
        .await
        .expect("R0 exact proposal is narrowed")
        .expect("R0 typed assignment");
    assert_eq!(control.cleanup_claims.lock().unwrap().len(), 2, "R0");
    assert_eq!(
        control.cleanup_claims.lock().unwrap()[1].lease_expires_at_unix_ms,
        40_000,
        "R0 Registry authority caps the Session claim"
    );
    WorkerControlClient::new(upstream.clone())
        .claim_next_terminal_cleanup(identity, target.clone(), Some(after))
        .await
        .expect("R1 exact cursor reaches Control");
    assert_eq!(
        control.cleanup_after.lock().unwrap()[2],
        Some(after.clone()),
        "R1 cursor is scheduling input only"
    );
    let claim_path = "/v1/worker/session/cleanup/claim-next";
    for (rule, body, expected_status) in [
        (
            "R2 missing version",
            serde_json::json!({
                "identity": identity,
                "target": target,
                "after_session_id": null,
            }),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "R2 unsupported version",
            serde_json::json!({
                "identity": identity,
                "target": target,
                "sweep_cursor_version": 0,
                "after_session_id": null,
            }),
            reqwest::StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = upstream
            .authorize(
                "POST",
                claim_path,
                upstream
                    .client()
                    .post(format!("http://{address}{claim_path}")),
            )
            .expect("R2 signed claim probe")
            .json(&body)
            .send()
            .await
            .expect("R2 route response");
        assert_eq!(response.status(), expected_status, "{rule}");
    }
    assert_eq!(
        control.cleanup_claims.lock().unwrap().len(),
        3,
        "R2 malformed cursor version cannot reach Control"
    );
}
