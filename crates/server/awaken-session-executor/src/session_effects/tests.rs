//! Exercise the two constructor-bound MCP roles, including injected failures.
use super::*;
use awaken_dispatch_execution::session_execution::RunMcpAttachments as _;
use awaken_session_realization::worker_projection::SessionProjectionExecution;
use awaken_session_execution_contract::McpAttachmentRealizer;
use std::sync::Mutex;

fn request(session: &str, url: &str) -> crate::session_mcp::StageMcpAttachment {
    crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: crate::test_mcp::mcp_fence(session, "docs", 1, "runtime", 1, u64::MAX),
        realization_id: "realize-docs".into(),
        stage_idempotency_key: "stage-docs".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(url).unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    }
}

#[tokio::test]
async fn ordinary_run_and_managed_session_use_distinct_mcp_admission() {
    // A1 unknown root: Session rejects before I/O, Run uses SDK Direct.
    // A2 an installed live root: Session replays that exact original receipt.
    // A3 closed root: both reject; Direct cannot override installed authority.
    let host =
        crate::HostDependencies::ephemeral(Arc::new(crate::NoModelConfiguredExecutor), "stub")
            .await
            .build()
            .unwrap();
    let (url, seen) = crate::test_mcp::start(None).await;
    let stage = request("constructor-mcp-admission", &url);
    let run = &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone());
    let session = SessionProjectionExecution::mcp_effects(host.run_assembly.as_ref());
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&stage.generation),
        None
    );
    assert_eq!(
        session
            .stage_mcp_attachment(stage.clone())
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership"
    );
    assert!(
        host.run_assembly
            .mcp_projection(&stage.generation)
            .is_none()
    );
    assert!(seen.lock().unwrap().is_empty());
    let receipt = run.stage_mcp_attachment(stage.clone()).await.unwrap();
    run.publish_mcp_generation(stage.generation.clone())
        .await
        .unwrap();
    assert_eq!(
        run.active_stages(stage.generation.session_id().as_str())
            .unwrap(),
        vec![stage.clone()]
    );
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&stage.generation),
        None,
        "ordinary dispatch must not manufacture a Session root"
    );
    host.run_assembly
        .install_session_realization_lease(
            stage.generation.session_id().as_str(),
            crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "runtime".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
        )
        .unwrap();
    assert_eq!(
        session.stage_mcp_attachment(stage.clone()).await.unwrap(),
        receipt
    );
    let _closed = host
        .run_assembly
        .begin_session_realization_revocation()
        .expect("installed Sessions");
    let requests_before = seen.lock().unwrap().len();
    for effects in [session, run as &dyn McpAttachmentRealizer] {
        assert_eq!(
            effects
                .stage_mcp_attachment(stage.clone())
                .await
                .unwrap_err()
                .code,
            "mcp_stale_ownership"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), requests_before);
}

#[derive(Default)]
struct FailingRealizer {
    stages: Mutex<Vec<crate::session_mcp::StageMcpAttachment>>,
    generations: Mutex<Vec<(&'static str, crate::session_mcp::McpGenerationRef)>>,
}

#[async_trait::async_trait]
impl McpAttachmentRealizer for FailingRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.stages.lock().unwrap().push(request);
        Err(RunError::classified(
            "configured_mcp_failure",
            "original configured stage failure",
        ))
    }
    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.generations
            .lock()
            .unwrap()
            .push(("publish", generation));
        Err(RunError::classified(
            "configured_mcp_failure",
            "original configured publish failure",
        ))
    }
    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.generations.lock().unwrap().push(("drain", generation));
        Err(RunError::classified(
            "configured_mcp_failure",
            "original configured drain failure",
        ))
    }
}

#[tokio::test]
async fn configured_realizer_is_exclusive_for_both_mcp_roles() {
    // Both constructor-bound roles forward exact requests and original failures.
    // No configured error grants a local stage, publication, or drain fallback.
    let realizer = Arc::new(FailingRealizer::default());
    let host =
        crate::HostDependencies::ephemeral(Arc::new(crate::NoModelConfiguredExecutor), "stub")
            .await
            .with_session_execution(
                crate::SessionExecutionDependencies::default()
                    .with_mcp_attachment_realizer(realizer.clone()),
            )
            .build()
            .unwrap();
    let (url, seen) = crate::test_mcp::start(None).await;
    let stage = request("configured-mcp-admission", &url);
    let run = &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone());
    let session = SessionProjectionExecution::mcp_effects(host.run_assembly.as_ref());
    for effects in [session, run as &dyn McpAttachmentRealizer] {
        let error = effects
            .stage_mcp_attachment(stage.clone())
            .await
            .unwrap_err();
        assert_eq!(error.code, "configured_mcp_failure");
        assert_eq!(error.message, "original configured stage failure");
        let error = effects
            .publish_mcp_generation(stage.generation.clone())
            .await
            .unwrap_err();
        assert_eq!(error.code, "configured_mcp_failure");
        assert_eq!(error.message, "original configured publish failure");
        let error = effects
            .drain_mcp_generation(stage.generation.clone())
            .await
            .unwrap_err();
        assert_eq!(error.code, "configured_mcp_failure");
        assert_eq!(error.message, "original configured drain failure");
    }
    assert_eq!(
        *realizer.stages.lock().unwrap(),
        vec![stage.clone(), stage.clone()]
    );
    assert_eq!(
        *realizer.generations.lock().unwrap(),
        vec![
            ("publish", stage.generation.clone()),
            ("drain", stage.generation.clone()),
            ("publish", stage.generation.clone()),
            ("drain", stage.generation.clone())
        ]
    );
    assert!(
        host.run_assembly
            .mcp_projection(&stage.generation)
            .is_none()
    );
    assert!(seen.lock().unwrap().is_empty());
}
