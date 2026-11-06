//! Separate the SDK direct assertion from an installed service assignment.
use awaken_session_execution_contract::McpAttachmentRealizer;
use std::sync::Arc;

#[tokio::test]
async fn assigned_staging_requires_a_root_and_direct_assertions_never_override_a_closed_root() {
    // Table: unknown root -> Assigned rejects, Direct may stage; live original
    // root -> Assigned can replay the exact receipt; closed root -> both reject.
    // Keep the real transport and its original projection visible to the test:
    // no replacement installed Session, fabricated root, or alternate material store exists.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let managed = host.clone();
    let session = "explicit-staging-admission";
    let generation = crate::test_mcp::mcp_fence(session, "docs", 1, "runtime-1", 1, u64::MAX);
    let (url, seen) = crate::test_mcp::start(None).await;
    let request = crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: generation.clone(),
        realization_id: "realization".into(),
        stage_idempotency_key: "stage".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(&url).unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&generation)
            .expect("healthy registry"),
        None
    );
    let error = managed
        .run_assembly
        .stage_mcp_attachment(request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "mcp_stale_ownership");
    assert!(host.run_assembly.mcp_projection(&generation).is_none());
    assert!(
        seen.lock().is_empty(),
        "unknown assignment cannot enter transport I/O"
    );

    let direct = managed
        .run_assembly
        .stage_direct_mcp_attachment(request.clone())
        .await
        .unwrap();
    let projection = host
        .run_assembly
        .mcp_projection(&generation)
        .expect("original direct projection");
    assert_eq!(projection.receipt, direct);
    assert!(projection.server.is_some());
    let requests_before = seen.lock().len();
    assert_eq!(
        managed
            .run_assembly
            .stage_mcp_attachment(request.clone())
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership"
    );
    assert_eq!(
        seen.lock().len(),
        requests_before,
        "an existing projection grants no assigned-root fallback"
    );

    host.run_assembly
        .install_session_realization_lease(
            session,
            crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "runtime-1".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
        )
        .unwrap();
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&generation)
            .expect("healthy registry"),
        Some(true)
    );
    assert_eq!(
        managed
            .run_assembly
            .stage_mcp_attachment(request.clone())
            .await
            .unwrap(),
        direct
    );
    assert_eq!(
        seen.lock().len(),
        requests_before,
        "exact replay retains the original transport"
    );

    let _closed = host
        .run_assembly
        .begin_session_realization_revocation()
        .expect("installed Sessions");
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&generation)
            .expect("healthy registry"),
        Some(false)
    );
    assert_eq!(
        managed
            .run_assembly
            .stage_mcp_attachment(request.clone())
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership"
    );
    assert_eq!(
        managed
            .run_assembly
            .stage_direct_mcp_attachment(request)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership"
    );
    assert_eq!(seen.lock().len(), requests_before);
    assert_eq!(
        host.run_assembly
            .mcp_projection(&generation)
            .unwrap()
            .receipt,
        direct
    );
}
