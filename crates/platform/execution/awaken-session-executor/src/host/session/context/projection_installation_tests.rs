//! Context publication must observe the same MCP inputs that assembly consumed.
use super::{PreparedContext, install};
use awaken_session_mcp::{
    McpGenerationProjection, McpProjectionState, McpTransportMaterial, McpTransportMaterialKind,
    McpWiring,
};
use std::sync::Arc;

struct NoInference;
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for NoInference {
    async fn infer(
        &self,
        _: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        unreachable!("constructing a context must not perform inference")
    }
}

fn projection(thread: &str, version: u64) -> McpGenerationProjection {
    let request = crate::session_mcp::StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: crate::test_mcp::mcp_fence(
            thread,
            "docs",
            version,
            "runtime-1",
            7,
            u64::MAX - 1,
        ),
        realization_id: format!("realize-{version}"),
        stage_idempotency_key: format!("stage-{version}"),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http(format!(
            "https://mcp-{version}.example.test"
        ))
        .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    McpGenerationProjection {
        receipt: awaken_session_configuration_contract::mcp::McpRealizationReceipt {
            generation: request.generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(McpTransportMaterial {
            name: "docs".into(),
            prompts_as_skills: false,
            transport: McpTransportMaterialKind::Http {
                url: format!("https://mcp-{version}.example.test"),
                bearer: None,
                refresh: None,
            },
        }),
        native_wiring: Some(McpWiring::empty()),
        mcp_process: None,
        platform_egress: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staged,
    }
}

async fn host() -> Arc<crate::host::ExecutorComponents> {
    crate::SessionExecutorBuilder::ephemeral(Arc::new(NoInference), "stub")
        .await
        .with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor))
        .build()
        .expect("validated context fixture")
}

async fn publish(host: &crate::host::ExecutorComponents, thread: &str, version: u64) {
    let next = projection(thread, version);
    let generation = next.request.generation.clone();
    host.run_assembly.insert_mcp_projection(next).unwrap();
    host.run_assembly
        .publish_mcp_projection(&generation)
        .await
        .unwrap();
}

#[tokio::test]
async fn late_context_install_cannot_restore_inputs_preceding_mcp_publication_or_removal() {
    // C1 the constructor captured an empty or active generation set; C2 before
    // its final installation, publication adds/replaces a generation or drain
    // removes it. The same production install must refuse the stale result,
    // leave the invalidated publication cache empty. Drain retains the old
    // Runtime owner until outer quiescence finishes; refusal must retain that
    // owner rather than clear it. No timer chooses these rows.
    for (initial, replacement) in [(0, 1), (1, 2), (1, 0)] {
        let thread = &format!("context-install-{initial}-{replacement}");
        let host = host().await;
        if initial != 0 {
            publish(&host, thread, initial).await;
        }
        let ctx = host.run_assembly.ctx_for(thread, None).await.unwrap();
        let active_mcp = host.run_assembly.active_mcp_projections(thread).unwrap();
        if replacement != 0 {
            publish(&host, thread, replacement).await;
        } else {
            host.run_assembly
                .drain_mcp_projection(&active_mcp[0].request.generation)
                .await
                .unwrap();
        }
        let cached = host
            .run_assembly
            .installed_sessions
            .read(thread, |s| s.run.context.clone())
            .unwrap()
            .unwrap();
        assert_eq!(cached.is_none(), replacement != 0);
        if let Some(cached) = cached {
            assert!(Arc::ptr_eq(&cached, &ctx));
        }
        let late = install(
            &host.run_assembly,
            thread,
            PreparedContext {
                ctx: ctx.clone(),
                active_mcp,
            },
        )
        .await;
        assert_eq!(
            late.err().expect("stale constructor must be refused").code,
            "mcp_context_inputs_changed",
            "{initial}->{replacement}"
        );
        let cached = host
            .run_assembly
            .installed_sessions
            .read(thread, |s| s.run.context.clone())
            .unwrap()
            .unwrap();
        assert_eq!(cached.is_none(), replacement != 0);
        if let Some(cached) = cached {
            assert!(Arc::ptr_eq(&cached, &ctx));
        }
        if replacement == 0 {
            assert!(
                host.run_assembly
                    .active_mcp_projections(thread)
                    .unwrap()
                    .is_empty()
            );
        } else {
            let fresh = host.run_assembly.ctx_for(thread, None).await.unwrap();
            let targets = &fresh.attempt_context.prepared_mcp;
            assert_eq!(targets.len(), 1);
            assert_eq!(
                targets[0].target,
                projection(thread, replacement).request.target
            );
        }
    }
}

