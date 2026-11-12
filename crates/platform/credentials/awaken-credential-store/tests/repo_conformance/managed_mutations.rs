// @formal evidence credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
// @formal evidence credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication
//! Shared Managed mutation and receipt conformance under the existing backend runner.
use super::*;

/// Managed begin/phase cause-effect table shared by all three adapters. C1 the
/// fresh fence is canonical or forged; C2 no WAL or one logically identical WAL
/// already exists; C3 the caller is the durable physical owner. Effects: M1
/// forged Ready/epoch/owner/replacement lineage rejects with zero WAL; M2 first begin owns exactly
/// one attempt; M3 a fresh random follower attaches as non-owner and creates no
/// second WAL/ref; M4 only the exact owner may mark Ready, publish the pair, and
/// complete Reclaiming; M5 a caller whose before-material projection differs
/// from the exact durable Reclaiming envelope is rejected and cannot drive
/// cleanup. Rules M1 !C1=>E1; M2 C1+no WAL=>E2; M3 C1+same logical WAL=>E3;
/// M4 C3=>E4; M5 !exact-envelope=>conflict and durable WAL retained.
/// M6 independent source/child identity collisions: neither present admits;
/// source-only, child-only or both present reject before creating a WAL. Reuse
/// the published IDs with fresh opposite IDs, without fabricating torn storage.
pub(super) async fn managed_material_mutation_begin_and_phases_conform(
    repo: &dyn ManagedCredentialRepository,
) {
    expired_writer_claims_are_exact_snapshot_cas(repo).await;
    managed_vault_workspace_authority_conforms(repo).await;
    let vault = ManagedVault {
        id: "vault-begin-owner".into(),
        workspace_id: "ws".into(),
        display_name: "vault-begin-owner".into(),
        metadata: Default::default(),
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    repo.insert_vault("ws", vault.clone()).await.unwrap();
    let mut source = source("cred:managed-begin-owner", "ws");
    source.material_ref = Some(
        awaken_credential_vault::repo::application_mcp_material_ref(
            &source.id,
            "generation-one",
            1,
        )
        .unwrap(),
    );
    source.descriptor = Some(
        awaken_credential_vault::mcp_bearer_descriptor("https://mcp.example.test")
            .with_expiry(5_000),
    );
    source.provider_id = None;
    source.protocol_endpoint_id = Some("managed-begin-owner-endpoint".into());
    source.env_key = None;
    let child = ManagedVaultCredential {
        id: "child-begin-owner".into(),
        vault_id: vault.id,
        workspace_id: "ws".into(),
        source_id: source.id.clone(),
        auth: ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.test".into(),
        },
        metadata: Default::default(),
        display_name: None,
        created_at: "2026-09-17T00:00:00Z".into(),
        updated_at: "2026-09-17T00:00:00Z".into(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let owner =
        PendingManagedCredentialMutation::create(source.clone(), child.clone(), 1_000).unwrap();
    let follower = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();

    let mut forged_ready = owner.clone();
    forged_ready.material_fence.phase = CredentialMaterialMutationPhase::Ready;
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_ready, 1_000).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged Ready"
    );
    let mut forged_epoch = owner.clone();
    forged_epoch.material_fence.writer_epoch = epoch(2);
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_epoch, 1_000).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged epoch"
    );
    let mut forged_owner = owner.clone();
    forged_owner.material_fence.writer_token = Some("forged-owner".into()); // awaken-allow(secret): -- writer identity, not material
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_owner, 1_000).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged owner"
    );
    let mut forged_lineage = owner.clone();
    forged_lineage.after_source.replacement_of = Some(CredentialRef {
        id: "predecessor-not-fenced-by-managed-create".into(),
        revision: 1,
    });
    assert!(
        matches!(
            repo.begin_managed_mutation(forged_lineage, 1_000).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "M1 forged replacement lineage"
    );
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M1"
    );

    assert!(
        repo.begin_managed_mutation(owner.clone(), 1_000)
            .await
            .unwrap(),
        "M2"
    );
    assert_ne!(
        owner.material_fence.attempt_id(),
        follower.material_fence.attempt_id(),
        "M3 exercises a fresh random physical attempt"
    );
    assert!(
        !repo.begin_managed_mutation(follower, 1_000).await.unwrap(),
        "M3"
    );
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![owner.clone()],
        "M3"
    );

    let ready = repo.mark_managed_mutation_ready(&owner).await.unwrap();
    let reclaiming = repo.commit_managed_mutation(&ready).await.unwrap();
    assert_eq!(
        reclaiming.material_fence.phase,
        CredentialMaterialMutationPhase::Reclaiming,
        "M4"
    );
    repo.complete_managed_mutation(&reclaiming).await.unwrap();
    assert!(
        repo.pending_managed_mutations().await.unwrap().is_empty(),
        "M4"
    );

    let before_source = repo.get(&owner.after_source.id).await.unwrap();
    let before_child = repo
        .get_vault_credential("ws", &owner.after_credential.id)
        .await
        .unwrap()
        .unwrap();
    let mut after_source = before_source.clone();
    after_source.version += 1;
    after_source.material_ref = Some(
        awaken_credential_vault::repo::application_mcp_material_ref(
            &after_source.id,
            "generation-two",
            2,
        )
        .unwrap(),
    );
    after_source.descriptor.as_mut().unwrap().expires_at_unix_ms = Some(9_000);
    let mut after_child = before_child.clone();
    after_child.revision += 1;
    after_child
        .metadata
        .insert("generation".into(), "two".into());
    let update = PendingManagedCredentialMutation::change(
        "managed-update:exact-reclaiming-envelope".into(),
        ManagedCredentialOperation::Update,
        before_source,
        after_source,
        before_child,
        after_child,
        true,
        1_000,
    )
    .unwrap();
    // Public serde fields never authorize a validity change. These decoded
    // envelopes must reject before any durable WAL/source/child/outbox write,
    // on Memory, SQLite and PostgreSQL alike.
    for case in 0..10 {
        let mut forged = update.clone();
        match case {
            0 => {
                let reference = &mut forged.after_source.material_ref.as_mut().unwrap().0;
                *reference = reference.replace(":generation:2:", ":generation:1:");
            }
            1 => {
                forged.after_source.material_ref =
                    forged.before_source.as_ref().unwrap().material_ref.clone()
            }
            2 => forged.after_source.version -= 1,
            3 => forged.after_source.version += 1,
            4 => forged
                .after_source
                .descriptor
                .as_mut()
                .unwrap()
                .provider
                .0
                .push_str("-foreign"),
            5 => forged.after_source.descriptor.as_mut().unwrap().targets[0]
                .target
                .audience
                .0
                .push_str("/foreign"),
            6 => {
                let reference = &mut forged.after_source.material_ref.as_mut().unwrap().0;
                *reference = format!(
                    "sec:ordinary:attempt:{}",
                    forged.material_fence.attempt_id()
                );
            }
            7 => {
                forged.after_source.material_ref =
                    forged.before_source.as_ref().unwrap().material_ref.clone();
                forged.after_source.auxiliary_material_refs.insert(
                    "only-new-auxiliary".into(),
                    SecretRef(format!(
                        "sec:auxiliary:attempt:{}",
                        forged.material_fence.attempt_id()
                    )),
                );
            }
            8 => {
                forged.operation = ManagedCredentialOperation::Archive;
                forged.after_source.status = CredentialStatus::Archived;
                forged.after_credential.lifecycle = ManagedCredentialLifecycle::Archived {
                    at: "archived".into(),
                };
            }
            9 => {
                forged.operation = ManagedCredentialOperation::Delete;
                forged.after_source.status = CredentialStatus::Archived;
                forged.after_credential.lifecycle = ManagedCredentialLifecycle::Deleted {
                    at: "deleted".into(),
                };
            }
            _ => unreachable!(),
        }
        let decoded: PendingManagedCredentialMutation =
            serde_json::from_value(serde_json::to_value(forged).unwrap()).unwrap();
        assert!(
            decoded.validate().is_err(),
            "validity forged envelope case {case}"
        );
        assert!(
            repo.begin_managed_mutation(decoded, 1_000).await.is_err(),
            "validity forged case {case}"
        );
        assert!(
            repo.pending_managed_mutations().await.unwrap().is_empty(),
            "validity forged case {case}"
        );
        assert_eq!(
            repo.get(&update.after_source.id).await.unwrap(),
            *update.before_source.as_ref().unwrap()
        );
        assert_eq!(
            repo.get_vault_credential("ws", &update.after_credential.id)
                .await
                .unwrap(),
            update.before_credential
        );
        assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    }
    assert!(
        repo.begin_managed_mutation(update.clone(), 1_000)
            .await
            .unwrap()
    );
    assert_eq!(
        repo.get(&update.after_source.id).await.unwrap(),
        *update.before_source.as_ref().unwrap(),
        "expiry and material remain the old version before Ready/publication"
    );
    let update = repo.mark_managed_mutation_ready(&update).await.unwrap();
    let update_reclaiming = repo.commit_managed_mutation(&update).await.unwrap();
    assert_eq!(
        repo.get(&update.after_source.id).await.unwrap(),
        update.after_source
    );
    assert_eq!(
        update
            .after_source
            .descriptor
            .as_ref()
            .unwrap()
            .expires_at_unix_ms,
        Some(9_000)
    );
    // Rollout identity decision table, shared by all three real repositories.
    // C1 event key exists; C2 every payload field matches; C3 exact mutation is
    // replayed. R1 C1+C2+C3 => same event and committed pair; R2 C1+!C2 =>
    // conflict with unchanged event/pair; R3 !C1 => idempotent no-op; R4 C1+C2
    // acknowledgement => remove pending visibility and retain exact receipt; replay is a no-op.
    // Single-field collisions cover each of the seven non-key fields; all
    // eight-field combinations are independently covered in the contract's
    // native/Kani table. The SQL transaction/JSON boundary is exercised here.
    let events = repo.pending_managed_rollouts().await.unwrap();
    assert_eq!(events.len(), 1, "rollout R1");
    let event = events[0].clone();
    assert_eq!(
        repo.commit_managed_mutation(&update).await.unwrap(),
        update_reclaiming,
        "rollout R1 exact commit replay"
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    for field in 0..7 {
        let mut collision = event.clone();
        match field {
            0 => collision.workspace_id.push('x'),
            1 => collision.vault_id.push('x'),
            2 => collision.credential_id.push('x'),
            3 => collision.source_id.0.push('x'),
            4 => collision.source_version ^= 1,
            5 => collision.credential_revision ^= 1,
            6 => collision.operation = ManagedCredentialOperation::Delete,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                repo.complete_managed_rollout(&collision).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "rollout R2 field {field}"
        );
        assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
        assert_eq!(
            repo.get(&update.after_source.id).await.unwrap(),
            update.after_source
        );
        assert_eq!(
            repo.get_vault_credential("ws", &update.after_credential.id)
                .await
                .unwrap(),
            Some(update.after_credential.clone())
        );
    }
    let mut forged_update = update.clone();
    forged_update
        .before_source
        .as_mut()
        .unwrap()
        .auxiliary_material_refs
        .insert("foreign".into(), SecretRef("sec:foreign".into()));
    assert!(
        repo.commit_managed_mutation(&forged_update).await.is_err(),
        "M5"
    );
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![update_reclaiming.clone()],
        "M5"
    );
    repo.complete_managed_mutation(&update_reclaiming)
        .await
        .unwrap();

    // R5 a later valid mutation reuses the still-durable event id but advances
    // the child revision => insert collision rejects the entire publication.
    // Public repository commands seed the collision, so this same rollback
    // witness runs on all adapters without a parallel test-only insertion port.
    let mut next_child = update.after_credential.clone();
    next_child.revision += 1;
    let mut next_source = update.after_source.clone();
    next_source.version += 1;
    next_source.material_ref = Some(
        awaken_credential_vault::repo::application_mcp_material_ref(
            &next_source.id,
            "generation-three",
            3,
        )
        .unwrap(),
    );
    next_source.descriptor.as_mut().unwrap().expires_at_unix_ms = Some(12_000);
    let collision_update = PendingManagedCredentialMutation::change(
        update.operation_id.clone(),
        ManagedCredentialOperation::Update,
        update.after_source.clone(),
        next_source,
        update.after_credential.clone(),
        next_child,
        true,
        1_000,
    )
    .unwrap();
    assert!(
        repo.begin_managed_mutation(collision_update.clone(), 1_000)
            .await
            .unwrap()
    );
    let collision_update = repo
        .mark_managed_mutation_ready(&collision_update)
        .await
        .unwrap();
    assert!(
        matches!(
            repo.commit_managed_mutation(&collision_update).await,
            Err(awaken_credential_vault::catalog::ManagedCredentialMutationError::RevisionConflict)
        ),
        "rollout R5"
    );
    assert_eq!(
        repo.get(&update.after_source.id).await.unwrap(),
        update.after_source,
        "outbox conflict rolls back the attempted expiry/material/revision advance"
    );
    assert_eq!(
        repo.get_vault_credential("ws", &update.after_credential.id)
            .await
            .unwrap(),
        Some(update.after_credential.clone())
    );
    assert_eq!(repo.pending_managed_rollouts().await.unwrap(), events);
    assert_eq!(
        repo.pending_managed_mutations().await.unwrap(),
        vec![collision_update.clone()]
    );
    let aborted = repo
        .abort_managed_mutation(&collision_update)
        .await
        .unwrap();
    repo.complete_managed_mutation(&aborted).await.unwrap();
    let mut absent = event.clone();
    absent.id.push('x');
    repo.complete_managed_rollout(&absent).await.unwrap();
    assert_eq!(
        repo.pending_managed_rollouts().await.unwrap(),
        events,
        "rollout R3"
    );
    repo.complete_managed_rollout(&event).await.unwrap();
    assert!(
        repo.pending_managed_rollouts().await.unwrap().is_empty(),
        "rollout R4"
    );
    repo.complete_managed_rollout(&event).await.unwrap();

    // R6-R8 cross independent source/child transactions, a shared event key,
    // and left-first/right-first/concurrent scheduling. Each candidate is
    // valid against its OWN unchanged before-pair: only the outbox key can
    // cause rejection. Exactly one pair and Reclaiming WAL publish; the loser
    // retains its complete before-pair and Ready WAL. A third committed event
    // is a spectator for every commit and ACK (frame, not just cardinality).
    // R9 losing-payload ACK rejects while the winner exists; after exact ACK
    // it is an absent-key no-op. R10 retries of the winner preserve its WAL
    // and event. These are native transaction witnesses, not schedule-complete
    // Rust/SQL refinement; join! overlaps yielding SQL calls, while synchronous
    // adapters serialize at their existing command lock.
    for schedule in ["left-first", "right-first", "concurrent"] {
        let shared_id = format!("rollout-race:{schedule}");
        let mut candidates = Vec::new();
        for index in 0..3 {
            let mut initial_source = owner.after_source.clone();
            initial_source.id = CredentialSourceId(format!("cred:rollout:{schedule}:{index}"));
            // Distinct vaults avoid serializing PostgreSQL contenders at the
            // parent row lock before they reach the shared outbox unique key.
            let mut candidate_vault = repo
                .get_vault("ws", &owner.after_credential.vault_id)
                .await
                .unwrap()
                .unwrap();
            candidate_vault.id = format!("vault:rollout:{schedule}:{index}");
            repo.insert_vault("ws", candidate_vault.clone())
                .await
                .unwrap();
            let mut initial_child = owner.after_credential.clone();
            initial_child.vault_id = candidate_vault.id;
            initial_child.id = format!("child:rollout:{schedule}:{index}");
            initial_child.source_id = initial_source.id.clone();
            let create = PendingManagedCredentialMutation::create(
                initial_source.clone(),
                initial_child.clone(),
                1_000,
            )
            .unwrap();
            assert!(
                repo.begin_managed_mutation(create.clone(), 1_000)
                    .await
                    .unwrap()
            );
            let ready = repo.mark_managed_mutation_ready(&create).await.unwrap();
            let committed = repo.commit_managed_mutation(&ready).await.unwrap();
            repo.complete_managed_mutation(&committed).await.unwrap();
            // Create namespaces material references to its real attempt; use
            // the committed pair as the next command's before-image.
            let initial_source = committed.after_source.clone();
            let mut after_source = initial_source.clone();
            after_source.version += 1;
            let mut after_child = initial_child.clone();
            after_child.revision += 1;
            after_child.display_name = Some(format!("published-{schedule}-{index}"));
            let candidate = PendingManagedCredentialMutation::change(
                if index == 2 {
                    format!("spectator:{schedule}")
                } else {
                    shared_id.clone()
                },
                ManagedCredentialOperation::Update,
                initial_source,
                after_source,
                initial_child,
                after_child,
                false,
                1_000,
            )
            .unwrap();
            assert!(
                repo.begin_managed_mutation(candidate.clone(), 1_000)
                    .await
                    .unwrap()
            );
            candidates.push(candidate);
        }
        let spectator_wal = repo.commit_managed_mutation(&candidates[2]).await.unwrap();
        let spectator = repo
            .managed_rollout(&candidates[2].operation_id)
            .await
            .unwrap()
            .unwrap();
        let (left, right) = match schedule {
            "left-first" => (
                repo.commit_managed_mutation(&candidates[0]).await,
                repo.commit_managed_mutation(&candidates[1]).await,
            ),
            "right-first" => {
                let right = repo.commit_managed_mutation(&candidates[1]).await;
                (repo.commit_managed_mutation(&candidates[0]).await, right)
            }
            "concurrent" => tokio::join!(
                repo.commit_managed_mutation(&candidates[0]),
                repo.commit_managed_mutation(&candidates[1]),
            ),
            _ => unreachable!(),
        };
        let (winner, winner_wal, rejected) = match (left, right) {
            (Ok(wal), Err(error)) => (0, wal, error),
            (Err(error), Ok(wal)) => (1, wal, error),
            outcomes => panic!("R6 one atomic winner: {schedule}: {outcomes:?}"),
        };
        assert!(
            matches!(
                rejected,
                awaken_credential_vault::catalog::ManagedCredentialMutationError::RevisionConflict
            ),
            "R6 exact event collision: {schedule}"
        );
        if schedule != "concurrent" {
            assert_eq!(
                winner,
                usize::from(schedule == "right-first"),
                "R7 both serial winners"
            );
        }
        let loser = 1 - winner;
        let event = repo.managed_rollout(&shared_id).await.unwrap().unwrap();
        // Independent event oracle: expected identities/revisions come from
        // the requested winning pair, never from the production constructor.
        assert_eq!(event.id, shared_id);
        assert_eq!(event.workspace_id, "ws");
        assert_eq!(event.vault_id, candidates[winner].after_credential.vault_id);
        assert_eq!(event.credential_id, candidates[winner].after_credential.id);
        assert_eq!(event.source_id, candidates[winner].after_source.id);
        assert_eq!(event.source_version, 2);
        assert_eq!(event.credential_revision, 2);
        assert_eq!(event.operation, ManagedCredentialOperation::Update);
        let mut expected_events = vec![event.clone(), spectator.clone()];
        expected_events.sort_by(|left, right| left.id.cmp(&right.id));
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(durable_events, expected_events, "R8 outbox frame");
        assert_eq!(
            repo.commit_managed_mutation(&candidates[winner])
                .await
                .unwrap(),
            winner_wal,
            "R10"
        );
        let mut colliding_ack = event.clone();
        colliding_ack.vault_id = candidates[loser].after_credential.vault_id.clone();
        colliding_ack.credential_id = candidates[loser].after_credential.id.clone();
        colliding_ack.source_id = candidates[loser].after_source.id.clone();
        assert!(
            matches!(
                repo.complete_managed_rollout(&colliding_ack).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R9 losing ACK"
        );
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(durable_events, expected_events, "R9 outbox frame");
        for (index, candidate) in candidates.iter().enumerate() {
            let (expected_source, expected_child) = if index == loser {
                (
                    candidate.before_source.as_ref().unwrap(),
                    candidate.before_credential.as_ref().unwrap(),
                )
            } else {
                (&candidate.after_source, &candidate.after_credential)
            };
            assert_eq!(
                &repo.get(&candidate.after_source.id).await.unwrap(),
                expected_source,
                "R8 source frame"
            );
            assert_eq!(
                repo.get_vault_credential("ws", &candidate.after_credential.id)
                    .await
                    .unwrap()
                    .as_ref(),
                Some(expected_child),
                "R8 child frame"
            );
        }
        let mut expected_wals = vec![
            winner_wal.clone(),
            candidates[loser].clone(),
            spectator_wal.clone(),
        ];
        expected_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        let mut durable_wals = repo.pending_managed_mutations().await.unwrap();
        durable_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        assert_eq!(durable_wals, expected_wals, "R8 complete WAL frame");
        repo.complete_managed_rollout(&event).await.unwrap();
        repo.complete_managed_rollout(&colliding_ack).await.unwrap();
        repo.complete_managed_rollout(&event).await.unwrap();
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            vec![spectator.clone()],
            "R9 completed replay preserves spectator"
        );
        // R11 the key becomes reusable only after exact acknowledgement. The same
        // previously rejected Ready transaction can now publish its own pair.
        // Delayed ACK of the OLD payload must reject, even though the id is
        // identical. This crosses key reuse with payload comparison (ABA),
        // without inventing a second event-insertion path or a permanent id
        // tombstone that the production contract does not have.
        let successor_wal = repo
            .commit_managed_mutation(&candidates[loser])
            .await
            .unwrap();
        assert_eq!(
            repo.managed_rollout(&shared_id).await.unwrap(),
            Some(colliding_ack.clone()),
            "R11 exact successor event"
        );
        assert!(
            matches!(
                repo.complete_managed_rollout(&event).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R11 delayed old ACK"
        );
        assert_eq!(
            repo.commit_managed_mutation(&candidates[winner])
                .await
                .unwrap(),
            winner_wal,
            "R11 old commit replay"
        );
        let mut durable_events = repo.pending_managed_rollouts().await.unwrap();
        durable_events.sort_by(|left, right| left.id.cmp(&right.id));
        let mut successor_events = vec![colliding_ack.clone(), spectator.clone()];
        successor_events.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(
            durable_events, successor_events,
            "R11 replay preserves successor and spectator"
        );
        for candidate in &candidates {
            assert_eq!(
                repo.get(&candidate.after_source.id).await.unwrap(),
                candidate.after_source,
                "R11 committed source frame"
            );
            assert_eq!(
                repo.get_vault_credential("ws", &candidate.after_credential.id)
                    .await
                    .unwrap(),
                Some(candidate.after_credential.clone()),
                "R11 committed child frame"
            );
        }
        let mut durable_wals = repo.pending_managed_mutations().await.unwrap();
        durable_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        let mut completed_wals = vec![
            winner_wal.clone(),
            successor_wal.clone(),
            spectator_wal.clone(),
        ];
        completed_wals.sort_by(|left, right| left.after_source.id.0.cmp(&right.after_source.id.0));
        assert_eq!(durable_wals, completed_wals, "R11 WAL frame");
        repo.complete_managed_rollout(&colliding_ack).await.unwrap();
        for finished in [&winner_wal, &successor_wal, &spectator_wal] {
            repo.complete_managed_mutation(finished).await.unwrap();
        }
        repo.complete_managed_rollout(&spectator).await.unwrap();
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        assert!(repo.pending_managed_rollouts().await.unwrap().is_empty());
    }

    for source_exists in [false, true] {
        for child_exists in [false, true] {
            let mut source = owner.after_source.clone();
            let mut child = owner.after_credential.clone();
            if !source_exists {
                source.id = CredentialSourceId("cred:managed-begin-fresh".into());
            }
            if !child_exists {
                child.id = "child-begin-fresh".into();
            }
            child.source_id = source.id.clone();
            let candidate = PendingManagedCredentialMutation::create(source, child, 1_000).unwrap();
            let result = repo.begin_managed_mutation(candidate.clone(), 1_000).await;
            if source_exists || child_exists {
                assert!(
                    matches!(result, Err(CredentialError::MutationConflict(_))),
                    "M6"
                );
            } else {
                assert!(result.unwrap(), "M6");
                // This begin-only case wrote no SecretStore material. Retire
                // its exact abort envelope before inspecting the next case.
                let aborted = repo.abort_managed_mutation(&candidate).await.unwrap();
                repo.complete_managed_mutation(&aborted).await.unwrap();
            }
            assert!(
                repo.pending_managed_mutations().await.unwrap().is_empty(),
                "M6"
            );
        }
    }
    retained_receipt_lifecycle_conforms(repo).await;
}

