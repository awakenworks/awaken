//! MCP projection generations: hot replacement, drain, quiescence, and relay routes.

use super::*;

async fn host_with_process_reaper() -> Arc<ExecutorComponents> {
    (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor)))
    .build()
    .expect("validated Host dependencies")
}

/// Native and ACP are projections of the same exact durable generation.  This
/// test deliberately drives the Host projection state directly: transport
/// differences may change how a visible generation is consumed, but may never
/// change which generation is visible.
#[tokio::test]
async fn native_and_acp_project_the_same_generation_across_hot_replacement() {
    use awaken_session_configuration_contract::mcp::McpRealizationReceipt;
    use awaken_session_mcp::project_mcp_session_transport;
    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};
    use awaken_session_mcp::{McpTransportMaterial, McpWiring};

    let host = host_with_process_reaper().await;
    let generation = |number: u64| {
        crate::test_mcp::mcp_fence("mcp-parity", "mcp-docs", number, "runtime-1", 7, u64::MAX)
    };
    let projection = |number: u64, secret: &str| {
        let request = crate::session_mcp::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation(number),
            realization_id: format!("realize-{number}"),
            stage_idempotency_key: format!("stage-{number}"),
            name: "docs".into(),
            target: awaken_agent_contract::McpTarget::parse_http(format!(
                "https://mcp-{number}.example.test"
            ))
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        McpGenerationProjection {
            receipt: McpRealizationReceipt {
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
                transport: awaken_session_mcp::McpTransportMaterialKind::Http {
                    url: format!("https://mcp-{number}.example.test"),
                    bearer: Some(awaken_agent_contract::RedactedString::new(secret)),
                    refresh: None,
                },
            }),
            native_wiring: Some(McpWiring {
                notification_transports: Vec::new(),
                plugins: Vec::new(),
                tool_ids: vec![format!("docs-generation-{number}")],
                skill_registries: Vec::new(),
                call_fences: Vec::new(),
            }),
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Staged,
        }
    };
    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    assert!(
        host.run_assembly.mcp.install_relay(relay.clone()),
        "install the canonical Host relay"
    );

    // Cause graph:
    // exact generation + private route staged --publish--> active/visible --replacement publish-->
    // old draining + new active --old drain--> old route removed.  Both adapter
    // projections consume only `active_mcp_projections`; ACP additionally turns
    // that exact generation into a loopback route without a credential.
    //
    // | Rule | g1 state | g2 state | Native visible | ACP visible | old route |
    // |------|----------|----------|----------------|-------------|-----------|
    // | P1   | staged   | absent   | none           | none        | staged/private |
    // | P2   | active   | absent   | g1             | g1          | present   |
    // | P3   | active   | staged   | g1             | g1          | present   |
    // | P4   | draining | active   | g2             | g2          | present   |
    // | P5   | removed  | active   | g2             | g2          | absent    |
    host.run_assembly
        .insert_mcp_projection(projection(1, "secret-one"))
        .unwrap();
    let staged = host.run_assembly.mcp_projection(&generation(1)).unwrap();
    relay.set_route(&staged.request.generation, staged.server.as_ref().unwrap());
    let staged_route = relay.route_url(&generation(1)).expect("P1 staged route");
    assert!(
        host.run_assembly
            .active_mcp_projections("mcp-parity")
            .expect("registry and projection are healthy")
            .is_empty(),
        "P1"
    );

    host.run_assembly
        .publish_mcp_projection(&generation(1))
        .await
        .unwrap();
    let visible = host
        .run_assembly
        .active_mcp_projections("mcp-parity")
        .expect("installed Sessions");
    assert_eq!(
        visible[0].native_wiring.as_ref().unwrap().tool_ids[0],
        "docs-generation-1",
        "P2"
    );
    let acp = project_mcp_session_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request,
        Some(&relay),
        &visible[0].receipt,
        awaken_run_executor_acp::acp_adapter_policy(),
        "codex",
    )
    .unwrap();
    assert!(acp.auth.is_none(), "P2 legacy route remains secret-free");
    let old_route = acp.url.expect("P2 expected HTTP transport");
    assert_eq!(old_route, staged_route, "P2 publish reuses staged effect");
    assert!(old_route.contains("/mcp-parity/mcp-docs/1/"), "P2");

    host.run_assembly
        .insert_mcp_projection(projection(2, "secret-two"))
        .unwrap();
    let staged = host.run_assembly.mcp_projection(&generation(2)).unwrap();
    relay.set_route(&staged.request.generation, staged.server.as_ref().unwrap());
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-parity")
            .expect("installed Sessions")[0]
            .request
            .generation
            .generation()
            .version(),
        1,
        "P3"
    );

    host.run_assembly
        .publish_mcp_projection(&generation(2))
        .await
        .unwrap();
    let visible = host
        .run_assembly
        .active_mcp_projections("mcp-parity")
        .expect("installed Sessions");
    assert_eq!(visible.len(), 1, "P4");
    assert_eq!(
        visible[0].native_wiring.as_ref().unwrap().tool_ids[0],
        "docs-generation-2",
        "P4"
    );
    let replacement = project_mcp_session_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request,
        Some(&relay),
        &visible[0].receipt,
        awaken_run_executor_acp::acp_adapter_policy(),
        "codex",
    )
    .unwrap();
    assert!(
        replacement.auth.is_none(),
        "P4 legacy route remains secret-free"
    );
    let new_route = replacement.url.expect("P4 expected HTTP transport");
    assert!(new_route.contains("/mcp-parity/mcp-docs/2/"), "P4");
    assert_ne!(old_route, new_route, "P4");

    host.run_assembly
        .drain_mcp_projection(&generation(1))
        .await
        .unwrap();
    assert_eq!(
        host.run_assembly
            .active_mcp_projections("mcp-parity")
            .expect("installed Sessions")[0]
            .request
            .generation
            .generation()
            .version(),
        2,
        "P5"
    );
    assert_eq!(
        reqwest::Client::new()
            .post(old_route)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "P5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_drain_acknowledges_removed_only_after_busy_call_quiesces() {
    use std::sync::{Arc, Condvar, Mutex};

    use async_trait::async_trait;
    use awaken_ext_mcp::transport::{McpToolTransport, revocable_transport};
    use awaken_ext_mcp::{CallToolResult, McpToolDefinition, McpTransportError};
    use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};

    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};
    use awaken_session_mcp::{McpTransportMaterial, McpTransportMaterialKind, McpWiring};

    struct DropGate(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for DropGate {
        fn drop(&mut self) {
            let (released, signal) = self.0.as_ref();
            let mut released = released.lock().expect("drop gate mutex");
            while !*released {
                released = signal.wait(released).expect("drop gate wait");
            }
        }
    }

    struct BusyTransport {
        started: Arc<tokio::sync::Notify>,
        drop_gate: Arc<(Mutex<bool>, Condvar)>,
    }

    #[async_trait]
    impl McpToolTransport for BusyTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: serde_json::Value,
        ) -> Result<CallToolResult, McpTransportError> {
            let _drop_gate = DropGate(self.drop_gate.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    // Cause/effect graph: C0 the busy task may enter before this test begins
    // awaiting its single start signal; C1 one exact generation is Active; C2 one local tool
    // call is in flight; C3 drain is requested; C4 cancellation has begun but
    // the call future has not completed its drop. Effects: E1 new visibility is
    // closed immediately; E2 state remains Draining and no Removed receipt can
    // be observed during C4; E3 releasing the last call guard permits Removed;
    // E4 the busy call terminates with revocation instead of producing a late
    // result. Constraints: C0 uses one retained Notify permit, never a broadcast
    // that can disappear before registration. Decision rules:
    // Q0 C0=>the start observation is retained; Q1 C1+C2+C3+C4=>E1+E2;
    // Q2 Q1+quiesced=>E3+E4.
    let host = host_with_process_reaper().await;
    let generation =
        crate::test_mcp::mcp_fence("mcp-busy-drain", "docs", 1, "runtime-1", 1, u64::MAX);
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let drop_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (transport, fence) = revocable_transport(Arc::new(BusyTransport {
        started: started.clone(),
        drop_gate: drop_gate.clone(),
    }));
    host.run_assembly
        .insert_mcp_projection(McpGenerationProjection {
            receipt: McpRealizationReceipt {
                generation: generation.clone(),
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
                    url: "https://mcp.example.test/sse".into(),
                    bearer: None,
                    refresh: None,
                },
            }),
            native_wiring: Some(McpWiring {
                notification_transports: Vec::new(),
                plugins: Vec::new(),
                tool_ids: Vec::new(),
                skill_registries: Vec::new(),
                call_fences: vec![fence],
            }),
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Active,
        })
        .unwrap();

    let busy_call =
        tokio::spawn(async move { transport.call_tool("busy", serde_json::Value::Null).await });
    started.notified().await;
    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let drain = tokio::spawn(async move {
        drain_host
            .run_assembly
            .drain_mcp_projection(&drain_generation)
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if host
                .run_assembly
                .mcp_projection(&generation)
                .is_some_and(|projection| projection.state == McpProjectionState::Draining)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Q1 drain enters Draining");
    assert!(
        !drain.is_finished(),
        "Q1/E2 no early Removed acknowledgement"
    );

    let (released, signal) = drop_gate.as_ref();
    *released.lock().expect("release mutex") = true;
    signal.notify_all();
    assert!(busy_call.await.unwrap().is_err(), "Q2/E4 busy call revoked");
    drain.await.unwrap().unwrap();
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "Q2/E3"
    );
}

