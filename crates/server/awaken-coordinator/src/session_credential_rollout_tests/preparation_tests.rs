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
