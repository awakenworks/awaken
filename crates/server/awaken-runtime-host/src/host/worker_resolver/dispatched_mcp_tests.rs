//! Claimed-dispatch MCP projection tests kept beside the resolver behavior.

use super::test_support::{AdoptionModel, deferred_environment};
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
    let legacy = awaken_run_ingress::SessionRuntimeEnvelope::new(
        serde_json::json!({
            "environment": deferred_environment(),
            "toolsets": null
        })
        .to_string(),
    );
    assert!(
        legacy
            .decode_projection()
            .expect("R1 legacy envelope")
            .mcp_stages
            .is_none(),
        "R1"
    );
    let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
    let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    host.register_thread_agent_projection("mcp-dispatch", "agent-a");
    host.register_thread_backend_projection("mcp-dispatch", "acp:gemini");
    host.install_environment_projection("mcp-dispatch", &deferred_environment())
        .expect("install frozen Environment");

    let request = |session: &str, attachment: &str, expiry: u64| {
        awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: session.into(),
                attachment_id: awaken_session_contract::McpAttachmentId(attachment.into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "control-runtime".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: expiry,
            },
            realization_id: format!("realize-{attachment}"),
            stage_idempotency_key: format!("stage-{attachment}"),
            name: attachment.into(),
            target: awaken_session_contract::McpTarget::parse_http(format!(
                "https://{attachment}.example.test/mcp"
            ))
            .expect("HTTP target"),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        }
    };
    let exact = request("mcp-dispatch", "docs", u64::MAX);

    HostWorkerResolver::reconcile_dispatched_mcp(
        host.as_ref(),
        "mcp-dispatch",
        vec![exact.clone()],
    )
    .await
    .expect("R2");
    assert_eq!(host.active_mcp_projections("mcp-dispatch").len(), 1, "R2");
    HostWorkerResolver::reconcile_dispatched_mcp(
        host.as_ref(),
        "mcp-dispatch",
        vec![exact.clone()],
    )
    .await
    .expect("R3");
    assert_eq!(host.active_mcp_projections("mcp-dispatch").len(), 1, "R3");

    HostWorkerResolver::reconcile_dispatched_mcp(host.as_ref(), "mcp-dispatch", Vec::new())
        .await
        .expect("R4");
    assert!(host.active_mcp_projections("mcp-dispatch").is_empty(), "R4");

    let retained = request("mcp-dispatch", "retained", u64::MAX);
    HostWorkerResolver::reconcile_dispatched_mcp(host.as_ref(), "mcp-dispatch", vec![retained])
        .await
        .expect("R5 setup");
    let foreign = request("foreign-session", "replacement", u64::MAX);
    assert!(
        HostWorkerResolver::reconcile_dispatched_mcp(host.as_ref(), "mcp-dispatch", vec![foreign],)
            .await
            .is_err(),
        "R5 foreign"
    );
    assert_eq!(host.active_mcp_projections("mcp-dispatch").len(), 1, "R5");
    let expired = request("mcp-dispatch", "replacement", 0);
    assert!(
        HostWorkerResolver::reconcile_dispatched_mcp(host.as_ref(), "mcp-dispatch", vec![expired],)
            .await
            .is_err(),
        "R5 expired"
    );
    assert_eq!(host.active_mcp_projections("mcp-dispatch").len(), 1, "R5");
}
