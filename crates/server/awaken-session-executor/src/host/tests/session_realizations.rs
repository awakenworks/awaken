//! The realizer's `SessionRealizations` view over the installed Sessions.

use super::*;

#[tokio::test]
async fn session_realizations_reach_only_frozen_reads_realization_and_the_narrow_run_query() {
    // Decision table over one installed Session as the realizer sees it.
    // C1 the Session is installed / absent; C2 the access is `modify` (only an
    // installed Session) or `update` (installs an empty one first); C3 which
    // layer is touched. The view's types allow nothing else: frozen inputs are
    // a shared borrow, the run layer is the `SessionRunQuery` trait object.
    //
    // | Rule | C1        | C2 / C3                               | Effect |
    // |------|-----------|---------------------------------------|--------|
    // | V1   | absent    | modify                                | not visited, nothing installed |
    // | V2   | absent    | update                                | installed empty, then visited |
    // | V3   | installed | frozen read                           | the host's exact frozen value; absent reads empty |
    // | V4   | installed | realization write / host write        | each visible to the other side |
    // | V5   | installed | run query                             | context, Session dispatch and claim as the host holds them |
    // | V6   | installed | drop_context                          | only the run context is dropped |
    // | V7   | installed | enter_session_dispatch                | only the Session-dispatch mark is set |
    // | V8   | installed | end_terminal_disposal                 | run context and Resource reconciliation end together; nothing else |
    //
    // The host side is observed through its own full-slot accessors, never
    // through the view, so each row checks the shared state itself.
    let host = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .build()
        .expect("validated Host dependencies");
    let sessions = host.realizer.sessions.clone();

    assert!(sessions.modify_session("absent", |_| ()).is_none(), "V1");
    assert!(
        !host.installed_sessions.contains("absent"),
        "V1 nothing installed"
    );
    assert!(sessions.dispatch_claim("absent").is_none(), "V1 no claim");
    assert!(
        sessions.update_session("created", |access| access.realization.manifest.is_none()),
        "V2 empty"
    );
    assert!(host.installed_sessions.contains("created"), "V2 installed");

    let thread = "realizations-view";
    host.installed_sessions
        .replace_frozen_with(thread, |next| next.workspace = Some("ws-view".into()));
    assert!(
        Arc::ptr_eq(
            &sessions.frozen(thread),
            &host.installed_sessions.realizations().frozen(thread)
        ),
        "V3 exact frozen value"
    );
    assert_eq!(
        sessions.frozen(thread).workspace.as_deref(),
        Some("ws-view")
    );
    assert!(sessions.frozen("unknown").workspace.is_none(), "V3 absent");

    sessions.update_realization(thread, |r| r.staged_resource_effect_key = Some("k1".into()));
    assert_eq!(
        host.installed_sessions
            .read(thread, |slot| slot
                .realization
                .staged_resource_effect_key
                .clone())
            .flatten()
            .as_deref(),
        Some("k1"),
        "V4 realizer write"
    );
    host.installed_sessions.update(thread, |slot| {
        slot.realization.staged_resource_effect_key = Some("k2".into());
    });
    assert_eq!(
        sessions
            .read_realization(thread, |r| r.staged_resource_effect_key.clone())
            .flatten()
            .as_deref(),
        Some("k2"),
        "V4 host write"
    );

    let query = |sessions: &crate::session_realizations::SessionRealizations| {
        sessions
            .modify_session(thread, |access| {
                (
                    access.run.holds_context(),
                    access.run.session_dispatch(),
                    access.run.dispatch_claim().cloned(),
                )
            })
            .unwrap()
    };
    assert_eq!(query(&sessions), (false, false, None), "V5 empty run layer");
    let ctx = host.ctx_for(thread, None).await.expect("context");
    let claim = awaken_dispatch_contract::RunClaim {
        run_id: RunId("realizations-view-run".into()),
        owner: "worker".into(),
        epoch: 3,
    };
    let deferred: Arc<dyn awaken_tool_contract::tool::ToolExecutor> = Arc::new(
        crate::lazy_sandbox::DeferredSandboxExecutor::new(Arc::downgrade(&host), thread),
    );
    host.installed_sessions.update_run(thread, |run| {
        run.context = Some(ctx.clone());
        run.dispatch_claim = Some(claim.clone());
        run.deferred_executor = Some(deferred.clone());
    });
    assert_eq!(
        query(&sessions),
        (true, false, Some(claim.clone())),
        "V5 the host's run layer"
    );
    assert_eq!(sessions.dispatch_claim(thread), Some(claim.clone()), "V5");

    sessions.drop_run_context(thread);
    let (runtime, deferred_kept, kept_claim, session_dispatch) = host
        .installed_sessions
        .read_run(thread, |run| {
            (
                run.context.is_some(),
                run.deferred_executor.is_some(),
                run.dispatch_claim.clone(),
                run.session_dispatch,
            )
        })
        .unwrap();
    assert!(!runtime, "V6 context dropped");
    assert!(deferred_kept, "V6 deferred executor kept");
    assert_eq!(kept_claim, Some(claim.clone()), "V6 claim kept");
    assert!(!session_dispatch, "V6 dispatch mark untouched");

    sessions.enter_session_dispatch(thread);
    assert_eq!(query(&sessions), (false, true, Some(claim.clone())), "V7");
    assert!(
        host.installed_sessions
            .read_run(thread, |run| run.deferred_executor.is_some())
            .unwrap(),
        "V7 deferred executor kept"
    );

    host.installed_sessions.update(thread, |slot| {
        slot.run.context = Some(ctx.clone());
        slot.realization.environment_resource_reconciliation =
            crate::staged_resources::EnvironmentResourceReconciliation::Adopted;
    });
    let staged_before = host
        .installed_sessions
        .read(thread, |slot| {
            slot.realization.staged_resource_effect_key.clone()
        })
        .unwrap();
    sessions.end_terminal_disposal(thread);
    let after = host
        .installed_sessions
        .read(thread, |slot| {
            (
                slot.run.context.is_some(),
                slot.realization.environment_resource_reconciliation,
                slot.run.dispatch_claim.clone(),
                slot.run.session_dispatch,
                slot.realization.staged_resource_effect_key.clone(),
            )
        })
        .unwrap();
    assert_eq!(
        after,
        (
            false,
            crate::staged_resources::EnvironmentResourceReconciliation::None,
            Some(claim),
            true,
            staged_before,
        ),
        "V8"
    );
}