#[tokio::test(start_paused = true)]
async fn worker_relay_drain_retains_exact_route_across_cancel_and_timeout_retry() {
    use awaken_credential_contract::CredentialRealizationKind;
    use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};

    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};
    use awaken_session_mcp::{McpTransportMaterial, McpTransportMaterialKind};

    // Route-drain cause/effect table (paired with mcp_relay's actual-forward
    // acceptance test):
    // | Rule | exact relay permit | drain attempt | Effect |
    // | R1 | active | Future cancelled | Draining + same closed Route retained; no proof |
    // | R2 | active | outer revoke wait expires | Unavailable; Draining + owner retained; no proof |
    // | R3 | settled | retry | exact Route removed, then projection Removed + proof |
    // C1 the receipt is WorkerRelay and C2 this is its exact generation are
    // constraints for every rule. E1 no new route permit is admitted after the
    // first close. Cancellation/timeout never manufactures a different fence,
    // route registry, or Removed receipt.
    let host = host_with_process_reaper().await;
    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    assert!(host.run_assembly.mcp.install_relay(relay.clone()));
    let generation = crate::test_mcp::mcp_fence(
        "mcp-worker-relay-drain",
        "docs",
        1,
        "runtime-1",
        1,
        u64::MAX,
    );
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: McpTransportMaterialKind::Http {
            url: "https://mcp.example.test/sse".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("relay-secret")),
            refresh: None,
        },
    };
    relay.set_route(&generation, &server);
    let permit = relay
        .route_call_fence(&generation)
        .unwrap()
        .try_enter()
        .expect("accepted exact route activity");
    host.run_assembly
        .insert_mcp_projection(McpGenerationProjection {
            receipt: McpRealizationReceipt {
                generation: generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: Some(CredentialRealizationKind::WorkerRelay),
                receipt_fingerprint: request.fingerprint(),
            },
            request,
            server: Some(server),
            native_wiring: None,
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Active,
        })
        .unwrap();

    let cancelled_host = host.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_host
            .run_assembly
            .revoke_all_session_realizations()
            .await
    });
    loop {
        if host
            .run_assembly
            .mcp_projection(&generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancelled.abort();
    let _ = cancelled.await;
    assert!(
        relay.route_url(&generation).is_some(),
        "R1 exact Route retained"
    );
    assert!(
        relay
            .route_call_fence(&generation)
            .unwrap()
            .try_enter()
            .is_none(),
        "R1/E1 closed admission is retry-stable"
    );
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "R1 no Removed proof"
    );

    let timeout = host
        .run_assembly
        .revoke_all_session_realizations()
        .await
        .expect_err("R2 outer revoke must propagate held route timeout");
    assert_eq!(timeout.code, "mcp_generation_call_quiescence_timeout", "R2");
    assert!(relay.route_url(&generation).is_some(), "R2 owner retained");
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "R2 no Removed proof"
    );

    drop(permit);
    let proof = host
        .run_assembly
        .drain_mcp_projections(
            generation.session_id().as_str(),
            std::slice::from_ref(&generation),
        )
        .await
        .expect("R3 settled retry");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&generation),
        "R3 proof"
    );
    assert!(
        relay.route_url(&generation).is_none(),
        "R3 exact Route removed"
    );
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "R3 Removed follows route quiescence"
    );
}

