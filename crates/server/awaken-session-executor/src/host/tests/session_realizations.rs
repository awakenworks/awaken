//! The realization executor's `SessionRealizations` view over the installed Sessions.

use super::*;

#[tokio::test]
async fn session_realizations_reach_only_frozen_reads_realization_and_the_narrow_run_query() {
    // Decision table over one installed Session as the realization executor sees it.
    // C1 the Session is installed / absent; C2 the access is `modify` (only an
    // installed Session) or `update` (installs an empty one first); C3 which
    // layer is touched. The view's types allow nothing else: frozen inputs are
    // a shared borrow, the run layer is the `SessionRunQuery` trait object.
    //
    // | Rule | C1        | C2 / C3                               | Effect |
    // |------|-----------|---------------------------------------|--------|
    // | V1   | absent    | modify                                | not visited, nothing installed |
    // | V2   | absent    | update                                | denied (SessionAbsent), nothing installed; after the install path, visited empty |
    // | V3   | installed | frozen read                           | the host's exact frozen value; absent reads empty |
    // | P2/C11 | installed/empty/absent | Workspace helper + Host delegate | exact frozen Workspace wins; fallback creates nothing |
    // | V4   | installed | realization write / host write        | each visible to the other side |
    // | V5   | installed | run query                             | context, Session dispatch and claim as the host holds them |
    // | V6   | installed | drop_context                          | only the run context is dropped |
    // | V7   | installed | enter_session_dispatch                | only the Session-dispatch mark is set |
    // | V8   | installed | end_terminal_disposal                 | run context and Resource reconciliation end together; nothing else |
    // | S1/R4 | absent    | named requirements                    | denied (SessionAbsent), nothing installed |
    // | S1/R4 | installed | named requirements, no preparation    | publish the correlated tuple, replace whole Frozen Arc |
    // | S2/R5 | installed | named requirements, missing/foreign/closed/cancelled/expired proof | authority_lost; every prior correlated field and Run preserved |
    // | S3/R6 | installed | exact live proof | publish under the same installed Session guard; old Arc and Run preserved |
    // | S3/R12 | installed | callback returns Err | same Result, callback once; actual mutex stays healthy |
    // S4 actual installed Session poison has its necessary case in installed_session/tests.rs.
    // Existing cold-resource IO tests own R1-R3/R7-R11; these rows isolate the
    // new named Source action's atomic publication, rather than mirror IO.
    //
    // The host side is observed through its own full-slot accessors, never
    // through the view, so each row checks the shared state itself.
    let host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
        .await
        .build()
        .expect("validated Host dependencies");
    let sessions = host.realization_executor.sessions().clone();

    assert!(
        sessions
            .modify_session("absent", |_| ())
            .expect("installed Sessions")
            .is_none(),
        "V1"
    );
    assert!(
        !host.run_assembly.installed_sessions.contains("absent"),
        "V1 nothing installed"
    );
    assert!(
        sessions
            .dispatch_claim("absent")
            .expect("installed Sessions")
            .is_none(),
        "V1 no claim"
    );
    assert!(
        matches!(
            sessions.update_session("created", |_| ()),
            Err(crate::realization_error::RealizationError::AuthorityLost {
                cause: crate::realization_error::AuthorityLoss::SessionAbsent,
                ..
            })
        ),
        "V2 a write to an absent Session is denied"
    );
    assert!(
        !host.run_assembly.installed_sessions.contains("created"),
        "V2 a write never installs"
    );
    host.run_assembly
        .installed_sessions
        .install("created")
        .expect("installed Sessions");
    assert!(
        sessions
            .update_session("created", |access| access
                .realization
                .staging()
                .manifest
                .is_none())
            .expect("installed Sessions"),
        "V2 empty"
    );
    assert!(
        host.run_assembly.installed_sessions.contains("created"),
        "V2 installed"
    );

    let thread = "realizations-view";
    host.run_assembly
        .installed_sessions
        .replace_frozen_with(thread, |next| next.workspace = Some("ws-view".into()))
        .expect("installed Sessions");
    assert!(
        Arc::ptr_eq(
            &sessions.frozen(thread).expect("installed Sessions"),
            &host
                .run_assembly
                .installed_sessions
                .realizations()
                .frozen(thread)
                .expect("installed Sessions")
        ),
        "V3 exact frozen value"
    );
    assert_eq!(
        sessions
            .frozen(thread)
            .expect("installed Sessions")
            .workspace
            .as_deref(),
        Some("ws-view")
    );
    assert!(
        sessions
            .frozen("unknown")
            .expect("installed Sessions")
            .workspace
            .is_none(),
        "V3 absent"
    );
    assert_eq!(
        sessions
            .workspace(thread, "other-fallback")
            .expect("installed Sessions"),
        "ws-view",
        "C11 installed Workspace"
    );
    assert_eq!(
        host.capabilities
            .thread_workspace(thread)
            .expect("healthy registry"),
        "ws-view",
        "C11 public thin delegate"
    );
    assert_eq!(
        sessions
            .workspace("created", "empty-fallback")
            .expect("installed Sessions"),
        "empty-fallback",
        "C11 installed empty"
    );
    assert_eq!(
        sessions
            .workspace("unknown", host.run_assembly.local_workspace())
            .expect("installed Sessions"),
        host.run_assembly.local_workspace(),
        "C11 absent fallback"
    );
    assert_eq!(
        host.capabilities
            .thread_workspace("unknown")
            .expect("healthy registry"),
        host.run_assembly.local_workspace(),
        "C11 same public fallback"
    );
    assert!(
        !host.run_assembly.installed_sessions.contains("unknown"),
        "C11 no absent installed Session created"
    );

    sessions
        .update_realization(thread, |r| r.staging_mut().effect_key = Some("k1".into()))
        .expect("installed Sessions");
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .read(thread, |installed| installed
                .realization
                .staging()
                .effect_key
                .clone())
            .expect("installed Sessions")
            .flatten()
            .as_deref(),
        Some("k1"),
        "V4 realizer write"
    );
    host.run_assembly
        .installed_sessions
        .install_with(thread, |installed| {
            installed.realization.staging_mut().effect_key = Some("k2".into());
        })
        .expect("installed Sessions");
    assert_eq!(
        sessions
            .read_realization(thread, |r| r.staging().effect_key.clone())
            .expect("installed Sessions")
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
            .expect("installed Sessions")
            .unwrap()
    };
    assert_eq!(query(&sessions), (false, false, None), "V5 empty run layer");
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("context");
    let claim = awaken_dispatch_contract::RunClaim {
        run_id: RunId("realizations-view-run".into()),
        owner: "worker".into(),
        epoch: 3,
    };
    let deferred: Arc<dyn awaken_tool_contract::tool::ToolExecutor> =
        Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
            Arc::downgrade(&host.run_assembly.installed_sessions),
            Arc::downgrade(&host.realization_executor),
            thread,
        ));
    host.run_assembly
        .installed_sessions
        .install_run(thread, |run| {
            run.context = Some(ctx.clone());
            run.dispatch_claim = Some(claim.clone());
            run.deferred_executor = Some(deferred.clone());
        });
    assert_eq!(
        query(&sessions),
        (true, false, Some(claim.clone())),
        "V5 the host's run layer"
    );
    assert_eq!(
        sessions.dispatch_claim(thread).expect("installed Sessions"),
        Some(claim.clone()),
        "V5"
    );
    // Q1-Q7 decision table: two REAL installed Session adapters x Primary(0/1) x all
    // DelegationStatus variants x child marker(0/1) = 24 rows. Completed skips
    // the child; Open and CancelRequested query it. Missing installed Session/context adds
    // the distinct false partitions. Q3 both Primary values + failed awaited
    // delegation must propagate the exact error BEFORE any child, never bypass
    // the await. Trace asserts query(root) -> poll(delegation) -> allowed child.
    // These are local marker observations, not physical-stop/cleanup evidence.
    // Bound this extended test's future layout at the existing heap boundary;
    // do not increase the default test thread stack for additional causes.
    Box::pin(async {
        use crate::session_realizations::primary_or_delegated_run_active;
        use awaken_agent_contract::agent::delegation::DelegationStatus as Status;
        use futures_util::FutureExt;
        let child_id = "realizations-query-child";
        let child = host
            .run_assembly.ctx_for(child_id, None)
            .await
            .expect("actual child context");
        host.run_assembly.installed_sessions
            .install_run(child_id, |run| run.context = Some(child.clone()));
        let delegated = |status| awaken_session_application_contract::DelegatedRun {
            run_id: RunId(child_id.into()),
            parent_call_id: "query-parent-call".into(),
            agent_id: "query-child-agent".into(),
            status,
        };
        for legacy in [false, true] {
            for primary_active in [false, true] {
                for (status, occupies) in [
                    (Status::Open, true),
                    (Status::Completed, false),
                    (Status::CancelRequested, true),
                ] {
                    for child_active in [false, true] {
                        ctx.foreground.install_for_test(primary_active.then(|| RunId("query-primary".into())));
                        child.foreground.install_for_test(child_active.then(|| RunId("query-child".into())));
                        let trace = std::sync::Mutex::new(Vec::new());
                        let result = primary_or_delegated_run_active(
                            thread,
                            |id| {
                                trace.lock().unwrap().push(format!("query:{id}"));
                                if legacy {
                                    Ok(host.run_assembly.installed_sessions
                                        .read_run(id, |run| {
                                            run.context
                                                .as_ref()
                                                .is_some_and(|runtime| runtime.has_active_run())
                                        }).expect("installed Sessions")
                                        .unwrap_or(false))
                                } else {
                                    Ok(sessions
                                        .modify_session(id, |installed| installed.run.has_active_run()).expect("installed Sessions")
                                        .unwrap_or(false))
                                }
                            },
                            async {
                                trace.lock().unwrap().push("delegate".into());
                                Ok::<_, crate::SessionExecutionError>(vec![delegated(status)])
                            },
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            result,
                            primary_active || (occupies && child_active),
                            "Q1-Q7 legacy={legacy} primary={primary_active} status={status:?} child={child_active}"
                        );
                        let mut expected = vec![format!("query:{thread}"), "delegate".into()];
                        if occupies {
                            expected.push(format!("query:{child_id}"));
                        }
                        assert_eq!(
                            *trace.lock().unwrap(),
                            expected,
                            "Q1-Q7 exact order, true Primary never skips children"
                        );
                    }
                }
                ctx.foreground.install_for_test(primary_active.then(|| RunId("query-error-primary".into())));
                let trace = std::sync::Mutex::new(Vec::new());
                let error = primary_or_delegated_run_active(
                    thread,
                    |id| {
                        trace.lock().unwrap().push(format!("query:{id}"));
                        if legacy {
                            Ok(host.run_assembly.installed_sessions
                                .read_run(id, |run| {
                                    run.context
                                        .as_ref()
                                        .is_some_and(|runtime| runtime.has_active_run())
                                }).expect("installed Sessions")
                                .unwrap_or(false))
                        } else {
                            Ok(sessions
                                .modify_session(id, |installed| installed.run.has_active_run()).expect("installed Sessions")
                                .unwrap_or(false))
                        }
                    },
                    async {
                        trace.lock().unwrap().push("delegate-error".into());
                        Err(crate::SessionExecutionError::unavailable_classified(
                            "delegation_read_unknown",
                            "original delegation failure",
                        ))
                    },
                )
                .await
                .expect_err("Q3 delegation error wins even when Primary is active");
                assert_eq!(error.kind, crate::SessionExecutionErrorKind::Unavailable, "Q3 kind");
                assert_eq!(error.code, "delegation_read_unknown", "Q3 original code");
                assert_eq!(
                    error.message, "original delegation failure",
                    "Q3 original message"
                );
                assert_eq!(
                    *trace.lock().unwrap(),
                    vec![format!("query:{thread}"), "delegate-error".into()],
                    "Q3 no child lookup"
                );
            }
            ctx.foreground.install_for_test(None);
            child.foreground.install_for_test(None);
            for id in ["query-missing-slot", "created"] {
                let mut missing = delegated(Status::Open);
                missing.run_id = RunId(id.into());
                let observed = primary_or_delegated_run_active(
                    id,
                    |session| {
                        if legacy {
                            Ok(host.run_assembly.installed_sessions
                                .read_run(session, |run| {
                                    run.context
                                        .as_ref()
                                        .is_some_and(|runtime| runtime.has_active_run())
                                }).expect("installed Sessions")
                                .unwrap_or(false))
                        } else {
                            Ok(sessions
                                .modify_session(session, |installed| installed.run.has_active_run()).expect("installed Sessions")
                                .unwrap_or(false))
                        }
                    },
                    async { Ok::<_, crate::SessionExecutionError>(vec![missing]) },
                )
                .await
                .unwrap();
                assert!(!observed, "Q1/Q5 missing installed Session or Runtime stays false");
            }
            assert!(
                !host.run_assembly.installed_sessions.contains("query-missing-slot"),
                "Q1 no new installed Session"
            );
            child.foreground.install_for_test(Some(RunId("query-child".into())));
            let mut second = delegated(Status::Open);
            second.run_id = RunId("query-never-read".into());
            let trace = std::sync::Mutex::new(Vec::new());
            assert!(
                primary_or_delegated_run_active(
                    thread,
                    |id| {
                        trace.lock().unwrap().push(format!("query:{id}"));
                        if legacy {
                            Ok(host.run_assembly.installed_sessions
                                .read_run(id, |run| {
                                    run.context
                                        .as_ref()
                                        .is_some_and(|runtime| runtime.has_active_run())
                                }).expect("installed Sessions")
                                .unwrap_or(false))
                        } else {
                            Ok(sessions
                                .modify_session(id, |installed| installed.run.has_active_run()).expect("installed Sessions")
                                .unwrap_or(false))
                        }
                    },
                    async {
                        trace.lock().unwrap().push("delegate".into());
                        Ok::<_, crate::SessionExecutionError>(vec![delegated(Status::Open), second])
                    }
                )
                .await
                .unwrap(),
                "Q6 first occupying child active"
            );
            assert_eq!(
                *trace.lock().unwrap(),
                vec![
                    format!("query:{thread}"),
                    "delegate".into(),
                    format!("query:{child_id}")
                ],
                "Q6 original iter.any stops after first true"
            );
        }
        ctx.foreground.install_for_test(None);
        child.foreground.install_for_test(None);
        // Q13 exercises the ACTUAL original commit projection while the actual
        // lifecycle guard is held. A public/relocking reader would time out.
        let lifecycle = sessions.lock_lifecycle(thread).await.expect("installed Sessions");
        // Use the request budget derived from this original fixture's 30s lease;
        // this detects recursive locking, not a one-second performance promise.
        let request_timeout =
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000)
                .request_timeout();
        assert!(
            !tokio::time::timeout(
                request_timeout,
                primary_or_delegated_run_active(
                    thread,
                    |id| Ok(sessions
                        .modify_session(id, |installed| installed.run.has_active_run()).expect("installed Sessions")
                        .unwrap_or(false)),
                    host.run_assembly.commit_source.delegated_runs_under_lifecycle(&host.thread_reads, lifecycle.held()),
                )
            )
            .await
            .expect("Q13 no lifecycle relock")
            .unwrap(),
            "Q13 current actual empty delegation snapshot"
        );
        drop(lifecycle);

        // Q12: real marker changes only AFTER the awaited reader is entered.
        // R1 prior Primary true -> false, child false stays false: retain earlier
        // true; R2 prior Primary false, child false -> true: read later child.
        // Join both signals/writer; never replace the actual contexts by bools.
        for legacy in [false, true] {
            for prior_primary in [false, true] {
                ctx.foreground.install_for_test(prior_primary.then(|| RunId("prior-primary".into())));
                child.foreground.install_for_test(None);
                let (entered, entry) = tokio::sync::oneshot::channel::<()>();
                let (changed, change) = tokio::sync::oneshot::channel::<()>();
                let writer = tokio::spawn({
                    let ctx = ctx.clone();
                    let child = child.clone();
                    async move {
                        entry.await.expect("Q12 actual reader entry");
                        ctx.foreground.install_for_test(None);
                        if !prior_primary {
                            child.foreground.install_for_test(Some(RunId("later-child".into())));
                        }
                        changed.send(()).expect("Q12 marker change joined");
                    }
                });
                let observed = std::panic::AssertUnwindSafe(primary_or_delegated_run_active(
                    thread,
                    |id| {
                        if legacy {
                            Ok(host.run_assembly.installed_sessions
                                .read_run(id, |run| {
                                    run.context
                                        .as_ref()
                                        .is_some_and(|runtime| runtime.has_active_run())
                                }).expect("installed Sessions")
                                .unwrap_or(false))
                        } else {
                            Ok(sessions
                                .modify_session(id, |installed| installed.run.has_active_run()).expect("installed Sessions")
                                .unwrap_or(false))
                        }
                    },
                    async {
                        entered.send(()).expect("Q12 start real marker writer");
                        change.await.expect("Q12 observe joined change");
                        Ok::<_, crate::SessionExecutionError>(vec![delegated(Status::Open)])
                    },
                ))
                .catch_unwind()
                .await;
                let joined = writer.await;
                joined.expect("Q12 writer joined even if the observed operation failed");
                let observed = observed
                    .expect("Q12 healthy query did not panic")
                    .expect("Q12 delegated read completed");
                assert!(
                    observed,
                    "Q12 preserve early Primary / late child for legacy={legacy} prior={prior_primary}"
                );
            }
        }
        ctx.foreground.install_for_test(None);
        child.foreground.install_for_test(None);
    }).await;

    // R4/R6: the named action grants no generic mutable Frozen access. Ordinary
    // callbacks above retain their readonly borrow; this command alone replaces
    // the whole Arc and the correlated requirements tuple.
    bind_test_memory(&host, thread, "view-memory", true);
    let memory = sessions
        .read_realization(thread, |r| r.staging().memory.clone())
        .expect("installed Sessions")
        .flatten()
        .unwrap();
    let compiled = || crate::staged_resources::CompiledEffectiveInputs {
        staged: crate::staged_resources::StagedResources {
            prompts: vec!["view requirements".into()],
            ..Default::default()
        },
        memory_bindings: std::collections::HashMap::from([("view-binding".into(), memory.clone())]),
    };
    assert!(
        matches!(
            sessions.publish_resource_requirements(
                "absent-requirements",
                "ws-published",
                None,
                compiled(),
                Vec::new(),
                "requirements-key".into(),
            ),
            Err(crate::realization_error::RealizationError::AuthorityLost {
                cause: crate::realization_error::AuthorityLoss::SessionAbsent,
                ..
            })
        ),
        "S1/R4 absent publication denied"
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .contains("absent-requirements"),
        "S1/R4 publication never installs"
    );
    let before = sessions.frozen(thread).expect("installed Sessions");
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
    let published = sessions.frozen(thread).expect("installed Sessions");
    assert_eq!(
        sessions
            .workspace(thread, "ignored-fallback")
            .expect("installed Sessions"),
        "ws-published",
        "C11 current whole Frozen Arc, no cached Workspace"
    );
    assert_eq!(
        host.capabilities
            .thread_workspace(thread)
            .expect("healthy registry"),
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
    host.run_assembly
        .installed_sessions
        .read(thread, |installed| {
            assert_eq!(
                installed.realization.staging().resources.prompts,
                ["view requirements"]
            );
            assert!(Arc::ptr_eq(
                &installed.realization.staging().memory_bindings["view-binding"],
                &memory
            ));
            assert!(
                installed.realization.staging().memory.is_none(),
                "S1 selection cleared"
            );
            assert_eq!(
                installed.realization.staging().skills,
                Some(Vec::new()),
                "S1 empty frozen Skills retained"
            );
            assert_eq!(
                installed.realization.staging().effect_key.as_deref(),
                Some("requirements-key")
            );
            assert!(
                Arc::ptr_eq(installed.run.context.as_ref().unwrap(), &ctx),
                "S1 Run kept"
            );
        })
        .expect("installed Sessions")
        .unwrap();

    let lease = crate::session_resource::SessionRealizationLease {
        owner: "view-worker".into(),
        runtime_incarnation: "view-worker/boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 30_000,
    };
    for rule in ["missing", "foreign", "closed", "cancelled", "expired"] {
        let token = Arc::new(awaken_runtime_contract::CancellationToken::new());
        host.run_assembly
            .installed_sessions
            .install_with(thread, |installed| {
                installed.realization.execution_mut().authority =
                    crate::realization_state::SessionRealizationAuthority::Live(
                        if rule == "expired" {
                            awaken_session_realization::test_support::test_expired_observed_lease(
                                lease.clone(),
                            )
                        } else {
                            awaken_session_realization::test_support::test_observed_lease(
                                lease.clone(),
                            )
                        },
                    );
                installed.realization.staging_mut().preparation = Some(token.clone());
                installed.realization.staging_mut().memory = Some(memory.clone());
                if rule == "closed" {
                    installed.realization.close_authority(None);
                }
            })
            .expect("installed Sessions");
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
        let failed = awaken_session_types::RunError::from(failed);
        assert_eq!(
            failed.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "S2/R5 {rule}"
        );
        assert_eq!(
            failed.code, "session_realization_authority_lost",
            "S2/R5 {rule}"
        );
        host.run_assembly
            .installed_sessions
            .read(thread, |installed| {
                assert!(
                    Arc::ptr_eq(&installed.inputs, &published),
                    "S2 Frozen {rule}"
                );
                assert_eq!(
                    installed.realization.staging().resources.prompts,
                    ["view requirements"],
                    "S2 resources {rule}"
                );
                assert!(
                    Arc::ptr_eq(
                        &installed.realization.staging().memory_bindings["view-binding"],
                        &memory
                    ),
                    "S2 bindings {rule}"
                );
                assert!(
                    Arc::ptr_eq(
                        installed.realization.staging().memory.as_ref().unwrap(),
                        &memory
                    ),
                    "S2 memory {rule}"
                );
                assert_eq!(
                    installed.realization.staging().skills,
                    Some(Vec::new()),
                    "S2 Skills {rule}"
                );
                assert_eq!(
                    installed.realization.staging().effect_key.as_deref(),
                    Some("requirements-key"),
                    "S2 key {rule}"
                );
                assert!(
                    Arc::ptr_eq(installed.run.context.as_ref().unwrap(), &ctx),
                    "S2 Run {rule}"
                );
            })
            .expect("installed Sessions")
            .unwrap();
    }
    host.run_assembly
        .installed_sessions
        .install_with(thread, |installed| {
            installed.realization.execution_mut().authority =
                crate::realization_state::SessionRealizationAuthority::Live(
                    awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                );
            installed.realization.staging_mut().preparation =
                Some(Arc::new(awaken_runtime_contract::CancellationToken::new()));
        })
        .expect("installed Sessions");
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
        sessions
            .frozen(thread)
            .expect("installed Sessions")
            .workspace
            .as_deref(),
        Some("ws-live"),
        "S3 exact live proof"
    );
    let mut callbacks = 0;
    let refused: Result<(), &str> = host
        .run_assembly
        .installed_sessions
        .modify(thread, |_| {
            callbacks += 1;
            Err("original callback error")
        })
        .expect("installed Sessions")
        .unwrap();
    assert_eq!(
        refused,
        Err("original callback error"),
        "S3 callback Result unchanged"
    );
    assert_eq!(callbacks, 1, "S3 callback exactly once");
    assert!(
        host.run_assembly.installed_sessions.contains(thread),
        "S3 Err never poisons the lock"
    );
    // Drop only the preparation marker installed by the explicit Source rows;
    // subsequent V6-V8 keep their original semantics and state expectations.
    host.run_assembly
        .installed_sessions
        .install_with(thread, |installed| {
            installed.realization.staging_mut().preparation = None
        })
        .expect("installed Sessions");

    sessions
        .drop_run_context(thread)
        .expect("installed Sessions");
    let (runtime, deferred_kept, kept_claim, session_dispatch) = host
        .run_assembly
        .installed_sessions
        .read_run(thread, |run| {
            (
                run.context.is_some(),
                run.deferred_executor.is_some(),
                run.dispatch_claim.clone(),
                run.session_dispatch,
            )
        })
        .expect("installed Sessions")
        .unwrap();
    assert!(!runtime, "V6 context dropped");
    assert!(deferred_kept, "V6 deferred executor kept");
    assert_eq!(kept_claim, Some(claim.clone()), "V6 claim kept");
    assert!(!session_dispatch, "V6 dispatch mark untouched");

    sessions
        .enter_session_dispatch(thread)
        .expect("installed Sessions");
    assert_eq!(query(&sessions), (false, true, Some(claim.clone())), "V7");
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.deferred_executor.is_some())
            .expect("installed Sessions")
            .unwrap(),
        "V7 deferred executor kept"
    );

    host.run_assembly
        .installed_sessions
        .install_with(thread, |installed| {
            installed.run.context = Some(ctx.clone());
            installed.realization.staging_mut().reconciliation =
                crate::staged_resources::EnvironmentResourceReconciliation::Adopted;
        })
        .expect("installed Sessions");
    let staged_before = host
        .run_assembly
        .installed_sessions
        .read(thread, |installed| {
            installed.realization.staging().effect_key.clone()
        })
        .expect("installed Sessions")
        .unwrap();
    sessions
        .end_terminal_disposal(thread)
        .expect("installed Sessions");
    let after = host
        .run_assembly
        .installed_sessions
        .read(thread, |installed| {
            (
                installed.run.context.is_some(),
                installed.realization.staging().reconciliation,
                installed.run.dispatch_claim.clone(),
                installed.run.session_dispatch,
                installed.realization.staging().effect_key.clone(),
            )
        })
        .expect("installed Sessions")
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