#[tokio::test]
async fn unchanged_or_only_staged_mcp_inputs_keep_a_built_context_valid() {
    // An un-published successor is not a visible replacement. Do not refuse a
    // context solely because staging changed: publication owns invalidation.
    for staged in [false, true] {
        let thread = &format!("context-staged-{staged}");
        let host = host().await;
        publish(&host, thread, 1).await;
        let ctx = host.run_assembly.ctx_for(thread, None).await.unwrap();
        let active_mcp = host.run_assembly.active_mcp_projections(thread).unwrap();
        if staged {
            host.run_assembly
                .insert_mcp_projection(projection(thread, 2))
                .unwrap();
        }
        let original = ctx.clone();
        let installed = install(
            &host.run_assembly,
            thread,
            PreparedContext { ctx, active_mcp },
        )
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&original, &installed));
        assert_eq!(
            installed.attempt_context.prepared_mcp[0].target,
            projection(thread, 1).request.target
        );
    }
}

#[tokio::test]
async fn lease_only_renewal_preserves_a_context_built_from_the_same_connection() {
    let thread = "context-renewal";
    let host = host().await;
    publish(&host, thread, 1).await;
    let ctx = host.run_assembly.ctx_for(thread, None).await.unwrap();
    let active_mcp = host.run_assembly.active_mcp_projections(thread).unwrap();
    let mut renewed = active_mcp[0].request.clone();
    renewed.generation = crate::test_mcp::mcp_fence(thread, "docs", 1, "runtime-1", 7, u64::MAX);
    renewed.stage_idempotency_key = "renew-same-connection".into();
    host.run_assembly
        .renew_mcp_projection(&renewed)
        .unwrap()
        .unwrap();
    let installed = install(
        &host.run_assembly,
        thread,
        PreparedContext {
            ctx: ctx.clone(),
            active_mcp,
        },
    )
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&ctx, &installed));
    assert_eq!(
        host.run_assembly.active_mcp_projections(thread).unwrap()[0]
            .request
            .generation,
        renewed.generation
    );
}

#[tokio::test]
async fn a_removed_session_or_recreated_same_generation_refuses_the_original_context() {
    // Equal logical generation and target do not make a new physical
    // connection the constructor's original input. Removal cannot resurrect
    // an installed Session through a late cache write either.
    for recreate in [false, true] {
        let thread = &format!("context-recreated-{recreate}");
        let host = host().await;
        publish(&host, thread, 1).await;
        let ctx = host.run_assembly.ctx_for(thread, None).await.unwrap();
        let active_mcp = host.run_assembly.active_mcp_projections(thread).unwrap();
        if recreate {
            host.run_assembly
                .drain_mcp_projection(&active_mcp[0].request.generation)
                .await
                .unwrap();
            assert!(
                host.run_assembly
                    .forget_replayable_removed_mcp_projection(&active_mcp[0].request)
            );
            publish(&host, thread, 1).await;
            let current = host.run_assembly.active_mcp_projections(thread).unwrap();
            assert_eq!(current[0].request, active_mcp[0].request);
            assert!(!Arc::ptr_eq(&current[0].drain, &active_mcp[0].drain));
        } else {
            assert!(
                host.run_assembly
                    .installed_sessions
                    .remove(thread)
                    .is_some()
            );
        }
        let late = install(
            &host.run_assembly,
            thread,
            PreparedContext { ctx, active_mcp },
        )
        .await;
        assert_eq!(
            late.err().expect("late installation must be refused").code,
            "mcp_context_inputs_changed"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .read(thread, |s| s.run.context.is_none())
                .unwrap(),
            recreate.then_some(true)
        );
    }
}

#[tokio::test]
async fn quiescence_refuses_late_install_even_when_the_captured_mcp_set_was_empty() {
    for active in [false, true] {
        let thread = &format!("context-quiescence-{active}");
        let host = host().await;
        if active {
            publish(&host, thread, 1).await;
        }
        let ctx = host.run_assembly.ctx_for(thread, None).await.unwrap();
        let active_mcp = host.run_assembly.active_mcp_projections(thread).unwrap();
        let session = awaken_session_types::SessionId::from(thread.as_str());
        let generation = awaken_session_contract::SandboxGeneration::new(
            &session,
            1,
            100,
            "environment",
            "base",
        );
        let operation = awaken_session_contract::SessionEnvironmentOperation::new(
            "workspace-a",
            &session,
            "suspend",
            &generation,
            3,
            None,
            None,
        );
        host.run_assembly
            .installed_sessions
            .realizations()
            .close_mcp_realization_admission(
                thread,
                awaken_session_mcp::McpQuiescenceAdmissionFence::new(
                    &operation,
                    "source-effect",
                    "source-binding",
                    &generation,
                ),
            )
            .unwrap();
        let late = install(
            &host.run_assembly,
            thread,
            PreparedContext {
                ctx: ctx.clone(),
                active_mcp,
            },
        )
        .await;
        assert_eq!(
            late.err().expect("late installation must be refused").code,
            "mcp_context_inputs_changed"
        );
        let retained = host
            .run_assembly
            .installed_sessions
            .read(thread, |s| s.run.context.clone())
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            Arc::ptr_eq(&ctx, &retained),
            "outer quiescence still owns the original Runtime"
        );
    }
}
