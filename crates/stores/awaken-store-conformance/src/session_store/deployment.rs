// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-store-conformance/src/session_store.rs
// @formal owner crates/stores/awaken-session-store/tests/session_repo_conformance.rs
// @formal reason: DeploymentRepository CAS, capacity and scheduled-claim rows split from the existing
// @formal reason: SessionStore conformance owner because both ports share the same root transaction. It
// @formal reason: remains tooling-only and creates no product admission path.
use super::*;

pub fn deployment_record(id: &str, revision: u64, scheduled: bool) -> DeploymentView {
    DeploymentView {
        id: id.to_string(),
        record: DeploymentRecord {
            revision,
            created_at: at(0),
            updated_at: at(0),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            environment_id: "env_a".into(),
            name: id.to_string(),
            description: None,
            metadata: Default::default(),
            initial_events: Vec::new(),
            resources: Vec::new(),
            schedule: scheduled.then(|| DeploymentSchedule::Cron {
                expression: "0 * * * *".into(),
                timezone: "UTC".into(),
            }),
            vault_ids: Vec::new(),
            budget_max_list_cost_minor: None,
            status: DeploymentStatus::Active,
            paused_reason: None,
            archived_at: None,
            last_run_at: None,
            next_fire_ms: scheduled.then_some(60_000),
        },
    }
}

pub fn deployment_fact(id: &str, object_id: &str) -> DeploymentLifecycleFact {
    DeploymentLifecycleFact {
        id: id.into(),
        object_id: object_id.into(),
        workspace_id: "ws_a".into(),
        event_type: "deployment.updated".into(),
        timestamp: 1,
    }
}

pub async fn deployment_cas_decision_table<
    R: DeploymentRepository + awaken_session_repository_contract::SessionStore,