#[tokio::test(start_paused = true)]
async fn mcp_quiescence_retains_failed_effect_owners_for_retry() {
    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;
    use awaken_session_configuration_contract::mcp::{
        McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

    struct Process {
        id: String,
        unreapable: bool,
    }

    #[async_trait]
    impl pc::ProcessHandle for Process {
        fn id(&self) -> &str {
            &self.id
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            if self.unreapable {
                std::future::pending().await
            } else {
                Ok(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                })
            }
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            if self.unreapable {
                Ok(None)
            } else {
                Ok(Some(self.wait().await?))
            }
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            if self.unreapable {
                Err(pc::SandboxError::backend(
                    "signal",
                    "scripted MCP signal failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    let host = host_with_process_reaper().await;
    let generation = |session: &str, number| {
        crate::test_mcp::mcp_fence(
            session,
            format!("mcp-{number}"),
            number,
            "runtime-1",
            1,
            u64::MAX,
        )
    };
    let projection = |generation: McpGenerationRef, unreapable| {
        let request = StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: format!("realize-{}", generation.generation().version()),
            stage_idempotency_key: format!("stage-{}", generation.generation().version()),
            name: format!("mcp-{}", generation.generation().version()),
            target: awaken_agent_contract::McpTarget::parse_http(format!(
                "https://mcp-{}.example.test",
                generation.generation().version()
            ))
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        McpGenerationProjection {
            receipt: McpRealizationReceipt {
                generation: generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            },
            request,
            server: None,
            native_wiring: None,
            mcp_process: Some(Arc::new(Process {
                id: format!("process-{}", generation.generation().version()),
                unreapable,
            })),
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Active,
        }
    };
    let missing_reaper_host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let missing = generation("mcp-reaper-missing", 9);
    let missing_projection = projection(missing.clone(), false);
    let same_handle = missing_projection.mcp_process.as_ref().unwrap().clone();
    missing_reaper_host
        .run_assembly
        .insert_mcp_projection(missing_projection)
        .unwrap();
    // P0: a tracked MCP process without an installed reaper closes admission
    // but cannot be marked Removed. The exact Arc remains for owner recovery;
    // no successful process observation is inferred from a missing port.
    let unavailable = missing_reaper_host
        .run_assembly
        .drain_mcp_projection(&missing)
        .await
        .expect_err("P0 missing process authority must fail closed");
    assert_eq!(
        unavailable.code, "mcp_generation_process_reaper_missing",
        "P0"
    );
    let retained = missing_reaper_host
        .run_assembly
        .mcp_projection(&missing)
        .unwrap();
    assert_eq!(retained.state, McpProjectionState::Draining, "P0");
    assert!(
        Arc::ptr_eq(retained.mcp_process.as_ref().unwrap(), &same_handle),
        "P0 same process owner retained"
    );

    let stuck = generation("mcp-partial-quiescence", 1);
    let clean = generation("mcp-partial-quiescence", 2);
    host.run_assembly
        .insert_mcp_projection(projection(stuck.clone(), true))
        .unwrap();
    host.run_assembly
        .insert_mcp_projection(projection(clean.clone(), false))
        .unwrap();

    // Cause/effect decision table:
    // | Rule | C2 exact expected | C3 process reap | C6 cancellation | Effect |
    // | P1 | both generations | first fails, second succeeds | no | no proof;
    // |    |                  |                             |    | first owner retained Draining, second Removed |
    // The loop must continue after P1's first failure so partial cleanup never
    // strands an independently reapable generation.
    assert!(
        host.run_assembly
            .drain_mcp_projections("mcp-partial-quiescence", &[stuck.clone(), clean.clone()])
            .await
            .is_err(),
        "P1"
    );
    let stuck_projection = host.run_assembly.mcp_projection(&stuck).unwrap();
    assert_eq!(stuck_projection.state, McpProjectionState::Draining, "P1");
    assert!(stuck_projection.mcp_process.is_some(), "P1 owner retained");
    assert_eq!(
        host.run_assembly.mcp_projection(&clean).unwrap().state,
        McpProjectionState::Removed,
        "P1 partial progress"
    );

    let runtime_thread = "mcp-runtime-quiescence";
    let (runtime_host, _runtime_managed) = dispatch_test_host(host_with_process_reaper().await);
    // Physical query causes/effects (the same Runtime marker used by P2-P4):
    // RQ0 absent Session => no callback, no slot created; RQ1 installed/no
    // Context => (false,false); RQ2 Context/inactive => (true,false); RQ3
    // Context/active => (true,true); RQ4 settled => (true,false), same owner.
    // holds_context never proves physical activity. The existing view reads
    // under registry -> active_run; MCP uses the same SessionCtx primitive.
    let view = runtime_host.realization_executor.sessions();
    let query = |thread: &str| {
        view.modify_session(thread, |slot| {
            (slot.run.holds_context(), slot.run.has_active_run())
        })
        .expect("installed Sessions")
    };
    assert_eq!(query("mcp-missing-query"), None, "RQ0 no callback");
    assert!(
        runtime_host
            .run_assembly
            .installed_sessions
            .read("mcp-missing-query", |_| ())
            .expect("installed Sessions")
            .is_none(),
        "RQ0 never creates a slot"
    );
    runtime_host
        .run_assembly
        .installed_sessions
        .install_with("mcp-empty-query", |_| ())
        .expect("installed Sessions");
    assert_eq!(query("mcp-empty-query"), Some((false, false)), "RQ1");
    let runtime = runtime_host
        .run_assembly
        .ctx_for(runtime_thread, None)
        .await
        .expect("runtime owner");
    assert_eq!(
        query(runtime_thread),
        Some((true, false)),
        "RQ2 inactive Context"
    );
    runtime
        .foreground
        .install_for_test(Some(RunId("mcp-runtime-active-run".into())));
    assert_eq!(
        query(runtime_thread),
        Some((true, true)),
        "RQ3 physical marker"
    );
    let runtime_generation = generation(runtime_thread, 3);
    runtime_host
        .run_assembly
        .insert_mcp_projection(projection(runtime_generation.clone(), false))
        .unwrap();

    // Runtime-owner cause/effect decision table:
    // | Rule | C4 active Run | C6 drain cancellation/timeout | Effect |
    // | P2 | yes | Future cancelled after Draining | same slot Runtime retained; no proof |
    // | P3 | yes | retry reaches timeout | same Runtime + Draining process retained; no proof |
    // | P4 | settled | retry | process Removed + exact proof; outer quiesce still owns Runtime removal |
    // P2/P3 prevent a retry from losing the only active-run fence and turning a
    // failed attempt into false quiescence. P4 preserves the single existing
    // Session-slot owner until the whole Environment transaction succeeds.
    let cancelled_host = runtime_host.clone();
    let cancelled_generation = runtime_generation.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_host
            .run_assembly
            .drain_mcp_projections(runtime_thread, &[cancelled_generation])
            .await
    });
    loop {
        if runtime_host
            .run_assembly
            .mcp_projection(&runtime_generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancelled.abort();
    let _ = cancelled.await;
    assert!(
        runtime_host
            .run_assembly
            .installed_sessions
            .read_run(runtime_thread, |run| run
                .context
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .expect("installed Sessions")
            .unwrap_or(false),
        "P2 retains the exact Runtime owner"
    );
    assert_eq!(
        runtime_host
            .run_assembly
            .mcp_projection(&runtime_generation)
            .unwrap()
            .state,
        McpProjectionState::Draining,
        "P2"
    );

    let timeout = runtime_host
        .run_assembly
        .drain_mcp_projections(runtime_thread, std::slice::from_ref(&runtime_generation))
        .await
        .expect_err("P3 active Run must time out");
    assert!(
        timeout
            .to_string()
            .contains("could not quiesce the active Session Run"),
        "P3 reports the active owner"
    );
    assert!(
        runtime_host
            .run_assembly
            .installed_sessions
            .read_run(runtime_thread, |run| run
                .context
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .expect("installed Sessions")
            .unwrap_or(false),
        "P3 retains the exact Runtime owner"
    );
    assert!(
        runtime_host
            .run_assembly
            .mcp_projection(&runtime_generation)
            .is_some_and(
                |projection| projection.state == McpProjectionState::Draining
                    && projection.mcp_process.is_some()
            ),
        "P3 retains the process owner"
    );

    runtime.foreground.install_for_test(None);
    assert_eq!(
        query(runtime_thread),
        Some((true, false)),
        "RQ4 settled marker"
    );
    let proof = runtime_host
        .run_assembly
        .drain_mcp_projections(runtime_thread, std::slice::from_ref(&runtime_generation))
        .await
        .expect("P4 settled retry");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&runtime_generation),
        "P4"
    );
    assert_eq!(
        runtime_host
            .run_assembly
            .mcp_projection(&runtime_generation)
            .unwrap()
            .state,
        McpProjectionState::Removed,
        "P4"
    );
    assert!(
        runtime_host
            .run_assembly
            .installed_sessions
            .read_run(runtime_thread, |run| run
                .context
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .expect("installed Sessions")
            .unwrap_or(false),
        "P4 leaves Runtime removal to the outer quiesce owner"
    );
    // RQ5 foreground record poison => a typed refusal, never a panic, and
    // never context presence substituted or a false quiescence result: the
    // marker reports the Run active, so every drain keeps refusing.
    // Environment continuation's primary/delegated reads use this same kernel,
    // preserving their Slot -> foreground lock order and existing Runtime Arc.
    // Their absent/inactive/active conditions remain RQ0-RQ4; delegated status
    // still short-circuits a non-occupying dispatch before the Slot read.
    // This fault proves only the active kernel; the distinct actual Slot S4
    // poison regression belongs to installed_session/tests.rs.
    // Poison the same actual Runtime only after all P2-P4 cleanup assertions;
    // the primitive reads no registry, so this fault does not poison the slot.
    let poisoned_runtime = runtime.clone();
    let poisoned = std::thread::spawn(move || {
        let _held = poisoned_runtime.foreground.hold_for_test();
        panic!("RQ5 inject active marker poison");
    });
    assert!(poisoned.join().is_err(), "RQ5 actual marker poisoned");
    assert!(
        runtime.has_active_run(),
        "RQ5 poison must not become inactive"
    );
    assert_eq!(
        runtime.foreground.observe().unwrap_err(),
        crate::host::ForegroundRunPoisoned,
        "RQ5 typed refusal"
    );
    assert_eq!(
        query(runtime_thread),
        Some((true, true)),
        "RQ5 the slot query keeps the Run active"
    );
}

#[tokio::test]
async fn mcp_quiescence_waits_for_staging_activity_before_removed_proof() {
    // Owner/phase migration: the staging task retains the same registry Arc;
    // ordinary slot reads borrow it and do not require a Clone registry. The
    // projection, activity and drain Arcs still identify the original owner.
    // S1-S5 below retain the pending-stage/drain/process proof assertions;
    // the MCP activity guard case covers a Future dropped before first poll.
    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;
    use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};

    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState, McpStagingActivity};

    struct Process {
        reaped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl pc::ProcessHandle for Process {
        fn id(&self) -> &str {
            "mcp-staging-process"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            self.reaped.store(true, Ordering::SeqCst);
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(Some(self.wait().await?))
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    let host = host_with_process_reaper().await;
    let generation = crate::test_mcp::mcp_fence(
        "mcp-staging-quiescence",
        "docs",
        1,
        "runtime-1",
        1,
        u64::MAX,
    );
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realize-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test").unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let staging = McpStagingActivity::default();
    let projection = McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: None,
        native_wiring: None,
        mcp_process: None,
        staging: Some(staging.clone()),
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staging,
    };
    host.run_assembly
        .insert_mcp_projection(projection.clone())
        .unwrap();

    let sandbox_generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("mcp-staging-quiescence"),
        1,
        100,
        "environment",
        "base",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace-a",
        &awaken_session_types::SessionId::from("mcp-staging-quiescence"),
        "suspend",
        &sandbox_generation,
        3,
        None,
        None,
    );
    host.run_assembly
        .installed_sessions
        .realizations()
        .close_mcp_realization_admission(
            "mcp-staging-quiescence",
            awaken_session_mcp::McpQuiescenceAdmissionFence::new(
                &operation,
                "source-effect",
                "source-binding",
                &sandbox_generation,
            ),
        )
        .unwrap();

    // Cause/effect decision table:
    // | Rule | Stage boundary vs close | Drain/staging result | Effect |
    // | S1 | insert commit after close | not owned | reject; residual set cannot grow |
    // | S2 | inserted before close; spawn commits after close | drain cancelled | tracked Draining process retained; no proof |
    // | S3 | publish/renew after close | not applicable | reject without visibility/claim mutation |
    // | S4 | tracked staging finishes; retry | reap succeeds | Removed + exact proof |
    // | S5 | Unmaterialized projection before/after S4 | residual / none | stay closed / reopen and admit a new stage |
    host.run_assembly
        .install_session_environment_owner_projection(
            "mcp-staging-quiescence",
            "workspace-a",
            &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
        )
        .unwrap();
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .realizations()
            .mcp_realization_admitted("mcp-staging-quiescence")
            .expect("installed Sessions"),
        "S5 residual staging owner keeps expiry projection closed"
    );
    let mut late_projection = projection.clone();
    late_projection.request.generation = late_projection
        .request
        .generation
        .with_persisted(|w| {
            w.generation = late_projection
                .request
                .generation
                .generation()
                .at_version(2)
        })
        .unwrap();
    late_projection.receipt.generation = late_projection.request.generation.clone();
    let late_generation = late_projection.request.generation.clone();
    assert_eq!(
        host.run_assembly
            .insert_mcp_projection(late_projection)
            .unwrap_err()
            .code,
        "session_environment_quiescing",
        "S1 late stage commit is rejected by the closed fence, not by an existing owner"
    );
    assert!(
        host.run_assembly.mcp_projection(&late_generation).is_none(),
        "S1 rejected commit cannot grow the residual owner set"
    );
    assert!(
        host.run_assembly
            .publish_mcp_projection(&generation)
            .await
            .is_err(),
        "S3 publish fenced"
    );
    assert!(
        host.run_assembly
            .renew_mcp_projection(
                &host
                    .run_assembly
                    .mcp_projection(&generation)
                    .unwrap()
                    .request
            )
            .is_err(),
        "S3 renewal fenced"
    );
    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let draining = tokio::spawn(async move {
        drain_host
            .run_assembly
            .drain_mcp_projections("mcp-staging-quiescence", &[drain_generation])
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if host
                .run_assembly
                .mcp_projection(&generation)
                .is_some_and(|projection| projection.state == McpProjectionState::Draining)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reaped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        !host
            .run_assembly
            .attach_staging_mcp_process(
                &generation,
                Arc::new(Process {
                    reaped: reaped.clone(),
                }),
            )
            .unwrap(),
        "S2 spawn transfers to the Draining owner"
    );
    assert!(
        host.run_assembly
            .complete_staging_mcp_projection(&generation, awaken_session_mcp::McpWiring::empty())
            .is_err(),
        "S2 commit after close is rejected"
    );
    assert!(!draining.is_finished(), "S2");
    draining.abort();
    let _ = draining.await;
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "S2 cancellation retains owner"
    );
    assert!(
        host.run_assembly
            .mcp_projection(&generation)
            .unwrap()
            .mcp_process
            .is_some(),
        "S2 process owner retained"
    );
    staging.finish();
    let proof = host
        .run_assembly
        .drain_mcp_projections("mcp-staging-quiescence", std::slice::from_ref(&generation))
        .await
        .unwrap();
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&generation),
        "S4"
    );
    assert_eq!(
        host.run_assembly.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "S4"
    );
    assert!(reaped.load(Ordering::SeqCst), "S4 process reaped");
    // S6: completed cleanup alone does not reopen realization admission. Both
    // exact replay and lease-only retry retain the tombstone until S5's owner
    // transition clears the same quiescence fence.
    let removed_request = host
        .run_assembly
        .mcp_projection(&generation)
        .unwrap()
        .request;
    assert!(
        !host
            .run_assembly
            .forget_replayable_removed_mcp_projection(&removed_request),
        "S6 exact replay fenced"
    );
    let mut renewed_removed_request = removed_request.clone();
    renewed_removed_request.generation = renewed_removed_request.generation.expiring_at(u64::MAX);
    renewed_removed_request
        .stage_idempotency_key
        .push_str("-renewed");
    assert!(
        !host
            .run_assembly
            .forget_replayable_removed_mcp_projection(&renewed_removed_request),
        "S6 renewed retry fenced"
    );
    host.run_assembly
        .install_session_environment_owner_projection(
            "mcp-staging-quiescence",
            "workspace-a",
            &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
        )
        .unwrap();
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .mcp_realization_admitted("mcp-staging-quiescence")
            .expect("installed Sessions"),
        "S5 exact source-free projection reopens only after residual cleanup"
    );
    assert!(
        host.run_assembly
            .forget_replayable_removed_mcp_projection(&removed_request),
        "S5 old Removed owner can be forgotten after reopen"
    );
    host.run_assembly
        .insert_mcp_projection(projection)
        .expect("S5 new realization is admitted after exact reopen");
}

/// MCP-effect authority cause/effect graph: C1 the asserted generation lease is
/// live; C2 the local Control projection has the same Runtime incarnation and
/// epoch; C3 that projection monotonically extends the asserted expiry; C4 the
/// projected lease is live. Effects: E1 permit the already-admitted effect; E2
/// fence it before any MCP I/O.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | A1 | yes | absent | absent | absent | E1 (direct assertion) |
/// | A2 | no | yes | yes | yes | E1 |
/// | A3 | no | no | any | yes | E2 |
/// | A4 | no | yes | no | yes | E2 |
/// | A5 | no | yes | yes | no | E2 |
/// | A6 | no | absent | absent | absent | E2 |
/// | A7 | yes | no | any | any | E2 (root overrides live assertion) |
/// | A8 | yes | yes | no OR C4=no | any | E2 |
#[tokio::test]
async fn mcp_effect_authority_accepts_only_a_live_assertion_or_its_live_same_epoch_renewal() {
    let base = awaken_session_realization::test_support::wall_clock_now_ms() + 60_000;
    let generation = crate::test_mcp::mcp_fence(
        "mcp-effect-authority",
        "browser",
        1,
        "runtime-a/boot-1",
        3,
        base + 101,
    );
    let live = host_with_process_reaper().await;
    assert!(
        live.run_assembly
            .mcp_generation_is_authorized_at(&generation, base + 100)
            .expect("healthy registry"),
        "A1/E1"
    );

    let mut expired = generation.clone();
    expired = expired.expiring_at(base + 90);
    for (rule, lease, expected) in [
        (
            "A2",
            Some(crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: base + 110,
            }),
            true,
        ),
        (
            "A3",
            Some(crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-b/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: base + 110,
            }),
            false,
        ),
        (
            "A4",
            Some(crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: base + 80,
            }),
            false,
        ),
        (
            "A5",
            Some(crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: base + 100,
            }),
            false,
        ),
        ("A6", None, false),
    ] {
        let host = host_with_process_reaper().await;
        let has_root = lease.is_some();
        if let Some(lease) = lease {
            // A4/A5 keep exact generation bytes but carry exhausted local
            // observations. Managed root admission must not use Worker wall time.
            let proof = if matches!(rule, "A4" | "A5") {
                awaken_session_realization::test_support::test_expired_observed_lease(lease)
            } else {
                awaken_session_realization::test_support::test_observed_lease(lease)
            };
            host.run_assembly.installed_sessions.install_realization(
                expired.session_id().as_str(),
                |r| {
                    r.execution_mut().authority =
                        crate::realization_state::SessionRealizationAuthority::Live(proof);
                },
            );
        }
        assert_eq!(
            host.run_assembly
                .mcp_generation_is_authorized_at(&expired, base + 100)
                .expect("healthy registry"),
            expected,
            "{rule}"
        );
        if has_root {
            assert_eq!(
                host.run_assembly
                    .mcp_generation_is_authorized_at(&generation, base + 100)
                    .expect("healthy registry"),
                expected,
                "A7/A8 {rule}: a future assertion cannot override the root"
            );
        }
    }
}