async fn retained_receipt_lifecycle_conforms(repo: &dyn ManagedCredentialRepository) {
    // Cause/effect table across the same Memory/SQLite/Postgres runner:
    // D1 acknowledge -> exact receipt retained but not pending; D2 exact rearm
    // -> same pending fact, no source/child mutation; D3 missing/colliding rearm
    // -> reject, same frame; D4 newer pending event -> old ack remains; D5 newer
    // ack -> prune only older ack; D6 delayed older ack -> keep newest receipt.
    // All records are published by the real Managed pair transaction. Pending
    // records never disappear merely because a newer event was acknowledged.
    let vault = ManagedVault {
        id: "receipt-vault".into(),
        workspace_id: "receipt-ws".into(),
        display_name: "Receipts".into(),
        metadata: Default::default(),
        created_at: "2026-10-26T00:00:00Z".into(),
        updated_at: "2026-10-26T00:00:00Z".into(),
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    repo.insert_vault("receipt-ws", vault.clone())
        .await
        .unwrap();
    let mut initial = source("receipt-source", "receipt-ws");
    initial.material_ref = Some(SecretRef("sec:receipt-source:initial".into()));
    let child = ManagedVaultCredential {
        id: "receipt-child".into(),
        vault_id: vault.id.clone(),
        workspace_id: "receipt-ws".into(),
        source_id: initial.id.clone(),
        auth: ManagedCredentialAuth::StaticBearer {
            mcp_server_url: "https://mcp.example.test".into(),
        },
        metadata: Default::default(),
        display_name: None,
        created_at: vault.created_at.clone(),
        updated_at: vault.updated_at.clone(),
        revision: 1,
        lifecycle: ManagedCredentialLifecycle::Active,
    };
    let create = PendingManagedCredentialMutation::create(initial, child, 1000).unwrap();
    repo.begin_managed_mutation(create.clone(), 1000)
        .await
        .unwrap();
    let ready = repo.mark_managed_mutation_ready(&create).await.unwrap();
    let committed = repo.commit_managed_mutation(&ready).await.unwrap();
    repo.complete_managed_mutation(&committed).await.unwrap();
    let mut prior: Option<awaken_credential_contract::ManagedCredentialRollout> = None;
    for revision in 2..=4 {
        let before = repo.get(&create.after_source.id).await.unwrap();
        let child = repo
            .get_vault_credential("receipt-ws", &create.after_credential.id)
            .await
            .unwrap()
            .unwrap();
        let mut after = child.clone();
        after.revision += 1;
        let pending = PendingManagedCredentialMutation::change(
            format!("receipt-event-{revision}"),
            ManagedCredentialOperation::Update,
            before.clone(),
            before.clone(),
            child,
            after,
            false,
            1000,
        )
        .unwrap();
        repo.begin_managed_mutation(pending.clone(), 1000)
            .await
            .unwrap();
        let committed = repo.commit_managed_mutation(&pending).await.unwrap();
        repo.complete_managed_mutation(&committed).await.unwrap();
        let event = repo
            .managed_rollout(&pending.operation_id)
            .await
            .unwrap()
            .unwrap();
        if let Some(previous) = prior.as_ref() {
            assert!(
                repo.managed_rollout(&previous.id).await.unwrap().is_some(),
                "D4 pending successor cannot prune ack"
            );
        }
        if revision == 4 {
            repo.rearm_managed_rollout(prior.as_ref().unwrap())
                .await
                .unwrap();
        }
        repo.complete_managed_rollout(&event).await.unwrap();
        assert_eq!(
            repo.managed_rollout(&event.id).await.unwrap(),
            Some(event.clone()),
            "D1 exact ack retained"
        );
        assert!(
            !repo
                .pending_managed_rollouts()
                .await
                .unwrap()
                .contains(&event),
            "D1 not background pending"
        );
        if revision == 2 {
            let source_before = repo.get(&before.id).await.unwrap();
            let child_before = repo
                .get_vault_credential("receipt-ws", &create.after_credential.id)
                .await
                .unwrap();
            let mut collision = event.clone();
            collision.source_id.0.push('x');
            assert!(
                matches!(
                    repo.rearm_managed_rollout(&collision).await,
                    Err(CredentialError::MutationConflict(_))
                ),
                "D3 collision"
            );
            let mut absent = event.clone();
            absent.id.push('x');
            assert!(
                matches!(
                    repo.rearm_managed_rollout(&absent).await,
                    Err(CredentialError::MutationConflict(_))
                ),
                "D3 absent proof"
            );
            repo.rearm_managed_rollout(&event).await.unwrap();
            assert!(
                repo.pending_managed_rollouts()
                    .await
                    .unwrap()
                    .contains(&event),
                "D2 same event pending"
            );
            assert_eq!(
                repo.get(&before.id).await.unwrap(),
                source_before,
                "D2 source unchanged"
            );
            assert_eq!(
                repo.get_vault_credential("receipt-ws", &create.after_credential.id)
                    .await
                    .unwrap(),
                child_before,
                "D2 child unchanged"
            );
            repo.complete_managed_rollout(&event).await.unwrap();
        }
        if let Some(previous) = prior.as_ref() {
            if revision == 4 {
                assert!(
                    repo.pending_managed_rollouts()
                        .await
                        .unwrap()
                        .contains(previous),
                    "D5 pending predecessor retained"
                );
                repo.complete_managed_rollout(previous).await.unwrap();
                assert_eq!(
                    repo.managed_rollout(&event.id).await.unwrap(),
                    Some(event.clone()),
                    "D6 newest receipt survives delayed ack"
                );
            }
            assert!(
                repo.managed_rollout(&previous.id).await.unwrap().is_none(),
                "D5/D6 only superseded ack pruned"
            );
        }
        prior = Some(event);
    }
}
