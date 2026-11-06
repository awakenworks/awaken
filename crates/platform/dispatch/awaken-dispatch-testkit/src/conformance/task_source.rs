// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Same-store Task source retention rules in the original backend conformance suite.
async fn task_sources_share_the_original_done_transaction(
    store: &dyn ConformanceDispatch,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    use awaken_dispatch_contract::SessionRunAdmissionObservation;
    if !capabilities.completion_events || !capabilities.local_commit_guard {
        return;
    }
    // R1 actual Task plugin / private input demand => original request retained;
    // R2 plain / forged advertised capability / only a descendant has Task =>
    // no full payload. The same production predicate owns codec+retention.
    // R3 wrong epoch Done => Fenced and unchanged Live source/no completion;
    // R4 actual Done => one immutable completion + exact settled row source,
    // no claimable row. R5 exact replay/read => same source/feed, no resurrection.
    // All Memory/SQLite/PG original suite bindings run this rule. Durable reopen
    // and NULL/corrupt storage rows have backend-specific original fixtures.
    for mode in [
        "plugin",
        "private",
        "plain",
        "capability-only",
        "descendant-only",
    ] {
        clock.set(700_000);
        let tag = format!("task-source-{mode}");
        let mut request = dispatch(ns, &tag, &tag);
        match mode {
            "plugin" => request.activation.snapshot.resolved_spec.plugin_ids.push("background_task".into()),
            "private" => request.private_task_input = Some(serde_json::from_value(serde_json::json!({
                "version":1, "observations":[], "tenure": {
                    "thread":request.thread_id().0, "partition":{"kind":"thread"}, "run":"original-task-run",
                    "task":"task", "holder":"holder", "epoch":1,
                },
            })).unwrap()),
            "capability-only" => { request.placement.required_capabilities.insert(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY.into()); }
            "descendant-only" => {
                let mut descendant = request.activation.snapshot.clone();
                descendant.root_agent_id = AgentId(format!("{ns}-task-descendant"));
                descendant.resolved_spec.plugin_ids.push("background_task".into());
                request.agent_publications.push(descendant);
            }
            _ => {}
        }
        // C0 publish intrinsic codec demand only after the plugin/private
        // inputs are final. The compiler reads those inputs, so R2's plain,
        // capability-only and descendant-only rows keep their classification.
        request = request.with_worker_codec_requirements();
        let eligible = matches!(mode, "plugin" | "private");
        assert_eq!(
            request.needs_background_task_control(),
            eligible,
            "R1/R2 {mode}"
        );
        store.enqueue(request.clone()).await.unwrap();
        let claimed = store
            .claim_run(
                request.run_id(),
                "task-source-owner",
                LEASE_MS,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        let live_before = store
            .observe_session_run_admission(request.run_id())
            .await
            .unwrap();
        let count_before = store
            .completion_events_after(0, usize::MAX)
            .await
            .unwrap()
            .len();
        assert_eq!(
            store
                .settle(
                    request.run_id(),
                    claimed.lease.epoch().saturating_sub(1),
                    DispatchOutcome::Done,
                    &[]
                )
                .await
                .unwrap(),
            SettleOutcome::Fenced,
            "R3 {mode}"
        );
        assert_eq!(
            store
                .observe_session_run_admission(request.run_id())
                .await
                .unwrap(),
            live_before,
            "R3 exact original Live source"
        );
        assert_eq!(
            store
                .completion_events_after(0, usize::MAX)
                .await
                .unwrap()
                .len(),
            count_before,
            "R3 no source/completion write"
        );
        assert_eq!(
            store
                .settle(
                    request.run_id(),
                    claimed.lease.epoch(),
                    DispatchOutcome::Done,
                    &[]
                )
                .await
                .unwrap(),
            SettleOutcome::Applied,
            "R4 {mode}"
        );
        let observed = store
            .observe_session_run_admission(request.run_id())
            .await
            .unwrap();
        let SessionRunAdmissionObservation::Completed {
            completion,
            retained_request,
        } = &observed
        else {
            panic!("R4 {mode} completion");
        };
        let retained = retained_request.as_ref().unwrap();
        assert_eq!(
            retained.as_deref(),
            eligible.then_some(&claimed.request),
            "R4 stored row value, not reconstruction {mode}"
        );
        assert_eq!(
            completion.request_fingerprint.as_deref(),
            Some(claimed.request.admission_fingerprint().as_str()),
            "R4 original identity scope"
        );
        let feed = store.completion_events_after(0, usize::MAX).await.unwrap();
        let wire = serde_json::to_value(
            feed.iter()
                .find(|item| item.run_id == *request.run_id())
                .unwrap(),
        )
        .unwrap();
        assert!(
            wire.get("retained_request").is_none(),
            "R4 public completion wire unchanged"
        );
        assert!(
            store
                .claim_run(
                    request.run_id(),
                    "another-owner",
                    LEASE_MS,
                    &Default::default()
                )
                .await
                .unwrap()
                .is_none(),
            "R4 unclaimable"
        );
        store.enqueue(request.clone()).await.unwrap();
        assert_eq!(
            store
                .observe_session_run_admission(request.run_id())
                .await
                .unwrap(),
            observed,
            "R5 same source on exact replay"
        );
        assert_eq!(
            store.completion_events_after(0, usize::MAX).await.unwrap(),
            feed,
            "R5 no new feed/source/sequence"
        );
    }
}
