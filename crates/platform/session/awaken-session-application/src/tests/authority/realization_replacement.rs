//! Current-head staging and exact replacement/retirement receipt replay.
use super::*;

#[tokio::test]
async fn recovery_replacement_partitions_staged_receipts_and_replays_activation() {
    use awaken_session_configuration_contract::mcp::{
        McpAttachmentDraft, McpAttachmentOrigin, McpAttachmentState, McpRealizationClaim,
        McpRealizationReceipt, SessionMcpAttachmentSet,
    };
    use awaken_session_configuration_contract::resource::SessionRealizationLease;
    use awaken_session_execution_contract::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        RenewSessionRealization, SessionRealizationAction, SessionRealizationControl,
        SessionRealizationControlFailure, SessionRealizationTarget,
    };
    // Cause/effect graph: C1 hot owner versus restarted owner; C2 renewal
    // between Stage and activation; C3 activation response loss/replay;
    // C4 valid versus corrupted recovered-old receipt. The full replacement
    // changes one name, removes another, and retains a third in the same CAS.
    // E1 current heads alone are prepared; every reported receipt is checked;
    // E2 Publish contains precisely still-Active staged receipts with their
    // original expiry; E3 superseded/removed names drain, never publish;
    // E4 exact replay stutters durably; E5 strict acknowledgement reaches Idle.
    //
    // | Rule | owner | renewal | receipt | response | effects |
    // | R1 | hot | no/yes | exact | delivered/lost | E1-E5; only new name publishes |
    // | R2 | restart | no/yes | exact | delivered/lost | E1-E5; retained name also publishes |
    // | R3 | restart | no/yes | corrupted old | any | reject before root write |
    // | R4 | either | no/yes | foreign/duplicate/bad fingerprint/incarnation/future expiry | replay | reject before root write |
    // C3 follows every R1/R2 activation; a heartbeat before activation checks
    // that neither a current expiry nor a drained generation fabricates Stage.
    for recovery in [false, true] {
        for renewal in [false, true] {
            let repo = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            let app = application(
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let expiry = awaken_time::system_clock().now().as_epoch_ms() + 60_000;
            let lease = SessionRealizationLease {
                owner: "owner".into(),
                runtime_incarnation: "old-boot".into(),
                epoch: 1,
                expires_at_unix_ms: expiry,
            };
            let draft = |name: &str, version: &str| McpAttachmentDraft {
                name: name.into(),
                target: McpTarget::parse_http(format!("https://{name}.example.test/{version}"))
                    .unwrap(),
                prompts_as_skills: false,
                credential: None,
                origin: McpAttachmentOrigin::Session,
            };
            let mut session = persisted("recovery-replacement", false, "idle");
            session.realization = Some(lease.clone());
            session.mcp = SessionMcpAttachmentSet::from_initial(
                vec![
                    draft("replace", "v1"),
                    draft("remove", "v1"),
                    draft("keep", "v1"),
                ],
                None,
            )
            .unwrap();
            for attachment in session.mcp.attachments.clone() {
                let generation = attachment.generation;
                let realization_id = format!("old-{}", attachment.name);
                session
                    .mcp
                    .claim_realization(
                        &generation,
                        McpRealizationClaim {
                            realization_id: realization_id.clone(),
                            runtime_incarnation: lease.runtime_incarnation.clone(),
                            lease_epoch: lease.epoch,
                            lease_expires_at_unix_ms: expiry,
                            stage_idempotency_key: realization_id.clone(),
                        },
                    )
                    .unwrap();
                session.mcp.activate(&generation, &realization_id).unwrap();
                session
                    .mcp
                    .acknowledge_publication(&generation, &realization_id)
                    .unwrap();
            }
            session
                .mcp
                .request_full_replacement(vec![draft("replace", "v2"), draft("keep", "v1")], None)
                .unwrap();
            create(repo.as_ref(), session).await;
            let staged = app
                .realizations
                .begin_session_realization(BeginSessionRealization {
                    session_id: "recovery-replacement".into(),
                    expected_resources: None,
                    target: SessionRealizationTarget {
                        owner: lease.owner.clone(),
                        runtime_incarnation: if recovery {
                            "new-boot".into()
                        } else {
                            lease.runtime_incarnation.clone()
                        },
                        lease_expires_at_unix_ms: expiry,
                        reassign_existing_lease: false,
                    },
                })
                .await
                .unwrap();
            let SessionRealizationAction::Stage { mcp_stages, .. } = &staged.action else {
                panic!("R1/R2 stage");
            };
            assert_eq!(mcp_stages.len(), if recovery { 2 } else { 1 });
            // R2+renewal also replays an earlier directive's already completed
            // predecessor/removal receipts. New preparation never requires
            // reopening them; exact optional receipts remain drain authority.
            let completed_requests = if recovery && renewal {
                let root = repo
                    .get(&SessionId::from("recovery-replacement"))
                    .await
                    .unwrap();
                root.mcp
                    .attachments
                    .iter()
                    .filter(|attachment| {
                        matches!(
                            attachment.state,
                            McpAttachmentState::Active | McpAttachmentState::Realizing
                        )
                    })
                    .map(|attachment| {
                        crate::projection::stage_mcp_request(
                            "workspace",
                            &root.session_id,
                            attachment,
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>()
            } else {
                mcp_stages.clone()
            };
            let receipts = completed_requests
                .iter()
                .map(|request| McpRealizationReceipt {
                    generation: request.generation.clone(),
                    realization_id: request.realization_id.clone(),
                    selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                    actual_realization_kind: None,
                    receipt_fingerprint: request.fingerprint(),
                })
                .collect::<Vec<_>>();
            let command = ActivateSessionRealization {
                session_id: "recovery-replacement".into(),
                lease: staged.authority.lease.clone(),
                prepared_resource_revision: None,
                mcp_receipts: receipts.clone(),
            };
            if renewal {
                app.realizations
                    .renew_session_realization(RenewSessionRealization {
                        session_id: command.session_id.clone(),
                        asserted_lease: command.lease.clone(),
                        requested_expires_at_unix_ms: expiry + 60_000,
                    })
                    .await
                    .unwrap();
            }
            if recovery {
                let before = repo.get(&command.session_id).await.unwrap();
                let mut corrupt = command.clone();
                corrupt.mcp_receipts[0]
                    .receipt_fingerprint
                    .push_str("-corrupt");
                assert!(
                    matches!(
                        app.realizations.activate_session_realization(corrupt).await,
                        Err(SessionRealizationControlFailure::Invalid(_))
                    ),
                    "R3 old receipt is not ignored"
                );
                assert_eq!(
                    repo.get(&command.session_id).await.unwrap(),
                    before,
                    "R3 no write"
                );
            }
            let activated = app
                .realizations
                .activate_session_realization(command.clone())
                .await
                .unwrap();
            let SessionRealizationAction::Publish { publish, drain } = &activated.action else {
                panic!("R1/R2 publish");
            };
            assert_eq!(
                publish.len(),
                if recovery { 2 } else { 1 },
                "E2 no obsolete publication"
            );
            assert_eq!(drain.len(), 2, "E3 replaced and removed names drain");
            let after = repo.get(&command.session_id).await.unwrap();
            for generation in publish {
                assert!(
                    receipts
                        .iter()
                        .any(|receipt| &receipt.generation == generation),
                    "E2 exact physical receipt"
                );
                assert_eq!(
                    generation.lease_expires_at_unix_ms(),
                    expiry,
                    "E2 no fabricated renewed receipt"
                );
                assert!(
                    after
                        .mcp
                        .attachments
                        .iter()
                        .any(|a| &a.generation == generation.generation()
                            && a.state == McpAttachmentState::Active),
                    "E2 active only"
                );
                assert!(
                    !drain
                        .iter()
                        .any(|old| old.generation() == generation.generation()),
                    "E3 disjoint sets"
                );
            }
            let replay = app
                .realizations
                .activate_session_realization(command.clone())
                .await
                .unwrap();
            assert!(
                replay.same_progress_as(&activated),
                "E4 response-loss replay"
            );
            assert_eq!(
                repo.get(&command.session_id).await.unwrap(),
                after,
                "E4 no replay write"
            );
            // In R2 the first receipt is now Draining: accepting its presence
            // on replay must not bypass any canonical receipt verification.
            for fault in [
                "foreign",
                "duplicate",
                "fingerprint",
                "incarnation",
                "future",
            ] {
                let mut invalid = command.clone();
                match fault {
                    "foreign" => {
                        let mut extra = invalid.mcp_receipts[0].clone();
                        let mut w = extra.generation.to_wire();
                        w.generation = w.generation.at_version(99);
                        extra.generation = w.try_into().unwrap();
                        invalid.mcp_receipts.push(extra);
                    }
                    "duplicate" => invalid.mcp_receipts.push(invalid.mcp_receipts[0].clone()),
                    "fingerprint" => invalid.mcp_receipts[0]
                        .receipt_fingerprint
                        .push_str("-corrupt"),
                    "incarnation" | "future" => {
                        let mut w = invalid.mcp_receipts[0].generation.to_wire();
                        match fault {
                            "incarnation" => w.runtime_incarnation.push_str("-foreign"),
                            _ => w.lease_expires_at_unix_ms = u64::MAX,
                        }
                        invalid.mcp_receipts[0].generation = w.try_into().unwrap();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    matches!(
                        app.realizations.activate_session_realization(invalid).await,
                        Err(SessionRealizationControlFailure::Invalid(_))
                    ),
                    "R4 reject {fault}"
                );
                assert_eq!(
                    repo.get(&command.session_id).await.unwrap(),
                    after,
                    "R4 {fault} no write"
                );
            }
            let complete = app
                .realizations
                .acknowledge_session_realization(AcknowledgeSessionRealization {
                    continuation: None,
                    session_id: command.session_id.clone(),
                    lease: activated.authority.lease,
                    published: publish.clone(),
                    drained: drain.clone(),
                })
                .await
                .unwrap();
            assert!(
                matches!(complete.action, SessionRealizationAction::Complete),
                "E5 convergence"
            );
            let complete = repo.get(&command.session_id).await.unwrap();
            assert_eq!(complete.execution, SessionExecutionState::Idle);
            assert!(
                complete
                    .mcp
                    .attachments
                    .iter()
                    .all(|a| a.state == McpAttachmentState::Removed
                        || (a.state == McpAttachmentState::Active && a.publication_acknowledged)),
                "E5 every generation is published or durably drained"
            );
        }
    }
}
