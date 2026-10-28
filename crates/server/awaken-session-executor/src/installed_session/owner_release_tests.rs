//! A real resident Context must not become its own installed-Session table owner.

use awaken_runtime_contract::execution::RunAttemptExecutor;
use std::sync::{Arc, Mutex, Weak};

#[tokio::test]
async fn idle_resident_context_releases_installed_sessions_when_last_host_owner_drops() {
    // Cause/effect graph: C1 normal idle Context is cached in the canonical
    // installed Session; C2 the last external Host/Context owners are dropped; C3 an active
    // physical attempt is absent. Effects: E1 the Host is released; E2 the same
    // installed Sessions and cached Context are released together without explicit eviction.
    // Decision rows O1=C1+!C2 => cached chain reachable; O2=C1+C2+C3 => E1+E2.
    // O3 post-capture fixture eviction is hygiene only and cannot satisfy O2.
    // Real assembly installs SessionContext -> Capture -> ArtifactHarvest into
    // both the direct driver and the claimed Worker. Observe only a Weak of the
    // inner chain via the existing decorator seam; this test adds no owner.
    let projection = Arc::new(Mutex::new(None::<Weak<dyn RunAttemptExecutor>>));
    let observed = projection.clone();
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .with_attempt_decorator(Arc::new(move |inner| {
        assert!(
            observed
                .lock()
                .unwrap()
                .replace(Arc::downgrade(&inner))
                .is_none()
        );
        inner
    }))
    .build()
    .expect("normal Host composition");
    let session = "idle-context-owner-release";
    let installed_sessions_before = Arc::strong_count(&host.run_assembly.installed_sessions);
    let context = host
        .run_assembly
        .ctx_for_session_coordination(session, None)
        .await
        .expect("normal idle coordination Context");
    let installed_sessions_after = Arc::strong_count(&host.run_assembly.installed_sessions);
    assert!(context.is_coordination_envelope());
    assert!(context.env.is_none(), "no physical Environment owner");
    assert!(!context.has_active_run(), "no active Run or attempt");
    assert!(context.config.resolved_spec.plugin_ids.is_empty());
    assert!(
        context.delivery.direct().is_some(),
        "final chain installed in direct driver"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(session, |run| run
                .context
                .as_ref()
                .is_some_and(|cached| Arc::ptr_eq(cached, &context)))
            .expect("installed Sessions")
            .unwrap(),
        "canonical installed Session owns this fully installed Context"
    );
    assert!(
        projection
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .is_some(),
        "the real assembled Session projection chain is reachable from the cache"
    );
    assert!(
        host.dispatch.pool().is_none(),
        "no process pool task started"
    );
    assert!(
        host.run_assembly.mcp.relay().is_none(),
        "no MCP relay task started"
    );
    let weak_host = Arc::downgrade(&host);
    let weak_installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    let weak_context = Arc::downgrade(&context);
    drop(context);
    drop(host);

    let host_released = weak_host.upgrade().is_none();
    let retained_installed_sessions = weak_installed_sessions.upgrade();
    let installed_sessions_released = retained_installed_sessions.is_none();
    let context_released = weak_context.upgrade().is_none();
    if let Some(installed_sessions) = retained_installed_sessions {
        // Capture the failure before cleanup. Remove the real resident Context
        // under its original mutex, and destroy the detached value outside it.
        let detached = installed_sessions
            .modify_run(session, |run| run.context.take())
            .expect("installed Sessions");
        drop(detached);
        drop(installed_sessions);
    }
    assert!(
        weak_context.upgrade().is_none() && weak_installed_sessions.upgrade().is_none(),
        "fixture cleanup must not leak the captured cycle"
    );
    assert!(host_released, "the last real Host owner was released");
    assert!(
        installed_sessions_released && context_released,
        "idle resident Context outlived its last real owner: slots_released={installed_sessions_released}, context_released={context_released}; assembly installed Sessions before={installed_sessions_before}, after={installed_sessions_after}; post-capture eviction is not owner-release success"
    );
}

#[tokio::test]
async fn retained_session_run_projection_callbacks_release_the_original_owners_and_idle_context() {
    // Cause/effect graph: C1 the real Host owns a cached idle Context; C2 only
    // its two projection callbacks remain after external Host/Context owners
    // drop; C3 a later callback reads the formerly known or an unknown Thread.
    // P6=C1+!C2 => original live projection; P7=C1+C2 => actual Host, registry,
    // capabilities, Context and realizer all release without cache eviction.
    // P8=C2+C3, both callbacks x both Threads => typed Unavailable, never local
    // fallback/Agent None, and no registry or Context resurrection. These are
    // original-owner lifetime observations, not a replacement registry fixture.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("real executor composition");
    let session = "retained-run-projection-owner";
    let context = host
        .run_assembly
        .ctx_for_session_coordination(session, None)
        .await
        .expect("real cached idle Context");
    assert!(
        context.is_coordination_envelope() && context.env.is_none() && !context.has_active_run()
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(session, |run| run
                .context
                .as_ref()
                .is_some_and(|cached| Arc::ptr_eq(cached, &context)))
            .unwrap()
            .unwrap(),
        "C1 exact cached Context"
    );
    // Reuse normal legacy Context assembly before selecting these read-only
    // admission inputs; no Agent publication or Environment fixture is needed.
    host.capabilities
        .register_thread_workspace(session, "retained-workspace")
        .unwrap();
    host.capabilities
        .register_thread_agent_projection(session, "retained-agent")
        .unwrap();
    let (workspace, agent) = host.capabilities.session_run_projection_callbacks();
    assert_eq!(workspace(session).unwrap(), "retained-workspace", "P6");
    assert_eq!(
        agent(session).unwrap().as_deref(),
        Some("retained-agent"),
        "P6"
    );
    assert!(
        host.dispatch.pool().is_none() && host.run_assembly.mcp.relay().is_none(),
        "C1 no active physical task"
    );
    let weak_host = Arc::downgrade(&host);
    let weak_installed_sessions = Arc::downgrade(&host.run_assembly.installed_sessions);
    let weak_capabilities = Arc::downgrade(&host.capabilities);
    let weak_context = Arc::downgrade(&context);
    let weak_realizer = Arc::downgrade(&host.realization_executor);
    drop(context);
    drop(host);
    assert!(weak_host.upgrade().is_none(), "P7 release original Host");
    assert!(
        weak_installed_sessions.upgrade().is_none(),
        "P7 release original registry"
    );
    assert!(
        weak_capabilities.upgrade().is_none(),
        "P7 release original capabilities"
    );
    assert!(
        weak_context.upgrade().is_none(),
        "P7 release original cached Context"
    );
    assert!(
        weak_realizer.upgrade().is_none(),
        "P7 release original realizer"
    );
    for thread in [session, "unknown-after-projection-owner-loss"] {
        for error in [
            workspace(thread).map(|_| ()).unwrap_err(),
            agent(thread).map(|_| ()).unwrap_err(),
        ] {
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "P8"
            );
            assert_eq!(
                error.code, "unavailable",
                "P8 existing typed owner-loss error"
            );
            assert_eq!(
                error.message, "Session projection owner is unavailable",
                "P8"
            );
        }
    }
    assert!(
        weak_host.upgrade().is_none()
            && weak_installed_sessions.upgrade().is_none()
            && weak_capabilities.upgrade().is_none()
            && weak_context.upgrade().is_none()
            && weak_realizer.upgrade().is_none(),
        "P8 reads cannot resurrect any original owner"
    );
}
