use super::*;

// R1 a staged route's original expiry passes, but its same-generation Session
// root remains live => forward through the original capability, no restaging.
// R2 a foreign incarnation/epoch asserts even a future expiry => reject.
// R3 current root expires => reject; R4 Host disappears => reject. All failures
// R5 invalidation / slot removal => reject even a still-live route assertion.
// must leave the real upstream call count unchanged. Canonical drain/cancel
// coverage remains in mcp_relay's existing shared-call-fence tests.
#[tokio::test]
async fn relay_uses_current_session_authority_without_restaging_its_route() {
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let session = "renewed-relay";
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    host.install_session_realization_lease(session, lease.clone())
        .unwrap();
    let relay = host
        .mcp
        .start_relay(Arc::downgrade(&host) as _)
        .await
        .unwrap();
    let (upstream, seen) = crate::test_mcp::start(Some("Bearer test-relay-secret")).await;
    let generation = crate::test_mcp::mcp_fence(
        session,
        "browser",
        1,
        lease.runtime_incarnation.clone(),
        lease.epoch,
        1,
    );
    let material = awaken_session_mcp::McpTransportMaterial {
        name: "browser".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: upstream,
            bearer: Some(awaken_agent_contract::RedactedString::new(
                "test-relay-secret",
            )),
            refresh: None,
        },
    };
    assert!(relay.stage_route(&generation, &material));
    let url = relay.route_url(&generation).unwrap();
    let client = reqwest::Client::new();
    let invoke = |url: String| {
        client
            .post(url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            }))
            .send()
    };
    assert_eq!(
        invoke(url.clone()).await.unwrap().status(),
        reqwest::StatusCode::OK,
        "R1"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    for incarnation in [false, true] {
        let mut foreign = generation.clone();
        foreign = foreign
            .with_persisted(|w| {
                w.generation = awaken_session_mcp_contract::McpGeneration::new(
                    awaken_session_mcp_contract::McpAttachmentId(format!("foreign-{incarnation}")),
                    foreign.generation().version(),
                    foreign.generation().digest().clone(),
                )
            })
            .unwrap();
        foreign = foreign.expiring_at(u64::MAX);
        if incarnation {
            foreign = foreign
                .with_persisted(|w| w.runtime_incarnation = "other/1".into())
                .unwrap();
        } else {
            foreign = foreign.with_persisted(|w| w.lease_epoch += 1).unwrap();
        }
        assert!(relay.stage_route(&foreign, &material));
        assert_eq!(
            invoke(relay.route_url(&foreign).unwrap())
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND,
            "R2"
        );
    }
    host.installed_sessions.update_realization(session, |r| {
        let mut expired = lease.clone();
        expired.expires_at_unix_ms = 1;
        r.authority = crate::installed_session::SessionRealizationAuthority::Live(
            crate::installed_session::test_expired_observed_lease(expired),
        );
    });
    assert_eq!(
        invoke(url.clone()).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "R3"
    );
    let mut live = generation.clone();
    live = live
        .with_persisted(|w| {
            w.generation = awaken_session_mcp_contract::McpGeneration::new(
                awaken_session_mcp_contract::McpAttachmentId("live-assertion".into()),
                live.generation().version(),
                live.generation().digest().clone(),
            )
        })
        .unwrap();
    live = live.expiring_at(u64::MAX);
    assert!(relay.stage_route(&live, &material));
    for remove in [false, true] {
        // Independent R5 allocations: a closed/expired owner cannot be revived
        // by reinstallation. Retain the relay route to test its stale authority.
        host.installed_sessions.remove(session);
        host.install_session_realization_lease(session, lease.clone())
            .unwrap();
        if remove {
            host.installed_sessions.remove(session);
        } else {
            host.invalidate_session_realization_lease(session, &lease);
        }
        assert_eq!(
            invoke(relay.route_url(&live).unwrap())
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND,
            "R5 missing authority cannot be replaced by a route's old assertion"
        );
    }
    drop(host);
    assert_eq!(
        invoke(url).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "R4"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "no unauthorized upstream effects"
    );
}

/// A relay route is keyed by (Session, attachment, version) because that is
/// the path a sandboxed CLI dials, but it is *owned* by one lease tenure. A
/// drain, publication or call-fence lookup asserted under any other tenure of
/// the same key must not touch the route, or a predecessor's late drain in
/// this process removes or re-points its successor's route.
///
/// Cause: the tenure the caller asserts against a route staged at
/// (incarnation `worker/1`, epoch 3, expiry 100).
/// | Rule | asserted tenure / expiry            | route_call_fence | update_staged_route | remove_route |
/// |------|-------------------------------------|------------------|---------------------|--------------|
/// | T1   | predecessor epoch 2                 | None             | false               | keeps route  |
/// | T2   | foreign incarnation, same epoch     | None             | false               | keeps route  |
/// | T3   | successor epoch 4                   | None             | false               | keeps route  |
/// | T4   | exact tenure, shorter expiry 50     | Some             | false               | -            |
/// | T5   | exact tenure, renewed expiry 200    | Some             | true                | removes      |
#[tokio::test]
async fn only_the_exact_route_tenure_may_fence_update_or_remove_a_relay_route() {
    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    let staged = crate::test_mcp::mcp_fence("tenure-relay", "browser", 1, "worker/1", 3, 100);
    let material = awaken_session_mcp::McpTransportMaterial {
        name: "browser".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: "https://fixture.test/mcp".into(),
            bearer: None,
            refresh: None,
        },
    };
    assert!(relay.stage_route(&staged, &material), "setup");
    let staged_url = relay.route_url(&staged).expect("setup route");
    let with = |incarnation: &str, epoch: u64, expiry: u64| {
        let mut asserted = staged.clone();
        asserted = asserted
            .with_persisted(|w| w.runtime_incarnation = incarnation.into())
            .unwrap();
        asserted = asserted.with_persisted(|w| w.lease_epoch = epoch).unwrap();
        asserted = asserted.expiring_at(expiry);
        asserted
    };
    for (rule, asserted) in [
        ("T1", with("worker/1", 2, 100)),
        ("T2", with("worker/2", 3, 100)),
        ("T3", with("worker/1", 4, 100)),
    ] {
        assert!(relay.route_call_fence(&asserted).is_none(), "{rule} fence");
        assert!(
            !relay.update_staged_route(&asserted, &material),
            "{rule} update"
        );
        relay.remove_route(&asserted);
        assert_eq!(
            relay.route_url(&staged).as_deref(),
            Some(staged_url.as_str()),
            "{rule} the owner's route survives"
        );
    }
    let shorter = with("worker/1", 3, 50);
    assert!(relay.route_call_fence(&shorter).is_some(), "T4 fence");
    assert!(!relay.update_staged_route(&shorter, &material), "T4 update");
    let renewed = with("worker/1", 3, 200);
    assert!(relay.route_call_fence(&renewed).is_some(), "T5 fence");
    assert!(relay.update_staged_route(&renewed, &material), "T5 update");
    relay.remove_route(&renewed);
    assert!(relay.route_url(&staged).is_none(), "T5 removed");
}
