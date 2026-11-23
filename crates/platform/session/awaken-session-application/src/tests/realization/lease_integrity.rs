//! A Session realization lease whose persisted generation is untrustworthy is
//! refused permanently at the Control boundary, and the begin loop never
//! retries it.

use super::*;

#[tokio::test]
async fn replacement_activation_requires_current_head_and_checks_predecessor_receipts() {
    // Causes: C1 only current receipt versus an earlier directive's complete
    // predecessor/current pair; C2 current omitted; C3 predecessor fingerprint
    // corrupt; C4 exact activation replay; C5 the desired head already failed.
    // Rules A1 current-only and A2 the
    // complete exact pair -> activate/publish only current, drain predecessor;
    // A3 missing current and A4 corrupt optional predecessor -> reject atomically;
    // A5 exact replay -> same publication/drain authority; A6 C5+old-only ->
    // reject because canonical activation does not retire the old generation.
    // The real SQLite root
    // and public Control port own every transition; NoopMcpRealizer supplies only
    // physical-effect receipts, not successful adoption or expected root state.
    for mode in [
        "current",
        "complete",
        "old-only",
        "corrupt-old",
        "failed-current",
    ] {
        let repo = sqlite_session_repository("MCP predecessor receipt repository").await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = format!("mcp-predecessor-{mode}");
        let (mut root, lease, _) = publication_fixture(&id);
        let mut current = root.mcp.attachments[0].clone();
        current.generation = current.generation.at_version(2);
        current.state = crate::session_mcp::McpAttachmentState::Realizing;
        current.realization.as_mut().unwrap().realization_id = "realize-browser-2".into();
        current.realization.as_mut().unwrap().stage_idempotency_key = "stage-browser-2".into();
        root.mcp.attachments.push(current);
        root.mcp.desired_names = Some(std::collections::BTreeSet::from(["browser".into()]));
        if mode == "failed-current" {
            let generation = root.mcp.attachments[1].generation.clone();
            root.mcp
                .fail_realization(&generation, "realize-browser-2", "stage failed")
                .unwrap();
        }
        let mut receipts = Vec::new();
        for attachment in &root.mcp.attachments {
            if (mode == "current" && attachment.generation.version() == 1)
                || (matches!(mode, "old-only" | "failed-current")
                    && attachment.generation.version() == 2)
            {
                continue;
            }
            let request =
                crate::projection::stage_mcp_request("workspace", &root.session_id, attachment)
                    .unwrap();
            let mut receipt = NoopMcpRealizer.stage_mcp_attachment(request).await.unwrap();
            if mode == "corrupt-old" && attachment.generation.version() == 1 {
                receipt.receipt_fingerprint.push_str("-corrupt");
            }
            receipts.push(receipt);
        }
        create(repo.as_ref(), root).await;
        let before = repo.get(&SessionId::from(&id)).await.unwrap();
        let command = awaken_session_execution_contract::ActivateSessionRealization {
            session_id: SessionId::from(&id),
            lease,
            prepared_resource_revision: None,
            mcp_receipts: receipts,
        };
        let result = app
            .realizations
            .activate_session_realization(command.clone())
            .await;
        if matches!(mode, "old-only" | "corrupt-old" | "failed-current") {
            assert!(result.is_err(), "A3/A4 {mode}");
            assert_eq!(
                repo.get(&SessionId::from(&id)).await.unwrap(),
                before,
                "A3/A4 no effects"
            );
            continue;
        }
        let directive = result.expect("A1/A2 exact current receipt is sufficient");
        let awaken_session_execution_contract::SessionRealizationAction::Publish { publish, drain } =
            directive.action
        else {
            panic!("A1/A2 expected Publish");
        };
        assert_eq!(publish.len(), 1);
        assert_eq!(publish[0].generation().version(), 2);
        assert_eq!(drain.len(), 1);
        assert_eq!(drain[0].generation().version(), 1);
        let applied = repo.get(&SessionId::from(&id)).await.unwrap();
        let replay = app
            .realizations
            .activate_session_realization(command)
            .await
            .unwrap();
        assert_eq!(
            repo.get(&SessionId::from(&id)).await.unwrap(),
            applied,
            "A5 no second mutation"
        );
        let awaken_session_execution_contract::SessionRealizationAction::Publish {
            publish: replay_publish,
            drain: replay_drain,
        } = replay.action
        else {
            panic!("A5 expected retained publication");
        };
        assert_eq!(replay_publish, publish, "A5 current publication unchanged");
        assert_eq!(replay_drain, drain, "A5 predecessor drain unchanged");
    }
}

#[tokio::test]
async fn an_untrustworthy_persisted_lease_generation_refuses_begin_permanently_without_retry() {
    // C1 the Session root's persisted realization lease generation: 0, or
    // u64::MAX on an expired lease that begin must succeed. Effects: E1 the
    // Control failure is Internal (terminal, never retried) with the stable
    // integrity code; E2 the begin root-CAS loop reads the root once per call
    // and never commits (no CAS retry); E3 three calls refuse identically and
    // leave the durable root, and its epoch, unchanged.
    // | Rule | persisted epoch | E1 detail                     | E2 reads/call | E3 |
    // | L1   | 0               | ... a granted lease carries 0 | 1             | =  |
    // | L2   | u64::MAX        | ... has no successor          | 1             | =  |
    for (rule, epoch, cause) in [
        ("L1", 0, "a granted lease carries epoch 0"),
        ("L2", u64::MAX, "the persisted lease epoch has no successor"),
    ] {
        let durable = sqlite_session_repository("lease integrity repository").await;
        let session_id = format!("lease-integrity-{rule}");
        let mut session = persisted(&session_id, false, "idle");
        session.realization = Some(crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch,
            expires_at_unix_ms: 1,
        });
        create(durable.as_ref(), session).await;
        let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
        let application = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let before = durable
            .get(&SessionId::from(session_id.as_str()))
            .await
            .unwrap();
        let expected =
            awaken_session_execution_contract::SessionRealizationControlFailure::Internal(format!(
                "Session realization lease: lease_generation_integrity_violated: {cause}"
            ));
        for attempt in 0..3 {
            let reads = repo.observed_gets.lock().unwrap().len();
            let refused = SessionRealizationControl::begin_session_realization(
                application.realizations.as_ref(),
                awaken_session_execution_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: session_id.as_str().into(),
                    target: awaken_session_execution_contract::SessionRealizationTarget {
                        owner: "worker-b".into(),
                        runtime_incarnation: "worker-b/boot-1".into(),
                        lease_expires_at_unix_ms: u64::MAX,
                        reassign_existing_lease: true,
                    },
                },
            )
            .await;
            assert_eq!(
                refused.err(),
                Some(expected.clone()),
                "{rule}/E1 attempt {attempt}"
            );
            assert_eq!(
                expected.disposition(),
                awaken_session_execution_contract::SessionRealizationControlDisposition::Terminal,
                "{rule}/E1"
            );
            assert_eq!(
                repo.observed_gets.lock().unwrap().len() - reads,
                1,
                "{rule}/E2 one root read, no CAS retry, attempt {attempt}"
            );
            assert_eq!(
                durable
                    .get(&SessionId::from(session_id.as_str()))
                    .await
                    .unwrap(),
                before,
                "{rule}/E3 root unchanged, attempt {attempt}"
            );
        }
    }
}
