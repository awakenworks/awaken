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
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let session = "renewed-relay";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    host.install_session_realization_lease(session, lease.clone())
        .unwrap();
    let relay = crate::mcp_relay::McpRelay::for_host(Arc::downgrade(&host))
        .await
        .unwrap();
    let (upstream, seen) = crate::test_mcp::start(Some("Bearer test-relay-secret")).await;
    let generation = awaken_session_contract::McpGenerationRef {
        session_id: session.into(),
        generation: awaken_session_contract::McpGeneration::of_bare_target(
            awaken_session_contract::McpAttachmentId("browser".into()),
            1,
            &awaken_session_contract::McpTarget::parse_http("https://fixture.test/mcp").unwrap(),
        ),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_epoch: lease.epoch,
        lease_expires_at_unix_ms: 1,
    };
    let material = crate::mcp::McpTransportMaterial {
        name: "browser".into(),
        prompts_as_skills: false,
        transport: crate::mcp::McpTransportMaterialKind::Http {
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
        foreign.generation = awaken_session_contract::McpGeneration::new(
            awaken_session_contract::McpAttachmentId(format!("foreign-{incarnation}")),
            foreign.generation.version(),
            foreign.generation.digest().clone(),
        );
        foreign.lease_expires_at_unix_ms = u64::MAX;
        if incarnation {
            foreign.runtime_incarnation = "other/1".into();
        } else {
            foreign.lease_epoch += 1;
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
    host.session_slots.update(session, |slot| {
        let mut expired = lease.clone();
        expired.expires_at_unix_ms = 1;
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_expired_observed_lease(expired),
        );
    });
    assert_eq!(
        invoke(url.clone()).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "R3"
    );
    let mut live = generation.clone();
    live.generation = awaken_session_contract::McpGeneration::new(
        awaken_session_contract::McpAttachmentId("live-assertion".into()),
        live.generation.version(),
        live.generation.digest().clone(),
    );
    live.lease_expires_at_unix_ms = u64::MAX;
    assert!(relay.stage_route(&live, &material));
    for remove in [false, true] {
        // Independent R5 allocations: a closed/expired owner cannot be revived
        // by reinstallation. Retain the relay route to test its stale authority.
        host.session_slots.remove(session);
        host.install_session_realization_lease(session, lease.clone())
            .unwrap();
        if remove {
            host.session_slots.remove(session);
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
