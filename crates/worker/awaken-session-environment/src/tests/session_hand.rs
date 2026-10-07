//! Session hand lifecycle: detach/adoption, reacquisition, hibernation, and quiescence.

use super::*;

struct BlockingHandFactory {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    block_once: Arc<std::sync::atomic::AtomicBool>,
}

struct BlockingHandExecutor {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    block_once: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolExecutor for BlockingHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError> {
        if self.block_once.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(awaken_tool_contract::tool::ToolOutput::ok(
            &call.call_id,
            "completed",
        ))
    }
}

impl HandExecutorFactory for BlockingHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        Arc::new(BlockingHandExecutor {
            started: self.started.clone(),
            release: self.release.clone(),
            block_once: self.block_once.clone(),
        })
    }
}

#[tokio::test]
async fn resident_hand_survives_worker_detach_and_is_reopened_on_adoption() {
    /*
     * Resident-Hand HA cause/effect graph and decision table.
     * Causes: C1 residency=resident; C2 Session Pod exists; C3 original
     * Worker attachment closes; C4 replacement Worker adopts the handle;
     * C5 Pod/Hand itself fails. Constraints: C4 requires C2; C5 excludes
     * healthy adoption. Effects: E1 no attached `hand --stdio` child; E2
     * lazily open one provider-owned Pod channel per Worker binding on first
     * tool demand; E3 advertise DurableRequest recovery before that bind; E4
     * detaching does not signal the resident process; E5 Pod/Hand failure is
     * not masked as Worker recovery.
     * Rules: RH1 C1+C2=>E1+E3; RH2 RH1+tool demand=>E2;
     * RH3 C1+C2+C3+C4+tool demand=>E2+E4;
     * RH4 C5=>E5 (covered by channel/open failure tests).
     * FMECA: Worker crash loses only the ephemeral channel (severity 2,
     * detectable by channel failure); re-adoption/open-channel is the
     * mitigation. A Pod/Hand crash remains severity 4 and requires workload
     * recovery, not a duplicate Worker-side Hand.
     */
    let provider = Arc::new(FakeContainerProvider::default());
    let factory =
        ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
    let environments = SandboxEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
        provider.clone(),
        None,
        Vec::new(),
        factory,
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::ZERO,
        awaken_session_environment_contract::ContainerHandResidency::Resident,
        awaken_time::system_clock(),
    )
    .into_session_provider();

    let original = environments.create(&spec()).await.unwrap();
    let handle = original.lifecycle().handle();
    assert_eq!(
        original
            .agent_tools()
            .tool_executor()
            .recovery_capability("write"),
        awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest
    );
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 0);
    original
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "original-resident-bind".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 1);

    original.lifecycle().stop_bound_processes().await.unwrap();
    let adopted = environments.adopt(&spec(), &handle).await.unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 1);
    assert_eq!(provider.renews.load(Ordering::SeqCst), 1);
    assert_eq!(
        adopted
            .agent_tools()
            .tool_executor()
            .recovery_capability("bash"),
        awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest
    );
    adopted
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "adopted-resident-bind".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 2);
    adopted.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn container_hand_reacquires_only_for_a_proven_pre_dispatch_failure() {
    /*
     * Container-Hand recovery cause/effect decision table.
     * Causes: C1 first binding succeeds or is unavailable before dispatch;
     * C2 one or two calls arrive; C3 replacement succeeds or is also
     * unavailable; C4 failure occurs after dispatch (indeterminate); C5 the
     * replacement process cannot start; C6 the owner is already closed.
     * Effects: E1 use the resident Hand without spawning; E2 stop the expired
     * binding, spawn exactly one replacement, and safely retry; E3 serialize
     * concurrent recovery behind that one replacement; E4 return after one
     * bounded retry; E5 never replay an indeterminate call; E6 propagate a
     * replacement-start failure without a loop; E7 never restart after close;
     * E8 idle hibernation and its stale-timer races are covered separately.
     * Rules: H1 success=>E1; H2 unavailable+C2+C3(success)=>E2+E3;
     * H3 unavailable+C3(unavailable)=>E4; H4 C4=>E5; H5 C5=>E6;
     * H6 C6=>E7.
     */
    let provider = Arc::new(FakeContainerProvider::default());
    let stable = ScriptedHandFactory::new([ScriptedHandOutcome::Success]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        stable.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.agent_tools().tool_executor();
    tokio::task::yield_now().await;
    assert_eq!(
        hand.invoke(&ToolCall {
            call_id: "stable".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap()
        .text(),
        "recovered"
    );
    assert_eq!(stable.binds.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
    assert!(matches!(
        hand.invoke(&ToolCall {
            call_id: "closed".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await,
        Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
    ));
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a closed owner never launches another Hand"
    );
    environment.lifecycle().dispose().await.unwrap();

    let provider = Arc::new(FakeContainerProvider::default());
    let recover = ScriptedHandFactory::new([
        ScriptedHandOutcome::UnavailableBeforeDispatch,
        ScriptedHandOutcome::Success,
    ]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        recover.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.agent_tools().tool_executor();
    let left = ToolCall {
        call_id: "left".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    };
    let right = ToolCall {
        call_id: "right".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    };
    let (left, right) = tokio::join!(hand.invoke(&left), hand.invoke(&right));
    assert_eq!(left.unwrap().text(), "recovered");
    assert_eq!(right.unwrap().text(), "recovered");
    assert_eq!(recover.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "concurrent callers share one replacement"
    );
    environment.lifecycle().dispose().await.unwrap();

    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .fail_hand_spawn_at
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let failed_replacement =
        ScriptedHandFactory::new([ScriptedHandOutcome::UnavailableBeforeDispatch]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        failed_replacement.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let error = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "replacement-spawn-failure".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect_err("a replacement spawn failure is propagated");
    assert!(matches!(
        error,
        awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(ref message)
            if message.contains("failed to reacquire Session hand")
    ));
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one initial spawn plus one failed replacement attempt"
    );
    assert_eq!(
        failed_replacement
            .binds
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a failed process spawn never creates an executor"
    );
    environment.lifecycle().dispose().await.unwrap();

    let provider = Arc::new(FakeContainerProvider::default());
    let bounded = ScriptedHandFactory::new([
        ScriptedHandOutcome::UnavailableBeforeDispatch,
        ScriptedHandOutcome::UnavailableBeforeDispatch,
        ScriptedHandOutcome::Success,
    ]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        bounded.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let error = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bounded".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect_err("a second dead channel ends the bounded retry");
    assert!(matches!(
        error,
        awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_)
    ));
    assert_eq!(bounded.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    environment.lifecycle().dispose().await.unwrap();

    let provider = Arc::new(FakeContainerProvider::default());
    let indeterminate = ScriptedHandFactory::new([ScriptedHandOutcome::Indeterminate]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        indeterminate.clone(),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let error = environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "indeterminate".into(),
            tool_id: "write".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect_err("a possibly executed call is never replayed");
    assert!(matches!(
        error,
        awaken_tool_contract::tool::ToolError::Indeterminate(_)
    ));
    assert_eq!(
        indeterminate
            .binds
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        provider
            .hand_spawns
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn live_projection_update_hibernates_and_fences_the_session_hand() {
    /*
     * Projection/Hand cause-effect graph and decision table.
     * Causes: C1=a bound Hand exists; C2=projection update begins; C3=the
     * update commits; C4=the update guard drops without commit; C5=a tool
     * arrives during/after the update. Effects: E1=wait for the in-flight
     * binding and reap it once; E2=reject C5 before dispatch while fenced;
     * E3=after C3 lazily launch exactly one Hand over the new projection;
     * E4=after C4 remain fenced until the authoritative retry commits.
     * Rules: U1 C1+C2=>E1+E2; U2 U1+C3+C5=>E3;
     * U3 U1+C4+C5=>E2+E4.
     */
    let provider = Arc::new(FakeContainerProvider::default());
    let factory = ScriptedHandFactory::new([
        ScriptedHandOutcome::Success,
        ScriptedHandOutcome::Success,
        ScriptedHandOutcome::Success,
    ]);
    let environment = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        factory,
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.agent_tools().tool_executor();
    hand.invoke(&ToolCall {
        call_id: "before-projection-update".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "U1/C1");

    let committed = environment
        .projection()
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("container Hand update");
    assert!(
        matches!(
            hand.invoke(&ToolCall {
                call_id: "during-update".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "U1/E2"
    );
    committed.commit();
    hand.invoke(&ToolCall {
        call_id: "after-commit".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U2/E3");

    let uncommitted = environment
        .projection()
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("container Hand update");
    drop(uncommitted);
    assert!(
        matches!(
            hand.invoke(&ToolCall {
                call_id: "after-failed-update".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "U3/E2,E4"
    );
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U3/E4");

    environment
        .projection()
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("retry update")
        .commit();
    hand.invoke(&ToolCall {
        call_id: "after-retry".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 3, "U3 retry");
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn live_projection_update_rejects_an_unreapable_hand() {
    // Projection-reap cause/effect rule. C1=a lazily bound Hand exists;
    // C2=its provider cannot prove reap; C3=projection update begins;
    // C4=the owning Environment is terminally disposed. E1=reject the update;
    // E2=fence later dispatch; E3=retain one process owner and never launch a
    // replacement; E4=still invoke the one physical Environment disposal
    // boundary exactly once. Rules UR1=C1+C2+C3=>E1+E2+E3;
    // UR2=UR1+C4=>E4 because physical disposal, unlike Hand replacement,
    // terminates the complete process substrate.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
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
    let hand = environment.agent_tools().tool_executor();
    hand.invoke(&ToolCall {
        call_id: "bind-unreapable-hand".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();

    let error = match environment
        .projection()
        .begin_live_projection_update()
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an unknown old-process outcome must reject the projection update"),
    };
    assert!(
        error
            .to_string()
            .contains("failed to reap Session hand before projection update")
    );
    assert!(matches!(
        hand.invoke(&ToolCall {
            call_id: "after-projection-reap-failure".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await,
        Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
    ));
    assert_eq!(
        provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "a failed projection reap never permits a replacement Hand"
    );
    environment.lifecycle().dispose().await.unwrap();
    assert_eq!(provider.disposals.load(Ordering::SeqCst), 1, "UR2/E4");
}

#[tokio::test]
async fn cancelled_projection_reap_retains_the_tracked_hand_owner() {
    // Cancelled-reap cause/effect rule. C1=a lazily bound Hand exists;
    // C2=projection retirement is in flight; C3=the update Future is
    // cancelled before wait proves exit. E1=retain the binding as the sole
    // process owner; E2=keep dispatch fenced; E3=do not spawn a replacement.
    // Rule CR1=C1+C2+C3=>E1+E2+E3.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__slow_reap_hand".into(), Vec::new());
    let environment = Arc::new(
        SandboxEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .into_session_provider()
        .create(&spec())
        .await
        .unwrap(),
    );
    environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-slow-reap-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    let updating = tokio::spawn({
        let environment = environment.clone();
        async move {
            let update = environment
                .projection()
                .begin_live_projection_update()
                .await?
                .expect("Container projection has a Hand fence");
            update.commit();
            Ok::<_, pc::SandboxError>(())
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    updating.abort();
    let _ = updating.await;

    let SandboxEnvironment::Container { hand, .. } = environment.sandbox_environment() else {
        panic!("test uses a Container environment")
    };
    assert!(
        hand.has_tracked_binding().await,
        "cancellation must retain the only known process owner"
    );
    assert!(hand.projection_is_updating());
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checkpoint_quiescence_fails_closed_when_attached_hand_cannot_be_reaped() {
    // Cause/effect graph: C2=attached Hand is tracked; C3=Supervisor cannot
    // prove reap. C2+C3 -> E1=no quiescence proof, E2=Ready owner retained,
    // E3=closed lifecycle prevents replacement. Decision rule QH1 covers
    // the unreapable terminal branch; the successful attached branch is
    // covered by the ordinary environment lifecycle tests above.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
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
    environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-unreapable-checkpoint-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();

    assert!(environment.lifecycle().quiesce().await.is_err(), "QH1/E1");
    let SandboxEnvironment::Container { hand, .. } = environment.sandbox_environment() else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QH1/E2");
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "QH1/E3");
}

#[tokio::test]
async fn checkpoint_quiescence_rejects_a_retained_resident_hand() {
    // Cause/effect graph: C1=tool demand has opened the lazy resident channel;
    // C2=resident Hand belongs to the Session Pod;
    // C3=checkpoint-and-release requests process quiescence; C4=the same
    // durable operation retries; C5=terminal disposal follows. C2+C3 ->
    // E1=first attempt has no proof while the resident binding and sandbox
    // remain live; C2+C3+C4 -> E2=retry remains Retained, never Vacant;
    // C5 -> E3=terminal stop may drop the channel before sandbox disposal.
    // Rules QR1/QR2/QR3 distinguish retry-stable checkpoint safety from the
    // existing terminal owner.
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SandboxEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
        provider.clone(),
        None,
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::ZERO,
        awaken_session_environment_contract::ContainerHandResidency::Resident,
        awaken_time::system_clock(),
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-resident-checkpoint-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();

    assert!(environment.lifecycle().quiesce().await.is_err(), "QR1/E1");
    let SandboxEnvironment::Container { hand, .. } = environment.sandbox_environment() else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QR1/E1");
    assert_eq!(
        environment.sandbox_environment().status().await.unwrap(),
        pc::SandboxStatus::Ready,
        "QR1/E1"
    );
    assert!(environment.lifecycle().quiesce().await.is_err(), "QR2/E2");
    assert!(hand.has_tracked_binding().await, "QR2/E2");
    assert_eq!(
        provider.resident_channel_opens.load(Ordering::SeqCst),
        1,
        "QR2/E2"
    );
    environment.lifecycle().dispose().await.unwrap();
    assert!(!hand.has_tracked_binding().await, "QR3/E3");
}

#[tokio::test]
async fn cancelled_checkpoint_quiescence_retains_the_attached_hand_owner() {
    // Cause/effect graph: C2=attached process; C6=quiescence Future is
    // cancelled during bounded reap. C2+C6 -> E1=no receipt and E2=the same
    // binding remains tracked for retry. Rule QC1 proves cancellation cannot
    // manufacture Vacant or admit a replacement.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__slow_reap_hand".into(), Vec::new());
    let environment = Arc::new(
        SandboxEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .into_session_provider()
        .create(&spec())
        .await
        .unwrap(),
    );
    environment
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-slow-checkpoint-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    let quiescing = tokio::spawn({
        let environment = environment.clone();
        async move { environment.lifecycle().quiesce().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    quiescing.abort();
    let _ = quiescing.await;

    let SandboxEnvironment::Container { hand, .. } = environment.sandbox_environment() else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QC1/E2");
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "QC1/E2");
}

/// Worker-local Hand inactivity cause/effect decision table.
/// C1=idle policy enabled; C2=deadline reached; C3=a newer invocation touches
/// the generation; C4=policy is zero; C5=invocation follows hibernation.
/// E1=keep one binding before the deadline; E2=stale deadline cannot stop a
/// newer generation; E3=deadline releases the Hand; E4=next call lazily
/// creates exactly one replacement; E5=zero disables hibernation; C6=the
/// provider cannot reap the expired process; E6=close the owner and never
/// launch a possibly concurrent replacement; C7=the Environment is terminally
/// disposed after C6; E7=invoke its one physical disposal boundary once. Rules:
/// I1 C1+!C2=>E1; I2 C1+C2+C3=>E2; I3 C1+C2+!C3=>E3;
/// I4 I3+C5=>E4; I5 C4=>E5; I6 C6=>E6; I7=I6+C7=>E7. This executes in the Runtime Host
/// without a Coordinator or durable Session scan, covering split deployment
/// ownership.
#[tokio::test(start_paused = true)]
async fn container_hand_hibernates_on_worker_local_inactivity_and_reacquires_once() {
    let provider = Arc::new(FakeContainerProvider::default());
    let factory =
        ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
    let environment = SandboxEnvironmentProvider::container_with_capacity_and_hand_idle(
        provider.clone(),
        None,
        Vec::new(),
        factory.clone(),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.agent_tools().tool_executor();
    hand.invoke(&ToolCall {
        call_id: "start-idle-window".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();

    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I1");

    hand.invoke(&ToolCall {
        call_id: "refresh-deadline".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I2");

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    hand.invoke(&ToolCall {
        call_id: "after-idle".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "I3+I4");
    assert_eq!(factory.binds.load(Ordering::SeqCst), 2, "I4");
    environment.lifecycle().dispose().await.unwrap();

    let disabled_provider = Arc::new(FakeContainerProvider::default());
    let disabled = SandboxEnvironmentProvider::container(
        disabled_provider.clone(),
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(3_600)).await;
    tokio::task::yield_now().await;
    disabled
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "disabled".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        disabled_provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "I5"
    );
    disabled.lifecycle().dispose().await.unwrap();

    let failed_provider = Arc::new(FakeContainerProvider::default());
    failed_provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
    let failed = SandboxEnvironmentProvider::container_with_capacity_and_hand_idle(
        failed_provider.clone(),
        None,
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    failed
        .agent_tools()
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-unreapable-idle-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        failed
            .agent_tools()
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "after-reap-failure".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
        Err(awaken_tool_contract::tool::ToolError::UnavailableBeforeDispatch(_))
    ));
    assert_eq!(failed_provider.hand_spawns.load(Ordering::SeqCst), 1, "I6");
    failed.lifecycle().dispose().await.unwrap();
    assert_eq!(failed_provider.disposals.load(Ordering::SeqCst), 1, "I7/E7");
}

/// Idle/invoke race rule I7: C1=the old deadline fires while a Hand call owns
/// the lifecycle mutex; C2=the call completes and advances its generation;
/// E1=the waiting timer observes the new generation and cannot reap the live
/// binding; E2=the next call reuses that same Hand without a second spawn.
#[tokio::test(start_paused = true)]
async fn an_idle_deadline_cannot_reap_a_concurrent_hand_invocation() {
    let provider = Arc::new(FakeContainerProvider::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let environment = SandboxEnvironmentProvider::container_with_capacity_and_hand_idle(
        provider.clone(),
        None,
        Vec::new(),
        Arc::new(BlockingHandFactory {
            started: started.clone(),
            release: release.clone(),
            block_once: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .into_session_provider()
    .create(&spec())
    .await
    .unwrap();
    tokio::task::yield_now().await;
    let hand = environment.agent_tools().tool_executor();
    let running_hand = hand.clone();
    let running = tokio::spawn(async move {
        running_hand
            .invoke(&ToolCall {
                call_id: "running-at-deadline".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
    });
    started.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    release.notify_one();
    assert_eq!(running.await.unwrap().unwrap().text(), "completed");
    tokio::task::yield_now().await;

    hand.invoke(&ToolCall {
        call_id: "reuse-after-race".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(
        provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "I7: a stale timer never forces replacement"
    );
    environment.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn adopting_a_container_renews_its_ownership_before_use() {
    let provider = Arc::new(FakeContainerProvider::default());
    let environments = SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_session_provider();
    let spec = spec();
    let effective_spec = environments.realizer().effective_spec(&spec).unwrap();
    let handle = current_fake_container_handle(&effective_spec);
    let adopted = environments
        .adopt(&spec, &handle)
        .await
        .expect("adopt environment");
    assert_eq!(
        provider.renews.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "adoption refreshes ownership before starting the replacement hand"
    );
    adopted.lifecycle().dispose().await.unwrap();
}

#[tokio::test]
async fn container_repository_transfer_reports_import_and_export_failures() {
    let source_root = tempfile::tempdir().expect("source root");
    let source = source_root.path().join("source");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .status()
            .expect("run git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    };
    git(&["init", "-q", source.to_str().unwrap()]);
    git(&[
        "-C",
        source.to_str().unwrap(),
        "config",
        "user.name",
        "fixture",
    ]);
    git(&[
        "-C",
        source.to_str().unwrap(),
        "config",
        "user.email",
        "fixture@example.invalid",
    ]);
    std::fs::write(source.join("README.md"), "fixture").unwrap();
    git(&["-C", source.to_str().unwrap(), "add", "README.md"]);
    git(&[
        "-C",
        source.to_str().unwrap(),
        "commit",
        "-q",
        "-m",
        "fixture",
    ]);

    let shared = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let container = FakeContainer {
        handle: current_fake_container_handle(&spec()),
        disposals: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        renews: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        hand_spawns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        resident_channel_opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_hand_spawn_at: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        shared: shared.clone(),
    };

    // Container Repository path rule: C1 a plan carries `/repo`; E1 reject
    // before host bundle acquisition/container exec, E2 never reinterpret it
    // as `/workspace/repo`.
    let unsupported_plan = pc::RepositoryRealizationPlan {
        repository_id: "unsupported".into(),
        mount_path: "/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let unsupported = container_repositories::provision(
        &container,
        "/opt/custom/awaken-sandbox",
        &unsupported_plan,
        None,
        &awaken_time::system_clock(),
    )
    .await
    .expect_err("C1/E1 unsupported path");
    assert!(
        unsupported
            .to_string()
            .contains("current providers require")
    );
    assert!(shared.lock().unwrap().is_empty(), "E1 no container effect");

    shared
        .lock()
        .unwrap()
        .insert("__fail_repository_import".into(), Vec::new());
    let import_plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let import_error = container_repositories::provision(
        &container,
        "/opt/custom/awaken-sandbox",
        &import_plan,
        None,
        &awaken_time::system_clock(),
    )
    .await
    .expect_err("a failed container import is not reported as provisioned");
    assert!(
        import_error
            .to_string()
            .contains("container Repository clone exited Some(23)")
    );

    {
        let mut state = shared.lock().unwrap();
        state.remove("__fail_repository_import");
        state.insert("__fail_repository_export".into(), Vec::new());
    }
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "main".into(),
        commit: "0000000000000000000000000000000000000000".into(),
        expected_prior_commit: None,
    };
    let export_error = container_repositories::push(
        &container,
        "/opt/custom/awaken-sandbox",
        &plan,
        &expectation,
        None,
        &awaken_time::system_clock(),
    )
    .await
    .expect_err("a failed container push is not published");
    assert!(
        export_error
            .to_string()
            .contains("container Repository push exited Some(19)")
    );

    // Container rejection decision table: C1 the process exits 42 with a
    // bounded third-ref observation, C2 the frozen create-only expectation
    // differs from that ref. E1 only the existing typed durable rejection is
    // returned; the preceding generic exit-19 failure remains unavailable.
    // | Rule | status | channel | result |
    // | R6 | 42 | canonical conflicting ref | Rejected(RemoteRefChanged) |
    // | R7 | 19 | empty | Unavailable (asserted above) |
    // | R8 | 42 | canonical different local HEAD | Rejected(LocalCommitMismatch) |
    let third = "3".repeat(40);
    {
        let mut state = shared.lock().unwrap();
        state.remove("__fail_repository_export");
        state.insert(
            "__repository_export_observation".into(),
            format!("REF:{third}\n").into_bytes(),
        );
    }
    let rejected = container_repositories::push(
        &container,
        "/opt/custom/awaken-sandbox",
        &plan,
        &expectation,
        None,
        &awaken_time::system_clock(),
    )
    .await
    .expect_err("R6 exact remote conflict is durable rejection evidence");
    assert!(matches!(
        rejected,
        pc::RepositoryPublicationError::Rejected(
            pc::RepositoryPublicationRejection::RemoteRefChanged { observed_commit }
        ) if observed_commit == third
    ));
    {
        let mut state = shared.lock().unwrap();
        state.insert(
            "__repository_export_observation".into(),
            format!("LOCAL:{third}\n").into_bytes(),
        );
    }
    let rejected = container_repositories::push(
        &container,
        "/opt/custom/awaken-sandbox",
        &plan,
        &expectation,
        None,
        &awaken_time::system_clock(),
    )
    .await
    .expect_err("R8 exact local HEAD conflict is durable rejection evidence");
    assert!(matches!(
        rejected,
        pc::RepositoryPublicationError::Rejected(
            pc::RepositoryPublicationRejection::LocalCommitMismatch { observed_commit }
        ) if observed_commit == third
    ));
}
