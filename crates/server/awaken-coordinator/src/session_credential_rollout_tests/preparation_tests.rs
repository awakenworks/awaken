//! Credential rotation must not deadlock an unattempted, reserved Session.
use super::*;
use awaken_credential_contract::ManagedCredentialRolloutTarget as _;
use awaken_session_state_contract::{PersistedSession, SessionAggregate};

fn preparing_session(with_activity: bool) -> PersistedSession {
    let old = persisted_with_mcp_source("preparing", "source-a", "vault-event");
    let mut baseline = old.frozen_baseline().unwrap().clone();
    baseline.mutation_policy =
        awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen;
    baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    let mut mcp = old.mcp;
    mcp.attachments[0].state =
        awaken_session_configuration_contract::mcp::McpAttachmentState::Requested;
    mcp.attachments[0].publication_acknowledged = false;
    mcp.attachments[0].realization = None;
    let mut root = PersistedSession::frozen_with_budget(
        "preparing",
        baseline,
        Default::default(),
        mcp,
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    root.created_at_unix_ms = Some(1);
    if with_activity {
        let mut aggregate = SessionAggregate::try_from(root).unwrap();
        assert_eq!(
            aggregate.activity().open(
                None,
                true,
                false,
                1,
                0,
                Some((
                    "reserved-operation",
                    &awaken_agent_contract::ThreadId("preparing".into()),
                    &awaken_agent_contract::RunId("reserved-run".into())
                )),
            ),
            awaken_session_state_contract::SessionActivityAdmissionPlan::Open(1)
        );
        root = aggregate.into_snapshot();
    }
    root
}

#[tokio::test]
async fn worker_rollout_waits_for_actual_adoption_and_keeps_draining_sources_discoverable() {
    // Causes: C1 Idle Worker-owned root with Active/ACK source v1; C2 committed
    // Update v2 or Archive/Delete; C3 no Worker phase result yet; C4 exact event
    // replay. Effects: E1 same lease and Session, E2 Pending rather than a
    // local realization or fabricated adoption, E3 exact-source discovery
    // survives until every live generation is terminal, E4 replay is inert.
    // R1=C1+Update+C3 -> Requested v2 + E1/E2/E3; R2=C1+revocation+C3 ->
    // no desired source but Draining still gives E1/E2/E3; R3=(R1|R2)+C4->E4.
    use awaken_credential_contract::{
        ManagedCredentialAdoptionProgress, ManagedCredentialOperation,
    };
    use awaken_session_configuration_contract::mcp::McpAttachmentState;
    for operation in [
        ManagedCredentialOperation::Update,
        ManagedCredentialOperation::Archive,
        ManagedCredentialOperation::Delete,
    ] {
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let id = SessionId::from("worker-rollout");
        let mut root = persisted_with_mcp_source(id.as_str(), "source-a", "vault-event");
        root.execution = SessionExecutionState::Idle;
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut root.baseline
        else {
            panic!("frozen fixture");
        };
        baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            epoch: 3,
            expires_at_unix_ms: u64::MAX,
        };
        root.realization = Some(lease.clone());
        root.mcp.attachments[0].state = McpAttachmentState::Active;
        root.mcp.attachments[0].publication_acknowledged = true;
        if let Some(claim) = &mut root.mcp.attachments[0].realization {
            claim
                .runtime_incarnation
                .clone_from(&lease.runtime_incarnation);
            claim.lease_epoch = lease.epoch;
            claim.lease_expires_at_unix_ms = lease.expires_at_unix_ms;
        }
        create_session_fixture(repo.as_ref(), "workspace-a", root).await;
        let runtime = Arc::new(RehydrateFake::default());
        let clock = awaken_time::system_clock();
        let managed = Arc::new(
            awaken_protocol_managed_testkit::managed_state_from_parts(
                runtime.clone(),
                Some(runtime),
                repo.clone(),
                awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
                clock,
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .with_credential_source(Arc::new(RolloutCredentialSource)),
        );
        let target = SessionCredentialRolloutAdoption::over_managed(
            &managed.session_application(),
            &managed,
        );
        let event = rollout_event("worker-pending", 2, operation);
        assert_eq!(
            target.rollout(&event).await.unwrap(),
            ManagedCredentialAdoptionProgress::Pending,
            "R1/R2"
        );
        let after = repo.get(&id).await.unwrap();
        assert_eq!(after.realization.as_ref(), Some(&lease), "E1");
        assert_eq!(
            after.execution,
            SessionExecutionState::Idle,
            "E1 no model Run"
        );
        assert!(
            after.is_run_free_preparation_candidate(),
            "E2 same root is work"
        );
        if operation == ManagedCredentialOperation::Update {
            let desired = after.mcp.desired_attachments();
            assert_eq!(desired.len(), 1, "R1");
            assert_eq!(
                desired[0].credential.as_ref().unwrap().credential.revision,
                2,
                "R1"
            );
            assert_eq!(
                desired[0].state,
                McpAttachmentState::Requested,
                "R1 no fabricated ACK"
            );
        } else {
            assert!(
                after.mcp.desired_attachments().is_empty(),
                "R2 desired source removed"
            );
            assert!(
                after
                    .mcp
                    .attachments
                    .iter()
                    .any(|a| a.state == McpAttachmentState::Draining),
                "R2 physical drain remains"
            );
        }
        let references = repo
            .scan(
                awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                    workspace_id: "workspace-a",
                    source_id: &event.source_id,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            references.sessions.len(),
            1,
            "E3 source is still discoverable"
        );
        assert_eq!(
            target.rollout(&event).await.unwrap(),
            ManagedCredentialAdoptionProgress::Pending,
            "R3"
        );
        assert_eq!(
            repo.get(&id).await.unwrap(),
            after,
            "E4 exact replay is inert"
        );
    }
}

#[tokio::test]
async fn unattempted_preparation_rotates_credentials_without_losing_reserved_activity() {
    // Causes: C1 frozen Preparing root with known-empty materialization; C2 no
    // realization/MCP effect admitted; C3 newer same-source credential; C4 a Run
    // activity already reserved. R1=C1+C2+C3+!C4 and R2=C1+C2+C3+C4 -> the same
    // root adopts the desired pin through the real update CAS, preserving every
    // activity coordinate. Runtime publication can remain pending: a changed
    // pin is not convergence. This reproduces the deployed pre-Run wait cycle.
    for with_activity in [false, true] {
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        create_session_fixture(
            repo.as_ref(),
            "workspace-a",
            preparing_session(with_activity),
        )
        .await;
        let id = SessionId::from("preparing");
        let before = repo.get(&id).await.unwrap();
        let runtime = Arc::new(RehydrateFake::default());
        let clock = awaken_time::system_clock();
        let managed = Arc::new(
            awaken_protocol_managed_testkit::managed_state_from_parts(
                runtime.clone(),
                Some(runtime),
                repo.clone(),
                awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
                clock,
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .with_credential_source(Arc::new(RolloutCredentialSource)),
        );
        let target = SessionCredentialRolloutAdoption::over_managed(
            &managed.session_application(),
            &managed,
        );
        let event = rollout_event(
            "preparation-rotation",
            2,
            awaken_credential_contract::ManagedCredentialOperation::Update,
        );
        let _progress = target.rollout(&event).await.unwrap();
        let after = repo.get(&id).await.unwrap();
        let desired = after.mcp.desired_attachments();
        assert_eq!(desired.len(), 1);
        assert_eq!(
            desired[0].credential.as_ref().unwrap().credential.revision,
            2,
            "R1/R2 desired credential must advance before the reserved Run can execute"
        );
        assert!(
            after
                .mcp
                .attachments
                .iter()
                .filter(|attachment| attachment
                    .credential
                    .as_ref()
                    .is_some_and(|access| access.credential.revision == 1))
                .all(|attachment| attachment.state
                    == awaken_session_configuration_contract::mcp::McpAttachmentState::Removed),
            "old unattempted credential cannot enter Stage before its successor"
        );
        assert_eq!(after.active_activity_epochs, before.active_activity_epochs);
        assert_eq!(after.run_activities(), before.run_activities());
        assert_eq!(
            after.environment_materialization(),
            before.environment_materialization()
        );
    }
}
