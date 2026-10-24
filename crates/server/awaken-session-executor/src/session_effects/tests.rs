//! Exercise the two constructor-bound MCP roles, including injected failures.
use super::*;
use awaken_dispatch_execution::session_execution::RunMcpAttachments as _;
use awaken_session_execution_contract::McpAttachmentRealizer;
use awaken_session_realization::worker_projection::SessionProjectionExecution;
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
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let (url, seen) = crate::test_mcp::start(None).await;
    let stage = request("constructor-mcp-admission", &url);
    let run = &crate::session_effects::RunMcpEffects::new(host.run_assembly.clone());
    let session = SessionProjectionExecution::mcp_effects(host.run_assembly.as_ref());
    assert_eq!(
        host.run_assembly
            .mcp_generation_root_authority(&stage.generation)
            .expect("healthy registry"),
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
            .mcp_generation_root_authority(&stage.generation)
            .expect("healthy registry"),
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
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
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

#[tokio::test]
async fn retained_configured_mcp_effects_release_unrelated_session_owners() {
    let realizer = Arc::new(FailingRealizer::default());
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_session_execution(
        crate::SessionExecutionDependencies::default()
            .with_mcp_attachment_realizer(realizer.clone()),
    )
    .build()
    .unwrap();
    let context = host
        .run_assembly
        .ctx_for_session_coordination("configured-mcp-idle", None)
        .await
        .unwrap();
    let effects = RunMcpEffects::new(host.run_assembly.clone());
    let assembly = Arc::downgrade(&host.run_assembly);
    let installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    let cached = Arc::downgrade(&context);
    drop(context);
    drop(host);
    assert!(
        assembly.upgrade().is_none()
            && installed_sessions.upgrade().is_none()
            && cached.upgrade().is_none()
    );
    let stage = request("released-configured-mcp", "http://127.0.0.1:9/mcp");
    for result in [
        effects
            .stage_mcp_attachment(stage.clone())
            .await
            .map(|_| ()),
        effects
            .publish_mcp_generation(stage.generation.clone())
            .await,
        effects.drain_mcp_generation(stage.generation.clone()).await,
    ] {
        assert_eq!(
            result.unwrap_err().code,
            "configured_mcp_failure",
            "original configured failure, no local fallback"
        );
    }
    assert_eq!(*realizer.stages.lock().unwrap(), vec![stage.clone()]);
    assert_eq!(
        *realizer.generations.lock().unwrap(),
        vec![
            ("publish", stage.generation.clone()),
            ("drain", stage.generation.clone())
        ]
    );
    assert_eq!(
        effects
            .active_stages(stage.generation.session_id().as_str())
            .unwrap_err()
            .code,
        "mcp_projection_owner_released",
        "missing original state is not an empty projection"
    );
    assert!(installed_sessions.upgrade().is_none());
}

#[tokio::test]
async fn retained_direct_mcp_effects_keep_required_execution_owner_until_drained() {
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let effects = RunMcpEffects::new(host.run_assembly.clone());
    let assembly = Arc::downgrade(&host.run_assembly);
    let installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    drop(host);
    assert!(
        assembly.upgrade().is_some() && installed_sessions.upgrade().is_some(),
        "owned Direct execution still needs its original context"
    );
    let (url, seen) = crate::test_mcp::start(None).await;
    let stage = request("released-direct-mcp", &url);
    effects.stage_mcp_attachment(stage.clone()).await.unwrap();
    effects
        .publish_mcp_generation(stage.generation.clone())
        .await
        .unwrap();
    assert_eq!(
        effects
            .active_stages(stage.generation.session_id().as_str())
            .unwrap(),
        vec![stage.clone()]
    );
    assert!(
        !seen.lock().unwrap().is_empty(),
        "actual original transport handled staging"
    );
    effects
        .drain_mcp_generation(stage.generation.clone())
        .await
        .unwrap();
    assert!(
        effects
            .active_stages(stage.generation.session_id().as_str())
            .unwrap()
            .is_empty()
    );
    drop(effects);
    assert!(
        assembly.upgrade().is_none() && installed_sessions.upgrade().is_none(),
        "last genuine execution role releases its original owners"
    );
}
