// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Test-only regression observations of the original component and its effects.
// @formal reason: This classifies the assertions; it is not a formal-proof claim.
//! Real listener ownership; a route or liveness snapshot is not a server owner.

use super::*;

#[tokio::test]
async fn last_mcp_relay_owner_drop_releases_the_original_listener() {
    assert_listener_lifetime(false).await;
}

#[tokio::test]
async fn mcp_relay_clones_retain_one_listener_until_the_last_owner_drops() {
    assert_listener_lifetime(true).await;
}

async fn assert_listener_lifetime(retain_clone: bool) {
    // L1 one owner, L2 an explicit SDK clone. Both bind the production relay's
    // real ephemeral TCP listener. L2 keeps the same listener live after the
    // first owner leaves. After the last owner, require actual TCP refusal and
    // the original task's terminal liveness observation, not a DTO flag alone.
    let relay = McpRelay::start_with_authority_and_exposure(
        Arc::new(|_, _| true),
        None,
        awaken_time::system_clock(),
        None,
    )
    .await
    .expect("bind original MCP relay");
    let address = relay
        .advertised_origin
        .strip_prefix("http://")
        .expect("loopback relay origin")
        .to_owned();
    let alive = relay.alive.clone();
    // Existing test-only exact-task cleanup is retained even for the broken
    // implementation. It runs only after measuring last-owner behavior.
    let original_task_abort = relay.server_abort.clone();
    let retained = retain_clone.then(|| relay.clone());
    drop(
        tokio::net::TcpStream::connect(&address)
            .await
            .expect("original listener is real"),
    );
    drop(relay);
    if retain_clone {
        assert!(
            alive.load(Ordering::Acquire),
            "L2 explicit SDK owner remains"
        );
        drop(
            tokio::net::TcpStream::connect(&address)
                .await
                .expect("L2 same original listener remains"),
        );
    }
    drop(retained);
    // The owner's original task must finish within the lifecycle deadline.
    // TCP refusal is a separate OS observation: on Windows it was measured
    // at 2039/2058 ms, beyond that two-second combined observation budget.
    // Keep lifecycle's two seconds and use the existing network deadline for
    // its own probe. Neither a timeout nor a liveness flag proves port closure.
    let task_closed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while alive.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    let physically_closed = matches!(
        tokio::time::timeout(
            RELAY_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(&address)
        )
        .await,
        Ok(Err(_))
    );
    let released_before_test_cleanup = task_closed && physically_closed;
    original_task_abort.abort();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while alive.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exact test-owned server task cleaned up");
    assert!(
        released_before_test_cleanup,
        "last MCP relay owner drop left the original physical listener running"
    );
}

#[tokio::test]
async fn explicit_relay_close_joins_one_listener_and_replays_for_every_clone() {
    let relay = McpRelay::start().await.expect("real relay");
    let retained = relay.clone();
    let address = relay
        .advertised_origin
        .strip_prefix("http://")
        .unwrap()
        .to_owned();
    drop(
        tokio::net::TcpStream::connect(&address)
            .await
            .expect("bound listener"),
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), relay.close_and_wait())
        .await
        .expect("bounded local join")
        .expect("joined original server");
    assert!(!relay.is_live() && !retained.is_live());
    assert!(!relay.is_accepting() && !retained.is_accepting());
    assert!(
        tokio::net::TcpStream::connect(&address).await.is_err(),
        "physical listener released"
    );
    retained
        .close_and_wait()
        .await
        .expect("same successful join replays");
}

#[tokio::test]
async fn relay_close_retains_original_route_drain_after_waiter_cancellation() {
    let relay = McpRelay::start().await.expect("real relay");
    let address = relay
        .advertised_origin
        .strip_prefix("http://")
        .unwrap()
        .to_owned();
    let generation =
        crate::test_mcp::mcp_fence("relay-close", "original", 1, "runtime", 1, u64::MAX);
    let material = McpTransportMaterial {
        name: "original".into(),
        prompts_as_skills: false,
        transport: crate::mcp_material::McpTransportMaterialKind::Http {
            url: "http://127.0.0.1:9".into(),
            bearer: None,
            refresh: None,
        },
    };
    assert!(relay.stage_route(&generation, &material));
    let fence = relay
        .route_call_fence(&generation)
        .expect("original route fence");
    let permit = fence.try_enter().expect("original admitted local activity");
    let waiter = tokio::spawn({
        let relay = relay.clone();
        async move { relay.close_and_wait().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), permit.cancelled())
        .await
        .expect("physical server drives the original route drain");
    assert!(
        !relay.is_accepting(),
        "close hides new admission immediately"
    );
    let late = crate::test_mcp::mcp_fence("relay-close", "late", 2, "runtime", 1, u64::MAX);
    assert!(
        !relay.stage_route(&late, &material),
        "closed instance cannot create another route"
    );
    assert!(
        !waiter.is_finished(),
        "held original permit is not a stop receipt"
    );
    assert!(
        relay.is_live(),
        "local server remains pending until original activity returns"
    );
    waiter.abort();
    assert!(
        waiter
            .await
            .expect_err("cancel only this waiting caller")
            .is_cancelled()
    );
    assert!(
        fence.try_enter().is_none(),
        "caller cancellation cannot reopen admission"
    );
    drop(permit);
    tokio::time::timeout(std::time::Duration::from_secs(2), relay.close_and_wait())
        .await
        .expect("next caller resumes original shutdown")
        .expect("original task joined");
    assert!(!relay.is_live());
    assert!(tokio::net::TcpStream::connect(&address).await.is_err());
    assert!(
        relay.route_call_fence(&generation).is_none(),
        "only joined drain removes owned routes"
    );
    relay
        .close_and_wait()
        .await
        .expect("completed result remains idempotent");
}