>(
    repo: &R,
    replicas: [&dyn DeploymentRepository; 3],
) {
    // Deployment repository cause/effect graph:
    // C1=create or three writers share revision r; C2=three independent clients
    // race distinct scheduled rows at capacity one; C3=scheduler presents current
    // or stale revision; C4=a current scheduler reuses the rejected stale claim
    // identity; C5=a duplicate run row forces the claim transaction to fail.
    // Effects: E1=one CAS write advances r exactly once; E2=the loser
    // conflicts without a lifecycle fact; E3=one exact capacity winner and its
    // fact commit while both rejected candidates leave no row or fact; E4=claim,
    // run, cursor and fact commit atomically; E5=stale claim preserves the winner
    // frame and creates no claim/run/fact; E6=C4 claims, proving that E5 did not
    // reserve the durable identity; E7=C5 preserves the complete deployment,
    // creates no fact or claim, and an exact retry of that claim identity commits.
    //
    // | Rule | C1                | C2 | C3 | C4      | Effect       |
    // | D1   | create            | F  | F  | -       | applied      |
    // | D2   | jump/reuse r      | F  | F  | -       | E2 reject   |
    // | D3   | three writers at r| F  | F  | -       | E1 + E2     |
    // | D4   | three creates     | T  | limit 1 | -  | E3          |
    // | D5a  | duplicate run     | -  | -  | current | E7          |
    // | D5b  | retry D5a claim id| -  | -  | current | E4 + E7     |
    // | D6   | scheduler at r-1  | -  | -  | stale   | E5          |
    // | D7   | reuse D6 claim id | -  | -  | current | E4 + E6     |
    let baseline_facts = repo.pending_lifecycle().await.unwrap().len();
    let created = deployment_record("depl_cas", 0, false);
    assert_eq!(
        repo.write_deployment(
            created.clone(),
            None,
            1,
            Some(deployment_fact("deployment-create", "depl_cas")),
        )
        .await
        .unwrap(),
        DeploymentWriteOutcome::Applied,
        "D1"
    );
    assert_eq!(
        repo.write_deployment(
            deployment_record("depl_cas", 2, false),
            Some(0),
            1,
            Some(deployment_fact("invalid-jump", "depl_cas")),
        )
        .await
        .unwrap(),
        DeploymentWriteOutcome::Conflict,
        "D2 exact successor"
    );
    let mut left = created.clone();
    left.record.revision = 1;
    left.record.name = "left".into();
    let mut right = left.clone();
    right.record.name = "right".into();
    let mut third = right.clone();
    third.record.name = "third".into();
    let (left_outcome, right_outcome, third_outcome) = tokio::join!(
        replicas[0].write_deployment(
            left,
            Some(0),
            1,
            Some(deployment_fact("left-writer", "depl_cas")),
        ),
        replicas[1].write_deployment(
            right,
            Some(0),
            1,
            Some(deployment_fact("right-writer", "depl_cas")),
        ),
        replicas[2].write_deployment(
            third,
            Some(0),
            1,
            Some(deployment_fact("third-writer", "depl_cas")),
        )
    );
    let outcomes = [
        left_outcome.unwrap(),
        right_outcome.unwrap(),
        third_outcome.unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == DeploymentWriteOutcome::Applied)
            .count(),
        1,
        "D3/E1"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == DeploymentWriteOutcome::Conflict)
            .count(),
        2,
        "D3/E2"
    );
    let committed = replicas[0]
        .deployments()
        .await
        .unwrap()
        .into_iter()
        .find(|deployment| deployment.id == "depl_cas")
        .expect("D3 committed Deployment");
    assert_eq!(committed.record.revision, 1, "D3/E1 exact successor");
    assert!(
        ["left", "right", "third"].contains(&committed.record.name.as_str()),
        "D3/E1 exact candidate"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(committed.clone()),
            "D3 replica {index} exact winner"
        );
    }
    let facts = repo.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), baseline_facts + 2, "D1/D3 lifecycle atomicity");
    assert!(
        facts.iter().all(|fact| fact.id != "invalid-jump"),
        "D2 no fact"
    );
    // D1/E1 outbox shape: the Deployment-owned fact becomes the shared outbox
    // record with its Workspace and no Session runtime interval.
    assert_eq!(
        facts.iter().find(|fact| fact.id == "deployment-create"),
        Some(&ManagedLifecycleFact {
            id: "deployment-create".into(),
            object_id: "depl_cas".into(),
            workspace_id: Some("ws_a".into()),
            event_type: "deployment.updated".into(),
            timestamp: 1,
            runtime_interval: None,
        }),
        "D1/E1 exact outbox record"
    );
    assert_eq!(
        usize::from(facts.iter().any(|fact| fact.id == "left-writer"))
            + usize::from(facts.iter().any(|fact| fact.id == "right-writer"))
            + usize::from(facts.iter().any(|fact| fact.id == "third-writer")),
        1,
        "D3/E2 only winner fact"
    );

    let capacity_left = deployment_record("depl_capacity_left", 0, true);
    let capacity_right = deployment_record("depl_capacity_right", 0, true);
    let capacity_third = deployment_record("depl_capacity_third", 0, true);
    let (left_capacity, right_capacity, third_capacity) = tokio::join!(
        replicas[0].write_deployment(
            capacity_left,
            None,
            1,
            Some(deployment_fact("capacity-left", "depl_capacity_left")),
        ),
        replicas[1].write_deployment(
            capacity_right,
            None,
            1,
            Some(deployment_fact("capacity-right", "depl_capacity_right")),
        ),
        replicas[2].write_deployment(
            capacity_third,
            None,
            1,
            Some(deployment_fact("capacity-third", "depl_capacity_third")),
        )
    );
    let capacity_outcomes = [
        (
            "depl_capacity_left",
            "capacity-left",
            left_capacity.unwrap(),
        ),
        (
            "depl_capacity_right",
            "capacity-right",
            right_capacity.unwrap(),
        ),
        (
            "depl_capacity_third",
            "capacity-third",
            third_capacity.unwrap(),
        ),
    ];
    assert_eq!(
        capacity_outcomes
            .iter()
            .filter(|(_, _, outcome)| *outcome == DeploymentWriteOutcome::Applied)
            .count(),
        1,
        "D4/E3 one admitted transaction"
    );
    assert_eq!(
        capacity_outcomes
            .iter()
            .filter(|(_, _, outcome)| {
                *outcome == DeploymentWriteOutcome::ScheduledCapacityReached
            })
            .count(),
        2,
        "D4/E3 both excess transactions rejected"
    );
    let capacity_rows = repo
        .deployments()
        .await
        .unwrap()
        .into_iter()
        .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
        .collect::<Vec<_>>();
    assert_eq!(capacity_rows.len(), 1, "D4/E3 no rejected candidate row");
    let capacity_winner = capacity_rows.into_iter().next().unwrap();
    let applied_capacity = capacity_outcomes
        .iter()
        .find(|(_, _, outcome)| *outcome == DeploymentWriteOutcome::Applied)
        .expect("D4/E3 applied capacity identity");
    assert_eq!(
        capacity_winner.id.as_str(),
        applied_capacity.0,
        "D4/E3 Applied identifies the durable winner"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
                .collect::<Vec<_>>(),
            vec![capacity_winner.clone()],
            "D4 replica {index} exact capacity winner"
        );
    }
    let capacity_facts = repo
        .pending_lifecycle()
        .await
        .unwrap()
        .into_iter()
        .filter(|fact| fact.id.starts_with("capacity-"))
        .collect::<Vec<_>>();
    assert_eq!(capacity_facts.len(), 1, "D4/E3 no rejected candidate fact");
    assert_eq!(
        capacity_facts[0].object_id, capacity_winner.id,
        "D4/E3 exact winner row/fact pair"
    );
    assert_eq!(
        capacity_facts[0].id.as_str(),
        applied_capacity.1,
        "D4/E3 Applied identifies the durable fact"
    );

    let collision_run = DeploymentRunView {
        id: "drun_collision".into(),
        record: DeploymentRunRecord {
            created_at: at(0),
            deployment_id: "depl_cas".into(),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            trigger: DeploymentTrigger::Manual,
            session_id: None,
            error: None,
        },
    };
    repo.upsert_deployment_run(collision_run.clone(), None)
        .await
        .expect("D5a collision setup");
    let failed_claim_id = "depl_cas:failed-then-retried";
    let mut advanced = committed.clone();
    advanced.record.revision = 2;
    advanced.record.name = "advanced".into();
    assert!(
        repo.claim_scheduled_run(
            failed_claim_id,
            1,
            advanced.clone(),
            collision_run,
            DeploymentLifecycleFact {
                id: "failed-claim-fact".into(),
                object_id: "drun_collision".into(),
                workspace_id: "ws_a".into(),
                event_type: "deployment_run.started".into(),
                timestamp: 1,
            },
        )
        .await
        .is_err(),
        "D5a/E7 duplicate run rejects the transaction"
    );
    assert_eq!(
        repo.deployments()
            .await
            .unwrap()
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas"),
        Some(committed),
        "D5a/E7 deployment update rolled back"
    );
    assert!(
        repo.pending_lifecycle()
            .await
            .unwrap()
            .iter()
            .all(|fact| fact.id != "failed-claim-fact"),
        "D5a/E7 lifecycle fact rolled back"
    );
    let run = DeploymentRunView {
        id: "drun_current".into(),
        record: DeploymentRunRecord {
            created_at: at(1),
            deployment_id: "depl_cas".into(),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            trigger: DeploymentTrigger::Schedule {
                scheduled_at: at(1),
            },
            session_id: None,
            error: None,
        },
    };
    let fact = DeploymentLifecycleFact {
        id: "deployment-run-started".into(),
        object_id: run.id.clone(),
        workspace_id: "ws_a".into(),
        event_type: "deployment_run.started".into(),
        timestamp: 1,
    };
    assert_eq!(
        repo.claim_scheduled_run(failed_claim_id, 1, advanced.clone(), run, fact)
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::Claimed,
        "D5b/E4+E7"
    );
    assert_eq!(
        repo.deployments()
            .await
            .unwrap()
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas"),
        Some(advanced.clone()),
        "D5b/E4 exact deployment successor"
    );
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 2, "D5b/E4 only setup and committed run");
    assert!(runs.iter().any(|run| run.id == "drun_current"), "D5b/E4");
    assert!(
        repo.pending_lifecycle().await.unwrap().iter().any(|fact| {
            fact.id == "deployment-run-started" && fact.object_id == "drun_current"
        }),
        "D5b/E4 exact run/fact pair"
    );
    let stale_claim_id = "depl_cas:stale-then-current";
    let mut stale_overwrite = advanced.clone();
    stale_overwrite.record.name = "stale-overwrite-must-not-commit".into();
    assert_eq!(
        replicas[1]
            .claim_scheduled_run(
                stale_claim_id,
                1,
                stale_overwrite,
                DeploymentRunView {
                    id: "drun_stale".into(),
                    record: DeploymentRunRecord {
                        created_at: at(2),
                        deployment_id: "depl_cas".into(),
                        workspace_id: "ws_a".into(),
                        agent: DeploymentAgent::new("agent_a", 1),
                        trigger: DeploymentTrigger::Manual,
                        session_id: None,
                        error: None,
                    },
                },
                DeploymentLifecycleFact {
                    id: "stale-fact".into(),
                    object_id: "drun_stale".into(),
                    workspace_id: "ws_a".into(),
                    event_type: "deployment_run.started".into(),
                    timestamp: 2,
                },
            )
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::StaleDeployment,
        "D6/E5"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(advanced.clone()),
            "D6/E5 replica {index} preserves the exact winner frame"
        );
    }
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 2, "D6/E5");
    assert!(runs.iter().all(|run| run.id != "drun_stale"), "D6/E5");
    assert!(
        repo.pending_lifecycle()
            .await
            .unwrap()
            .iter()
            .all(|fact| fact.id != "stale-fact"),
        "D6/E5"
    );

    let mut after_stale = advanced;
    after_stale.record.revision = 3;
    after_stale.record.name = "fresh-after-stale".into();
    assert_eq!(
        replicas[2]
            .claim_scheduled_run(
                stale_claim_id,
                2,
                after_stale.clone(),
                DeploymentRunView {
                    id: "drun_after_stale".into(),
                    record: DeploymentRunRecord {
                        created_at: at(3),
                        deployment_id: "depl_cas".into(),
                        workspace_id: "ws_a".into(),
                        agent: DeploymentAgent::new("agent_a", 1),
                        trigger: DeploymentTrigger::Schedule {
                            scheduled_at: at(3),
                        },
                        session_id: None,
                        error: None,
                    },
                },
                DeploymentLifecycleFact {
                    id: "fresh-after-stale-fact".into(),
                    object_id: "drun_after_stale".into(),
                    workspace_id: "ws_a".into(),
                    event_type: "deployment_run.started".into(),
                    timestamp: 3,
                },
            )
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::Claimed,
        "D7/E6 stale rejection did not reserve the claim identity"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(after_stale.clone()),
            "D7/E4 replica {index} reads the fresh complete frame"
        );
    }
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 3, "D7/E4");
    assert!(runs.iter().any(|run| run.id == "drun_current"), "D7/E4");
    assert!(runs.iter().any(|run| run.id == "drun_after_stale"), "D7/E4");
    assert!(runs.iter().all(|run| run.id != "drun_stale"), "D7/E5");
    let facts = repo.pending_lifecycle().await.unwrap();
    assert!(facts.iter().all(|fact| fact.id != "stale-fact"), "D7/E5");
    assert!(
        facts.iter().any(|fact| fact.id == "fresh-after-stale-fact"),
        "D7/E4"
    );
}