#[tokio::test]
async fn mcp_projection_renewal_updates_the_same_staged_or_active_slot() {
    use awaken_session_configuration_contract::mcp::{McpRealizationReceipt, StageMcpAttachment};
    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

    /* Runtime renewal cause/effect decision table. C1 the same logical
     * generation/incarnation/epoch and immutable binding exists; C2 its state is
     * Staged or Active; C3 expiry advances. E1 rewrites that one projection and
     * receipt without reconnecting; E2 no second slot appears. R1 C1+C2(Staged)+
     * C3=>E1+E2 closes renewal during first Stage; R2 C1+C2(Active)+C3=>E1+E2
     * covers steady renewal; R3 no matching projection=>no-op. Conflicting
     * bindings and non-monotonic expiry are rejected by the aggregate N2–N5
     * table and this adapter's shared conflict gate. R4 a Session-only lease
     * extension followed by publication of a longer MCP ref must fail: the
     * exact staged receipt is still old. R5 after the ordinary renewal Stage,
     * exact publication succeeds; neither phase creates a second projection.
     * This is the Runtime half of Session activation's F1/F2 catch-up table. */
    for (rule, state) in [
        ("R1", McpProjectionState::Staged),
        ("R2", McpProjectionState::Active),
    ] {
        let host = host_with_process_reaper().await;
        let old_generation = crate::test_mcp::mcp_fence(
            SessionId::from(format!("renew-{rule}")),
            "browser",
            1,
            "runtime-a/boot-1",
            3,
            u64::MAX - 1,
        );
        let request = StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: old_generation.clone(),
            realization_id: "realization-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "browser".into(),
            target: awaken_agent_contract::McpTarget::parse_http(
                "https://browser.example.test/mcp",
            )
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        host.run_assembly
            .insert_mcp_projection(McpGenerationProjection {
                receipt: McpRealizationReceipt {
                    receipt_fingerprint: request.fingerprint(),
                    generation: old_generation.clone(),
                    realization_id: request.realization_id.clone(),
                    selected_plaintext_holder: None,
                    actual_realization_kind: None,
                },
                request: request.clone(),
                server: None,
                native_wiring: None,
                mcp_process: None,
                staging: None,
                drain: Arc::new(tokio::sync::Mutex::new(())),
                state,
            })
            .unwrap();
        let mut renewed = request;
        renewed.generation = renewed.generation.expiring_at(u64::MAX);
        renewed.stage_idempotency_key = "renew-max".into();
        // R6: an otherwise valid renewed request must never forget an effect
        // that is still Staging, Staged, Active or Draining. Only Removed has
        // cleanup proof; restore the fixture state for ordinary renewal below.
        for live_state in [
            McpProjectionState::Staging,
            McpProjectionState::Staged,
            McpProjectionState::Active,
            McpProjectionState::Draining,
        ] {
            host.run_assembly
                .installed_sessions
                .realizations()
                .modify_realization(old_generation.session_id().as_str(), |r| {
                    r.staging_mut().mcp.projections_mut()[0].state = live_state
                })
                .expect("installed Sessions");
            assert!(
                !host
                    .run_assembly
                    .forget_replayable_removed_mcp_projection(&renewed),
                "R6 {live_state:?}"
            );
            assert!(
                host.run_assembly.mcp_projection(&old_generation).is_some(),
                "R6 retains owner"
            );
        }
        host.run_assembly
            .installed_sessions
            .realizations()
            .modify_realization(old_generation.session_id().as_str(), |r| {
                r.staging_mut().mcp.projections_mut()[0].state = state
            })
            .expect("installed Sessions");
        host.run_assembly
            .install_session_realization_lease(
                old_generation.session_id().as_str(),
                crate::session_resource::SessionRealizationLease {
                    owner: "worker-a".into(),
                    runtime_incarnation: old_generation.runtime_incarnation().to_owned(),
                    epoch: old_generation.lease_epoch().get(),
                    expires_at_unix_ms: u64::MAX,
                },
            )
            .unwrap();
        assert!(
            host.run_assembly
                .publish_mcp_projection(&renewed.generation)
                .await
                .is_err(),
            "R4 Session renewal alone is not an exact MCP receipt"
        );
        let receipt = host
            .run_assembly
            .renew_mcp_projection(&renewed)
            .expect(rule)
            .expect(rule);
        assert_eq!(receipt.generation, renewed.generation, "{rule}/E1");
        assert!(
            host.run_assembly.mcp_projection(&old_generation).is_none(),
            "{rule}/E1"
        );
        assert_eq!(
            host.run_assembly
                .mcp_projection(&renewed.generation)
                .expect(rule)
                .state,
            state,
            "{rule}/E1"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(renewed.generation.session_id().as_str(), |r| {
                    r.staging().mcp.projection_count()
                })
                .expect("installed Sessions"),
            Some(1),
            "{rule}/E2"
        );
        host.run_assembly
            .publish_mcp_projection(&renewed.generation)
            .await
            .expect("R5 exact renewed projection publishes");
        assert_eq!(
            host.run_assembly
                .mcp_projection(&renewed.generation)
                .unwrap()
                .state,
            McpProjectionState::Active,
            "R5"
        );
    }

    let absent = host_with_process_reaper().await;
    let request = StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: crate::test_mcp::mcp_fence(
            "renew-absent",
            "browser",
            1,
            "runtime-a/boot-1",
            3,
            u64::MAX,
        ),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "renew-max".into(),
        name: "browser".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://browser.example.test/mcp")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    assert_eq!(
        absent.run_assembly.renew_mcp_projection(&request).unwrap(),
        None,
        "R3"
    );
}

