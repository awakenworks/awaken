//! Claimed-dispatch MCP projection tests kept beside the resolver behavior.

use super::test_support::{
    AdoptionModel, deferred_environment, test_activation, with_empty_session_resources,
};
use super::*;

#[tokio::test]
async fn dispatched_mcp_effect_projection_decision_table() {
    // Cause/effect graph: C1 envelope MCP field absent/present; C2 exact
    // generation valid/foreign/expired; C3 desired set same/empty/replaced.
    // Effects: E1 legacy absence leaves process state untouched (owned by
    // the decoder test above); E2 valid exact input uses the canonical
    // stage+publish implementation; E3 replay is idempotent; E4 explicit
    // empty drains the old projection; E5 malformed/failed input performs
    // no replacement and fails the claimed Run closed.
    //
    // | Rule | field | generation | desired | effect |
    // | R1 | absent | n/a | n/a | legacy/no mutation |
    // | R2 | present | exact/live | add | one active projection |
    // | R3 | present | exact/live | same | idempotent one |
    // | R4 | present | n/a | empty | drain all |
    // | R5 | present | foreign/expired | replace | reject, retain prior |
    let legacy = awaken_dispatch_contract::SessionRuntimeEnvelope::new(
        serde_json::json!({
            "environment": deferred_environment(),
            "toolsets": null
        })
        .to_string(),
    );
    assert!(
        awaken_session_configuration_contract::SessionRuntimeProjection::decode(
            &legacy.projection_json
        )
        .expect("R1 legacy envelope")
        .mcp_stages
        .is_none(),
        "R1"
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let _managed = crate::ManagedHost::new(host.clone());
    host.run_assembly
        .register_thread_agent_projection("mcp-dispatch", "agent-a");
    host.run_assembly
        .register_thread_backend_projection("mcp-dispatch", "acp:gemini");
    host.run_assembly
        .install_environment_projection("mcp-dispatch", &deferred_environment())
        .expect("install frozen Environment");

    let request =
        |session: &str, attachment: &str, expiry: u64| crate::session_mcp::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: crate::test_mcp::mcp_fence(
                session,
                attachment,
                1,
                "control-runtime",
                1,
                expiry,
            ),
            realization_id: format!("realize-{attachment}"),
            stage_idempotency_key: format!("stage-{attachment}"),
            name: attachment.into(),
            target: awaken_agent_contract::McpTarget::parse_http(format!(
                "https://{attachment}.example.test/mcp"
            ))
            .expect("HTTP target"),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
    let exact = request("mcp-dispatch", "docs", u64::MAX);

    awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(
        &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
        "mcp-dispatch",
        vec![exact.clone()],
    )
    .await
    .expect("R2");
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-dispatch")
            .len(),
        1,
        "R2"
    );
    awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(
        &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
        "mcp-dispatch",
        vec![exact.clone()],
    )
    .await
    .expect("R3");
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-dispatch")
            .len(),
        1,
        "R3"
    );

    awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(
        &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
        "mcp-dispatch",
        Vec::new(),
    )
    .await
    .expect("R4");
    assert!(
        host.run_assembly
            .active_mcp_projections("mcp-dispatch")
            .is_empty(),
        "R4"
    );

    let retained = request("mcp-dispatch", "retained", u64::MAX);
    awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(
        &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
        "mcp-dispatch",
        vec![retained],
    )
    .await
    .expect("R5 setup");
    let foreign = request("foreign-session", "replacement", u64::MAX);
    assert!(
        awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(&crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
            "mcp-dispatch",
            vec![foreign],
        )
        .await
        .is_err(),
        "R5 foreign"
    );
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-dispatch")
            .len(),
        1,
        "R5"
    );
    let expired = request("mcp-dispatch", "replacement", 0);
    assert!(
        awaken_dispatch_execution::session_worker_resolver::claimed_dispatch::reconcile_dispatched_mcp(&crate::session_effects::RunMcpEffects::new(host.run_assembly.clone()),
            "mcp-dispatch",
            vec![expired],
        )
        .await
        .is_err(),
        "R5 expired"
    );
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-dispatch")
            .len(),
        1,
        "R5"
    );
}

#[tokio::test]
async fn undecodable_dispatched_runtime_projection_is_an_absorbing_resolution_failure() {
    // Cause/effect table: C1 the claim admitted the row on the Session's
    // published MCP requirements, so claim admission never decoded the
    // envelope; C2 the Worker's full decode of that envelope fails.
    // E1 resolution fails as a typed terminal resolution, so the claimed Run
    // settles as failed instead of being retried against the same bytes; E2
    // the failure names the invalid projection.
    use awaken_dispatch::WorkerResolver as _;

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock)
            .await
            .expect("dispatch store"),
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_ephemeral_dispatch(store.clone()))
    .build()
    .expect("validated Host dependencies");
    let mut request = with_empty_session_resources(
        awaken_dispatch_contract::RunDispatch::new(test_activation(
            "corrupt-projection",
            "run-corrupt-projection",
        ))
        .with_session_runtime(awaken_dispatch_contract::SessionRuntimeEnvelope::new(
            "{",
        )),
        host.run_assembly.local_workspace(),
    );
    request.placement.session_mcp_credentials = Some(Vec::new());
    store.enqueue(request).await.expect("enqueue");
    let claimed = store
        .claim("worker-a", 1_000, &Default::default())
        .await
        .expect("claim")
        .expect("C1 the published requirements admit the claim");
    let error = match (crate::host::session_worker_resolver(&host).expect("Host dispatch resolver"))
        .worker_for_claimed(&claimed)
        .await
    {
        Ok(_) => panic!("C2 an undecodable projection must not execute"),
        Err(error) => error,
    };
    assert!(error.is_terminal_resolution(), "E1: {error}");
    assert!(
        error
            .to_string()
            .contains("invalid Session runtime projection"),
        "E2: {error}"
    );
}

use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};

mod projection_loss;
