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
    // | P2/C11 | installed/empty/absent | Workspace helper + Host delegate | exact frozen Workspace wins; fallback creates nothing |
    // | V4   | installed | realization write / host write        | each visible to the other side |
    // | V5   | installed | run query                             | context, Session dispatch and claim as the host holds them |
    // | V6   | installed | drop_context                          | only the run context is dropped |
    // | V7   | installed | enter_session_dispatch                | only the Session-dispatch mark is set |
    // | V8   | installed | end_terminal_disposal                 | run context and Resource reconciliation end together; nothing else |
    // | S1/R4 | absent or installed | named requirements, no preparation | create/publish the correlated tuple, replace whole Frozen Arc |
    // | S2/R5 | installed | named requirements, missing/foreign/closed/cancelled/expired proof | authority_lost; every prior correlated field and Run preserved |
    // | S3/R6 | installed | exact live proof | publish under the same Slot guard; old Arc and Run preserved |
    // | S3/R12 | installed | callback returns Err | same Result, callback once; actual mutex stays healthy |
    // S4 actual Slot poison has its necessary case in installed_session/tests.rs.
    // Existing cold-resource IO tests own R1-R3/R7-R11; these rows isolate the
    // new named Source action's atomic publication, rather than mirror IO.
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
    assert_eq!(
        sessions.workspace(thread, "other-fallback"),
        "ws-view",
        "C11 installed Workspace"
    );
    assert_eq!(
        host.thread_workspace(thread),
        "ws-view",
        "C11 public thin delegate"
    );
    assert_eq!(
        sessions.workspace("created", "empty-fallback"),
        "empty-fallback",
        "C11 installed empty"
    );
    assert_eq!(
        sessions.workspace("unknown", host.local_workspace()),
        host.local_workspace(),
        "C11 absent fallback"
    );
    assert_eq!(
        host.thread_workspace("unknown"),
        host.local_workspace(),
        "C11 same public fallback"
    );
    assert!(
        !host.installed_sessions.contains("unknown"),
        "C11 no absent Slot created"
    );

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

    // R4/R6: the named action grants no generic mutable Frozen access. Ordinary
    // callbacks above retain their readonly borrow; this command alone replaces
    // the whole Arc and the correlated requirements tuple.
    bind_test_memory(&host, thread, "view-memory", true);
    let memory = sessions
        .read_realization(thread, |r| r.memory.clone())
        .flatten()
        .unwrap();
    let compiled = || crate::staged_resources::CompiledEffectiveInputs {
        staged: crate::staged_resources::StagedResources {
            prompts: vec!["view requirements".into()],
            ..Default::default()
        },
        memory_bindings: std::collections::HashMap::from([("view-binding".into(), memory.clone())]),
    };
    let before = sessions.frozen(thread);
    sessions
        .publish_resource_requirements(
            thread,
            "ws-published",
            None,
            compiled(),
            Vec::new(),
            "requirements-key".into(),
        )
        .unwrap();
    let published = sessions.frozen(thread);
    assert_eq!(
        sessions.workspace(thread, "ignored-fallback"),
        "ws-published",
        "C11 current whole Frozen Arc, no cached Workspace"
    );
    assert_eq!(
        host.thread_workspace(thread),
        "ws-published",
        "C11 delegate observes new publication"
    );
    assert!(
        !Arc::ptr_eq(&before, &published),
        "S1/R4 whole Frozen Arc replacement"
    );
    assert_eq!(
        before.workspace.as_deref(),
        Some("ws-view"),
        "S1 old reader remains stable"
    );
    assert_eq!(
        published.workspace.as_deref(),
        Some("ws-published"),
        "S1 workspace and requirements"
    );
    host.installed_sessions
        .read(thread, |slot| {
            assert_eq!(slot.realization.resources.prompts, ["view requirements"]);
            assert!(Arc::ptr_eq(
                &slot.realization.memory_bindings["view-binding"],
                &memory
            ));
            assert!(slot.realization.memory.is_none(), "S1 selection cleared");
            assert_eq!(
                slot.realization.skills,
                Some(Vec::new()),
                "S1 empty frozen Skills retained"
            );
            assert_eq!(
                slot.realization.staged_resource_effect_key.as_deref(),
                Some("requirements-key")
            );
            assert!(
                Arc::ptr_eq(slot.run.context.as_ref().unwrap(), &ctx),
                "S1 Run kept"
            );
        })
        .unwrap();

    let lease = crate::session_resource::SessionRealizationLease {
        owner: "view-worker".into(),
        runtime_incarnation: "view-worker/boot".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 30_000,
    };
    for rule in ["missing", "foreign", "closed", "cancelled", "expired"] {
        let token = Arc::new(awaken_runtime_contract::CancellationToken::new());
        host.installed_sessions.update(thread, |slot| {
            slot.realization.authority =
                crate::installed_session::SessionRealizationAuthority::Live(if rule == "expired" {
                    crate::installed_session::test_expired_observed_lease(lease.clone())
                } else {
                    crate::installed_session::test_observed_lease(lease.clone())
                });
            slot.realization.resource_preparation = Some(token.clone());
            slot.realization.memory = Some(memory.clone());
            if rule == "closed" {
                slot.realization.close_authority(None);
            }
        });
        if rule == "cancelled" {
            token.cancel();
        }
        let mut asserted = lease.clone();
        if rule == "foreign" {
            asserted.epoch = awaken_lease_contract::fence::Epoch::from_persisted(asserted.epoch)
                .unwrap()
                .next()
                .unwrap()
                .get();
        }
        let failed = sessions
            .publish_resource_requirements(
                thread,
                "forbidden-workspace",
                (rule != "missing").then_some(&asserted),
                crate::staged_resources::CompiledEffectiveInputs {
                    staged: Default::default(),
                    memory_bindings: Default::default(),
                },
                Vec::new(),
                "forbidden-key".into(),
            )
            .unwrap_err();
        assert_eq!(
            failed.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "S2/R5 {rule}"
        );
        assert_eq!(
            failed.code, "session_realization_authority_lost",
            "S2/R5 {rule}"
        );
        host.installed_sessions
            .read(thread, |slot| {
                assert!(Arc::ptr_eq(&slot.inputs, &published), "S2 Frozen {rule}");
                assert_eq!(
                    slot.realization.resources.prompts,
                    ["view requirements"],
                    "S2 resources {rule}"
                );
                assert!(
                    Arc::ptr_eq(&slot.realization.memory_bindings["view-binding"], &memory),
                    "S2 bindings {rule}"
                );
                assert!(
                    Arc::ptr_eq(slot.realization.memory.as_ref().unwrap(), &memory),
                    "S2 memory {rule}"
                );
                assert_eq!(
                    slot.realization.skills,
                    Some(Vec::new()),
                    "S2 Skills {rule}"
                );
                assert_eq!(
                    slot.realization.staged_resource_effect_key.as_deref(),
                    Some("requirements-key"),
                    "S2 key {rule}"
                );
                assert!(
                    Arc::ptr_eq(slot.run.context.as_ref().unwrap(), &ctx),
                    "S2 Run {rule}"
                );
            })
            .unwrap();
    }
    host.installed_sessions.update(thread, |slot| {
        slot.realization.authority = crate::installed_session::SessionRealizationAuthority::Live(
            crate::installed_session::test_observed_lease(lease.clone()),
        );
        slot.realization.resource_preparation =
            Some(Arc::new(awaken_runtime_contract::CancellationToken::new()));
    });
    sessions
        .publish_resource_requirements(
            thread,
            "ws-live",
            Some(&lease),
            compiled(),
            Vec::new(),
            "live-key".into(),
        )
        .unwrap();
    assert_eq!(
        published.workspace.as_deref(),
        Some("ws-published"),
        "S3/R6 prior Arc stable"
    );
    assert_eq!(
        sessions.frozen(thread).workspace.as_deref(),
        Some("ws-live"),
        "S3 exact live proof"
    );
    let mut callbacks = 0;
    let refused: Result<(), &str> = host
        .installed_sessions
        .modify(thread, |_| {
            callbacks += 1;
            Err("original callback error")
        })
        .unwrap();
    assert_eq!(
        refused,
        Err("original callback error"),
        "S3 callback Result unchanged"
    );
    assert_eq!(callbacks, 1, "S3 callback exactly once");
    assert!(
        host.installed_sessions.contains(thread),
        "S3 Err never poisons the lock"
    );
    // Drop only the preparation marker installed by the explicit Source rows;
    // subsequent V6-V8 keep their original semantics and state expectations.
    host.installed_sessions
        .update(thread, |slot| slot.realization.resource_preparation = None);

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