/// Relay effects are created only by MCP staging. Publication may expose an
/// exact staged effect, but must never manufacture the missing route as a
/// compatibility/recovery path.
#[tokio::test]
async fn authenticated_acp_publication_requires_the_exact_staged_relay_route() {
    use awaken_session_configuration_contract::mcp::McpRealizationReceipt;
    use awaken_session_mcp::McpTransportMaterial;
    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};

    let generation = crate::test_mcp::mcp_fence(
        "mcp-stage-authority",
        "mcp-docs",
        1,
        "runtime-1",
        4,
        u64::MAX,
    );
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("secret")),
            refresh: None,
        },
    };
    let projection = McpGenerationProjection {
        request: crate::session_mcp::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "docs".into(),
            target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        },
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: "receipt-1".into(),
        },
        server: Some(server.clone()),
        // ACP has no in-process MCP connection. This is the stable transport
        // discriminator used by the private Host projection.
        native_wiring: None,
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staged,
    };

    // Cause graph:
    // authenticated ACP projection + no relay/route -> reject publication;
    // stage exact private route -> publish exact generation -> active.
    //
    // | Rule | relay | exact route | publish | projection state |
    // |---|---|---|---|---|
    // | S1 | absent | absent | reject | staged |
    // | S2 | present | absent | reject | staged |
    // | S3 | present | present | accept | active |
    let host = host_with_process_reaper().await;
    host.run_assembly.insert_mcp_projection(projection).unwrap();
    assert!(
        host.run_assembly
            .publish_mcp_projection(&generation)
            .await
            .is_err(),
        "S1"
    );
    assert!(
        host.run_assembly
            .active_mcp_projections(generation.session_id().as_str())
            .expect("registry and projection are healthy")
            .is_empty(),
        "S1"
    );

    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    assert!(
        host.run_assembly.mcp.install_relay(relay.clone()),
        "S2 setup"
    );
    assert!(
        host.run_assembly
            .publish_mcp_projection(&generation)
            .await
            .is_err(),
        "S2"
    );
    assert!(
        host.run_assembly
            .active_mcp_projections(generation.session_id().as_str())
            .expect("registry and projection are healthy")
            .is_empty(),
        "S2"
    );

    relay.set_route(&generation, &server);
    host.run_assembly
        .publish_mcp_projection(&generation)
        .await
        .expect("S3");
    assert_eq!(
        host.run_assembly
            .active_mcp_projections(generation.session_id().as_str())
            .expect("registry and projection are healthy")
            .len(),
        1,
        "S3"
    );
}

