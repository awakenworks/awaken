//! A Session realization lease whose persisted generation is untrustworthy is
//! refused permanently at the Control boundary, and the begin loop never
//! retries it.

use super::*;

#[tokio::test]
async fn replacement_activation_requires_current_head_and_checks_predecessor_receipts() {
    // Cause/effect table: the same lease retains an old Active/unacknowledged
    // physical claim while a viable same-source credential successor is staged.
    // A1 current only and A2 exact old/current pair -> publish current, drain old;
    // A3 missing current and A4 corrupt optional old -> atomic refusal;
    // A5 exact replay -> identical publication/drain authority;
    // A6 failed successor + a REAL old Stage receipt -> preserve the canonical
    // fallback, publish old and drain nothing; A7 missing fallback proof -> refuse;
    // A8 unknown historical old claim -> refuse before writes, never invent cleanup.
    // Ordinary replacement/recovery and corrupt drain identity remain covered
    // in realization_replacement. The real SQLite root, canonical MCP commands
    // and public Control port own transitions. NoopMcpRealizer supplies physical
    // receipts only; it does not manufacture root adoption or external acceptance.
    for mode in [
        "current",
        "complete",
        "old-only",
        "corrupt-old",
        "failed-current",
        "failed-missing",
        "unknown-predecessor",
    ] {
        let repo = sqlite_session_repository("MCP predecessor receipt repository").await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = format!("mcp-predecessor-{mode}");
        let (mut root, lease, _) = publication_fixture(&id);
        let old = root.mcp.attachments[0].clone();
        let old_claim = old.realization.clone().unwrap();
        let holder = root
            .frozen_baseline()
            .unwrap()
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let draft = |revision| crate::session_mcp::McpAttachmentDraft {
            name: old.name.clone(),
            target: old.target.clone(),
            prompts_as_skills: false,
            origin: old.origin,
            credential: Some(awaken_agent_contract::credential::CredentialAccess::new(
                awaken_agent_contract::credential::CredentialRef {
                    id: "receipt-source".into(),
                    revision,
                },
                awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
                awaken_agent_contract::credential::CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                awaken_agent_contract::credential::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_agent_contract::credential::ModelExposurePolicy::Forbidden,
                ),
            )),
        };
        root.mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
            vec![draft(1)],
            Some(holder.clone()),
        )
        .unwrap();
        let old_generation = root.mcp.attachments[0].generation.clone();
        root.mcp
            .claim_realization(&old_generation, old_claim.clone())
            .unwrap();
        root.mcp
            .activate(&old_generation, &old_claim.realization_id)
            .unwrap();
        let plan = root
            .mcp
            .request_full_replacement(vec![draft(2)], Some(holder.clone()))
            .unwrap();
        let current = plan.requested[0].clone();
        root.mcp
            .claim_realization(
                &current,
                crate::session_mcp::McpRealizationClaim {
                    realization_id: "realize-browser-2".into(),
                    stage_idempotency_key: "stage-browser-2".into(),
                    ..old_claim
                },
            )
            .unwrap();
        if matches!(mode, "failed-current" | "failed-missing") {
            root.mcp
                .fail_realization(&current, "realize-browser-2", "stage failed")
                .unwrap();
        }
        if mode == "unknown-predecessor" {
            // Explicit historical missing evidence, not a newly admitted state.
            root.mcp.attachments[0].realization = None;
        }
        let mut receipts = Vec::new();
        for attachment in &root.mcp.attachments {
            if mode == "failed-missing"
                || (matches!(mode, "current" | "unknown-predecessor")
                    && attachment.generation.version() == 1)
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
        if matches!(
            mode,
            "old-only" | "corrupt-old" | "failed-missing" | "unknown-predecessor"
        ) {
            assert!(result.is_err(), "atomic refusal: {mode}");
            assert_eq!(
                repo.get(&SessionId::from(&id)).await.unwrap(),
                before,
                "refusal writes nothing: {mode}"
            );
            continue;
        }
        let directive = result.expect("exact required Stage receipts are sufficient");
        let awaken_session_execution_contract::SessionRealizationAction::Publish { publish, drain } =
            directive.action
        else {
            panic!("expected Publish: {mode}");
        };
        assert_eq!(publish.len(), 1);
        assert_eq!(
            publish[0].generation().version(),
            if mode == "failed-current" { 1 } else { 2 }
        );
        assert_eq!(drain.len(), if mode == "failed-current" { 0 } else { 1 });
        if let Some(drained) = drain.first() {
            assert_eq!(drained.generation().version(), 1);
        }
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
        assert_eq!(replay_publish, publish, "A5 publication unchanged");
        assert_eq!(replay_drain, drain, "A5 drain unchanged");
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