#[tokio::test]
async fn session_run_projection_callbacks_read_original_inputs_without_installing_or_opening_context()
 {
    // Cause/effect graph: C1 original registry is live; C2 Thread has frozen
    // Workspace/Agent inputs or is absent; C3 physical realization uses its
    // default source or a different decorated source. P1=C1+known, for both C3
    // rows => exact original inputs through both callbacks and the public Agent
    // getter. P2=C1+absent => immutable local Workspace/Agent None, no install.
    // P3 every read and factory construction => no Context, pool, or MCP relay.
    // The alternate existing source is a test seam, not admission authority.
    for decorated in [false, true] {
        let thread = "run-projection-inputs";
        let mut dependencies =
            crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
        if decorated {
            let alternate = Arc::new(crate::InstalledSessions::default());
            alternate
                .replace_frozen_with(thread, |next| {
                    next.workspace = Some("decorated-workspace".into());
                    next.agent_id = Some("decorated-agent".into());
                })
                .unwrap();
            dependencies.realization_source = Some(Box::new(move |_| alternate.clone()));
        }
        let host = dependencies.build().expect("real executor composition");
        let fallback = host.capabilities.local_workspace.clone();
        let (workspace, agent) = host.capabilities.session_run_projection_callbacks();
        assert!(
            !host.run_assembly.installed_sessions.contains(thread),
            "P3 factory is lazy"
        );
        host.capabilities
            .register_thread_workspace(thread, "original-workspace")
            .unwrap();
        host.capabilities
            .register_thread_agent_projection(thread, "original-agent")
            .unwrap();
        if decorated {
            assert_eq!(
                host.realization_executor
                    .sessions()
                    .workspace(thread, "unused")
                    .unwrap(),
                "decorated-workspace",
                "C3 actually different physical source"
            );
        }
        assert_eq!(workspace(thread).unwrap(), "original-workspace", "P1");
        assert_eq!(
            agent(thread).unwrap().as_deref(),
            Some("original-agent"),
            "P1"
        );
        assert_eq!(
            host.capabilities.thread_agent_projection(thread).unwrap(),
            agent(thread).unwrap(),
            "P1 same canonical Agent getter"
        );
        assert_eq!(
            workspace("unknown-projection").unwrap(),
            fallback,
            "P2 live unknown fallback"
        );
        assert_eq!(
            agent("unknown-projection").unwrap(),
            None,
            "P2 no inferred Agent"
        );
        assert!(
            !host
                .run_assembly
                .installed_sessions
                .contains("unknown-projection"),
            "P2 no read installation"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .read_run(thread, |run| run.context.is_none())
                .unwrap()
                .unwrap(),
            "P3 no Context read path"
        );
        assert!(host.dispatch.pool().is_none(), "P3 no pool task");
        assert!(
            host.run_assembly.mcp.relay().is_none(),
            "P3 no MCP relay task"
        );
    }
}