#[tokio::test]
async fn worker_authority_loss_revokes_every_session_projection() {
    use awaken_session_configuration_contract::mcp::McpRealizationReceipt;
    use awaken_session_mcp::{McpGenerationProjection, McpProjectionState};
    use awaken_session_mcp::{McpTransportMaterial, McpWiring};

    // Cause graph: unprovable Worker authority -> enumerate the canonical
    // process-local Session projections -> nonterminal realization revocation ->
    // route, material, and slot absent while durable environments remain owned
    // by the Session. Repeating the fence is idempotent.
    //
    // | Rule | Authority | Live projections | Effect |
    // |---|---|---|---|
    // | A1 | lost/unprovable | one | detach + revoke route + remove slot |
    // | A2 | lost/unprovable | none | zero/no-op |
    let host = host_with_process_reaper().await;
    let generation = crate::test_mcp::mcp_fence(
        "authority-loss",
        "mcp-docs",
        1,
        "worker-a/boot-1",
        3,
        u64::MAX,
    );
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: awaken_session_mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("secret")),
            refresh: None,
        },
    };
    host.run_assembly
        .insert_mcp_projection(McpGenerationProjection {
            request: crate::session_mcp::StageMcpAttachment {
                workspace_id: "workspace-a".into(),
                generation: generation.clone(),
                realization_id: "realize-1".into(),
                stage_idempotency_key: "stage-1".into(),
                name: "docs".into(),
                target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test")
                    .unwrap(),
                prompts_as_skills: false,
                credential: None,
                selected_plaintext_holder: None,
            },
            receipt: McpRealizationReceipt {
                generation: generation.clone(),
                realization_id: "realize-1".into(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
                receipt_fingerprint: "receipt-1".into(),
            },
            server: Some(server.clone()),
            native_wiring: Some(McpWiring::empty()),
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Staged,
        })
        .unwrap();
    host.run_assembly
        .publish_mcp_projection(&generation)
        .await
        .unwrap();
    let relay = awaken_session_mcp::McpRelay::start().await.unwrap();
    relay.set_route(&generation, &server);
    assert!(
        host.run_assembly.mcp.install_relay(relay.clone()),
        "A1 setup"
    );
    assert!(relay.route_url(&generation).is_some(), "A1 setup");

    assert_eq!(
        host.run_assembly
            .revoke_all_session_realizations()
            .await
            .unwrap(),
        1,
        "A1"
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .contains("authority-loss"),
        "A1"
    );
    assert!(relay.route_url(&generation).is_none(), "A1");
    assert_eq!(
        host.run_assembly
            .revoke_all_session_realizations()
            .await
            .unwrap(),
        0,
        "A2"
    );
}

