use super::*;

#[tokio::test]
async fn realized_and_deferred_hand_share_the_same_pre_entry_process_domain() {
    use awaken_runtime_contract::tool_batch::ToolExecutionDomain;
    // C1 provider has exact retained binding / no evidence / read failure;
    // C2 first or repeated metadata read through environment/direct Hand.
    // R1 exact => same controlled coordinate at both entrances; R2 absent =>
    // opaque, never Worker; R3 failure => unavailable before tool dispatch.
    // Every row must perform zero Hand launches/channel opens: metadata is not
    // execution, and reacquisition must not silently widen the durable domain.
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let executor = environment.agent_tools().tool_executor();
    let call = ToolCall {
        call_id: "domain".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({}),
    };
    for row in ["exact", "absent", "error"] {
        {
            let mut shared = provider.shared.lock().unwrap();
            shared.remove("__process_binding");
            shared.remove("__process_binding_error");
            if row == "exact" {
                shared.insert("__process_binding".into(), b"retained-pod-uid".to_vec());
            } else if row == "error" {
                shared.insert("__process_binding_error".into(), Vec::new());
            }
        }
        for _ in 0..2 {
            let environment_domain = environment.agent_tools().tool_execution_domain().await;
            let hand_domain = executor.execution_domain(&call).await;
            if row == "error" {
                assert!(environment_domain.is_err(), "R3");
                assert!(
                    matches!(
                        hand_domain,
                        Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
                    ),
                    "R3"
                );
            } else {
                let expected = if row == "exact" {
                    ToolExecutionDomain::ControlledProcess {
                        provider_ref: "kubernetes-sandbox".into(),
                        execution_ref: "retained-pod-uid".into(),
                    }
                } else {
                    ToolExecutionDomain::Opaque {}
                };
                assert_eq!(environment_domain.unwrap(), expected, "{row}");
                assert_eq!(hand_domain.unwrap(), expected, "{row}");
            }
        }
    }
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 0);
}
