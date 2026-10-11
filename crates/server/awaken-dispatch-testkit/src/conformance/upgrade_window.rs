// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Static owner: the upgrade-window replay rule of the shared dispatch conformance. Dynamic
// @formal reason: path: the tooling-only testkit includes this file for every store backend; it is absent
// @formal reason: from the product runtime graph. Evidence: crates/server/awaken-dispatch-testkit/src/lib.rs.
/// A frozen Session runtime projection with no MCP attachments, as both the
/// build before typed MCP credential requirements and the current build
/// write it. The current build additionally publishes `Some([])` beside it.
fn runtime_projection_json(environment_id: &str) -> String {
    serde_json::json!({
        "environment": {
            "environment_id": environment_id,
            "revision": 1,
            "self_hosted": false,
            "config_fingerprint": format!("{environment_id}@1"),
            "sandbox": {"requests": {}, "limits": {}},
            "sandbox_provisioning": "eager",
            "idle_retention": {
                "mode": "resident",
                "checkpoint_after_secs": 0,
                "retention_secs": 0,
                "expiry_behavior": "fresh_from_frozen_environment",
                "max_checkpoint_bytes": 0,
                "max_checkpoint_duration_secs": 0,
                "checkpoint_format": ""
            },
            "packages": {"type": "packages", "apt": [], "cargo": [], "gem": [], "go": [], "npm": [], "pip": []},
            "network": {"mode": "unrestricted"},
            "credential_realization": {
                "inference_holder": {"boundary": "worker", "trust_domain": "worker"},
                "mcp_holder": {"boundary": "worker", "trust_domain": "worker"},
                "resource_holder": {"boundary": "worker", "trust_domain": "worker"}
            }
        },
        "tools": null,
        "mcp_stages": []
    })
    .to_string()
}

/// One full-dispatch row carrying a runtime projection, either in the form a
/// build before typed MCP requirements wrote (`published == false`) or in the
/// current form, which publishes the requirements derived from the same
/// projection.
fn projected_dispatch(ns: &str, run: &str, environment_id: &str, published: bool) -> RunDispatch {
    let mut request = dispatch(ns, run, &format!("{run}-thread")).with_session_runtime(
        awaken_dispatch_contract::SessionRuntimeEnvelope::new(runtime_projection_json(
            environment_id,
        )),
    );
    if published {
        request.placement.session_mcp_credentials = Some(Vec::new());
    }
    request
}

/// Upgrade-window replay of caller-owned full-dispatch identity.
///
/// A row stored before the Session published typed MCP credential
/// requirements omits that field; a retry of the same command by the current
/// build carries it. The comparison normalizes the retry, never the stored
/// bytes.
///
/// | Rule | Stored | Incoming retry | Effect |
/// |---|---|---|---|
/// | U1 | legacy live | current, same command | replay; the stored row keeps no requirements |
/// | U2 | current live | current, same command | replay |
/// | U3 | legacy live | legacy, same command | replay |
/// | U4 | legacy live | current, different projection | conflict |
/// | U5 | current live | legacy form of the same command | conflict: an older writer cannot reproduce the published requirements |
/// | U6 | legacy tombstone | current, same command | replay |
/// | U7 | legacy tombstone | current, different projection | conflict |
async fn upgrade_window_retry_replays_rows_written_before_typed_requirements(
    store: &dyn ConformanceDispatch,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    let legacy = projected_dispatch(ns, "upgrade-legacy", "environment-a", false);
    store.enqueue(legacy.clone()).await.expect("U1 legacy row");
    store
        .enqueue(projected_dispatch(
            ns,
            "upgrade-legacy",
            "environment-a",
            true,
        ))
        .await
        .expect("U1 current retry replays the legacy row");
    store
        .enqueue(legacy.clone())
        .await
        .expect("U3 legacy retry replays the legacy row");
    assert!(
        store
            .enqueue(projected_dispatch(
                ns,
                "upgrade-legacy",
                "environment-b",
                true
            ))
            .await
            .is_err(),
        "U4 a different projection is another command"
    );

    let current = projected_dispatch(ns, "upgrade-current", "environment-a", true);
    store
        .enqueue(current.clone())
        .await
        .expect("U2 current row");
    store
        .enqueue(current.clone())
        .await
        .expect("U2 current retry replays the current row");
    assert!(
        store
            .enqueue(projected_dispatch(
                ns,
                "upgrade-current",
                "environment-a",
                false
            ))
            .await
            .is_err(),
        "U5 the legacy form does not replay a current row"
    );

    clock.set(95_000);
    let claim = store
        .claim_new_run(
            legacy.clone(),
            "upgrade-worker",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .expect("U1 claim the legacy row")
        .expect("U1 the legacy row is claimable");
    assert_eq!(
        claim.request.placement.session_mcp_credentials, None,
        "U1 the stored legacy bytes were not rewritten"
    );
    settle_done(store, &claim).await;
    store
        .enqueue(projected_dispatch(
            ns,
            "upgrade-legacy",
            "environment-a",
            true,
        ))
        .await
        .expect("U6 current retry replays the legacy tombstone");
    assert!(
        store
            .enqueue(projected_dispatch(
                ns,
                "upgrade-legacy",
                "environment-b",
                true
            ))
            .await
            .is_err(),
        "U7 a different projection conflicts with the legacy tombstone"
    );

    // Leave no runnable row behind for the later broad-claim rules.
    let claim = store
        .claim_new_run(current, "upgrade-worker", LEASE_MS, &Default::default())
        .await
        .expect("U2 claim the current row")
        .expect("U2 the current row is claimable");
    assert_eq!(
        claim.request.placement.session_mcp_credentials,
        Some(Vec::new()),
        "U2 the current row keeps its published requirements"
    );
    settle_done(store, &claim).await;
}