/// Repository credentials remain owned by Resource realization. They must not
/// create a hidden MCP desired-state or credential path beside Session MCP
/// generations.
#[tokio::test]
async fn a_github_repository_resource_does_not_create_a_parallel_mcp_projection() {
    use awaken_session_configuration_contract::SessionInit;
    let dependencies = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_mcp_process_reaper(Arc::new(awaken_run_executor_acp::Supervisor));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: dependencies.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_secret_token")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let host = (crate::host::with_test_resource_validation(dependencies)
        .with_credentials(credentials, secrets))
    .build()
    .unwrap();
    let managed = managed_with_resource_source(host.clone());

    managed
        .run_assembly
        .install_test_session_init(
            "t-gh",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0.clone()),
                ),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    // The repo is staged for a host-side clone...
    assert_eq!(
        host.realization_executor
            .staging()
            .thread_repository_activations("t-gh")
            .len(),
        1,
        "repo staged for cloning"
    );

    let activations = host
        .realization_executor
        .staging()
        .thread_repository_activations("t-gh");
    let activation = &activations[0];
    assert_eq!(
        activation
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.as_str()),
        Some(credential.id.0.as_str()),
        "the activation retains only the exact secret-free source pin"
    );
    assert!(
        host.run_assembly
            .active_mcp_projections("t-gh")
            .expect("registry and projection are healthy")
            .is_empty(),
        "Repository realization cannot manufacture Session MCP authority"
    );
}
