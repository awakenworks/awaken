// @formal evidence credential.managed_rollout_abstract_ack_requires_every_target_adoption
//! The Coordinator's Session adoption of credential rollouts over the real
//! Session application, repository and Managed projection (moved with the
//! adoption from the Managed adapter).

use std::sync::Arc;

use awaken_protocol_managed_testkit::{
    RecordingProjectionRefresh, RehydrateFake, RolloutCredentialSource, create_session_fixture,
    ephemeral_session_repo, paired_projection_refresh, persisted_with_mcp_source,
};
use awaken_session_state_contract::SessionExecutionState;
use awaken_session_types::SessionId;

use crate::SessionCredentialRolloutAdoption;

fn rollout_event(
    id: &str,
    version: u64,
    operation: awaken_credential_contract::ManagedCredentialOperation,
) -> awaken_credential_contract::ManagedCredentialRollout {
    awaken_credential_contract::ManagedCredentialRollout {
        id: id.into(),
        workspace_id: "workspace-a".into(),
        vault_id: "vault-event".into(),
        credential_id: "credential-a".into(),
        source_id: awaken_credential_contract::CredentialSourceId("source-a".into()),
        source_version: version,
        credential_revision: version,
        operation,
    }
}

#[tokio::test]
async fn vault_rollout_discovers_exact_actual_source_and_preserves_unrelated_roots() {
    // Vault rollout cause/effect graph. C1 a committed Vault event proves
    // Workspace A + source A while the affected Session's immutable authoring
    // baseline names a different Vault; C2 a Workspace-A Session names the
    // event Vault but actually pins source B; C3 a Workspace-B Session pins
    // source A; C4 operation is update/archive/delete. Effects: E1 update only
    // advances the Workspace-A source-A pin; E2 archive removes only that
    // dependency; E3 delete replay finds no remaining target and stutters; E4
    // source B and other-Workspace roots remain byte-for-byte unchanged.
    //
    // | Rule | Workspace | actual source | operation | Effect |
    // V1 also carries an older Active but unacknowledged generation. Staging
    // both versions then activating the replacement makes the old one Draining;
    // Publish must use only the new generation's actual receipt. Publishing the
    // complete pre-activation Stage set instead makes acknowledgement fail.
    // | V1 | A | A | update | E1 |
    // | V2 | A | B | any | E4 unrelated |
    // | V3 | B | A | any | E4 scope fence |
    // | V4 | A | A | archive | E2 |
    // | V5 | A | A absent | delete | E3 |
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    create_session_fixture(
        repo.as_ref(),
        "workspace-a",
        persisted_with_mcp_source("source-affected", "source-a", "authored-vault-other"),
    )
    .await;
    create_session_fixture(
        repo.as_ref(),
        "workspace-a",
        persisted_with_mcp_source("source-unrelated", "source-b", "vault-event"),
    )
    .await;
    create_session_fixture(
        repo.as_ref(),
        "workspace-b",
        persisted_with_mcp_source("source-other-workspace", "source-a", "vault-event"),
    )
    .await;
    let unrelated_before = repo
        .get(&SessionId::from("source-unrelated"))
        .await
        .unwrap();
    let other_workspace_before = repo
        .get(&SessionId::from("source-other-workspace"))
        .await
        .unwrap();
    let state = Arc::new(
        {
            let session_fixture_runtime = std::sync::Arc::new(RehydrateFake::default());
            let session_fixture_clock = awaken_time::system_clock();
            awaken_protocol_managed_testkit::managed_state_from_parts(
                session_fixture_runtime.clone(),
                Some(session_fixture_runtime),
                repo.clone(),
                awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                    .execution,
                session_fixture_clock,
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
        }
        .with_credential_source(Arc::new(RolloutCredentialSource)),
    );
    let state =
        SessionCredentialRolloutAdoption::over_managed(&state.session_application(), &state);

    // Publication regression rule: old active + replacement staged => publish
    // only the new pending generation and drain the old one. A receipt for the
    // old generation must not inflate the acknowledgement set from one to two.
    let updated = rollout_event(
        "rollout-source-update",
        2,
        awaken_credential_contract::ManagedCredentialOperation::Update,
    );
    let outcome =
        awaken_credential_contract::ManagedCredentialRolloutTarget::rollout(&*state, &updated)
            .await
            .unwrap();
    assert!(outcome.is_converged(), "V1/E1: {outcome:?}");
    let affected = repo.get(&SessionId::from("source-affected")).await.unwrap();
    assert_eq!(
        affected.mcp.desired_attachments()[0]
            .credential
            .as_ref()
            .unwrap()
            .credential
            .revision,
        2,
        "V1/E1"
    );
    assert_eq!(
        repo.get(&SessionId::from("source-unrelated"))
            .await
            .unwrap(),
        unrelated_before,
        "V2/E4"
    );
    assert_eq!(
        repo.get(&SessionId::from("source-other-workspace"))
            .await
            .unwrap(),
        other_workspace_before,
        "V3/E4"
    );

    let archived = rollout_event(
        "rollout-source-archive",
        3,
        awaken_credential_contract::ManagedCredentialOperation::Archive,
    );
    assert!(
        awaken_credential_contract::ManagedCredentialRolloutTarget::rollout(&*state, &archived)
            .await
            .unwrap()
            .is_converged(),
        "V4/E2"
    );
    assert!(
        repo.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "workspace-a",
                source_id: &awaken_credential_contract::CredentialSourceId("source-a".into())
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap()
        .is_empty(),
        "V4/E2"
    );
    let after_archive = repo.get(&SessionId::from("source-affected")).await.unwrap();
    assert!(after_archive.mcp.desired_attachments().is_empty(), "V4/E2");

    let deleted = rollout_event(
        "rollout-source-delete",
        4,
        awaken_credential_contract::ManagedCredentialOperation::Delete,
    );
    assert!(
        awaken_credential_contract::ManagedCredentialRolloutTarget::rollout(&*state, &deleted)
            .await
            .unwrap()
            .is_converged(),
        "V5/E3"
    );
    assert_eq!(
        repo.get(&SessionId::from("source-affected")).await.unwrap(),
        after_archive,
        "V5/E3"
    );
    assert_eq!(
        repo.get(&SessionId::from("source-unrelated"))
            .await
            .unwrap(),
        unrelated_before,
        "V2/E4"
    );
    assert_eq!(
        repo.get(&SessionId::from("source-other-workspace"))
            .await
            .unwrap(),
        other_workspace_before,
        "V3/E4"
    );
}

#[tokio::test]
async fn vault_rollout_reports_busy_and_postcommit_failure_then_replays_exact_event() {
    // Causes: C1 busy root, C2 idle root, C3 projection refresh unavailable,
    // C4 publication fails after commit, C5 exact event redelivery after recovery. Rules: R1 C1+C2 -> Pending,
    // idle sibling advances while busy root stays byte-identical; R2 C2+C3 ->
    // Pending/precommit diagnostic, revision 1; R3 C2+C4 -> Pending/postcommit
    // diagnostic, revision 2; R4 C5 ->
    // Converged without a second credential transition. Untrusted errors must
    // never enter diagnostics. Workspace/source discovery is covered by V1-V5.
    use awaken_credential_contract::ManagedCredentialOperation;
    use awaken_credential_contract::{
        ManagedCredentialAdoptionProgress, ManagedCredentialRolloutTarget,
    };
    use tracing::instrument::WithSubscriber;
    for case in ["busy", "precommit", "postcommit"] {
        let failed_refresh = case == "precommit";
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            Arc::new(ephemeral_session_repo().await);
        let mut busy = persisted_with_mcp_source("busy", "source-a", "vault-event");
        busy.execution = SessionExecutionState::Running;
        if case == "busy" {
            create_session_fixture(repo.as_ref(), "workspace-a", busy).await;
        }
        create_session_fixture(
            repo.as_ref(),
            "workspace-a",
            persisted_with_mcp_source("idle", "source-a", "vault-event"),
        )
        .await;
        let busy_before = repo.get(&SessionId::from("busy")).await.ok();
        let refresh = Arc::new(RecordingProjectionRefresh {
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail: std::sync::atomic::AtomicBool::new(failed_refresh),
        });
        let runtime = RehydrateFake::default();
        runtime
            .reject_mcp_publication
            .store(case == "postcommit", std::sync::atomic::Ordering::SeqCst);
        let managed = Arc::new(
            {
                let session_fixture_runtime = std::sync::Arc::new(runtime.clone());
                let session_fixture_clock = awaken_time::system_clock();
                awaken_protocol_managed_testkit::managed_state_from_parts(
                    session_fixture_runtime.clone(),
                    Some(session_fixture_runtime),
                    repo.clone(),
                    awaken_protocol_managed_testkit::environment_fixture(
                        session_fixture_clock.clone(),
                    )
                    .execution,
                    session_fixture_clock,
                    std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
            }
            .with_credential_source(Arc::new(RolloutCredentialSource))
            .with_executable_projection_refresh(paired_projection_refresh(refresh.clone())),
        );
        let state = SessionCredentialRolloutAdoption::over_managed(
            &managed.session_application(),
            &managed,
        );
        let event = rollout_event("rollout-retry", 2, ManagedCredentialOperation::Update);
        let captured = awaken_reliability_testkit::events::CapturedEvents::default();
        assert_eq!(
            state
                .rollout(&event)
                .with_subscriber(captured.clone())
                .await
                .unwrap(),
            ManagedCredentialAdoptionProgress::Pending
        );
        let diagnostics = captured.snapshot();
        let expected = if case == "postcommit" {
            "projection_after_commit"
        } else {
            "session_adoption"
        };
        let diagnostic = diagnostics
            .iter()
            .find(|fields| fields.get("phase").is_some_and(|phase| phase == expected))
            .unwrap_or_else(|| panic!("correlated failure ({failed_refresh}): {diagnostics:?}"));
        assert_eq!(diagnostic["rollout_id"], event.id);
        assert_eq!(
            diagnostic["session_id"],
            if case == "busy" { "busy" } else { "idle" }
        );
        assert!(!format!("{diagnostics:?}").contains("secret-"));
        assert_eq!(
            repo.get(&SessionId::from("idle"))
                .await
                .unwrap()
                .mcp
                .desired_attachments()[0]
                .credential
                .as_ref()
                .unwrap()
                .credential
                .revision,
            if failed_refresh { 1 } else { 2 }
        );
        if let Some(before) = busy_before {
            assert_eq!(
                repo.get(&SessionId::from("busy")).await.unwrap(),
                before,
                "R1"
            );
        } else {
            runtime
                .reject_mcp_publication
                .store(false, std::sync::atomic::Ordering::SeqCst);
            refresh
                .fail
                .store(false, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                state.rollout(&event).await.unwrap(),
                ManagedCredentialAdoptionProgress::Converged,
                "R3"
            );
            let recovered = repo.get(&SessionId::from("idle")).await.unwrap();
            assert_eq!(
                state.rollout(&event).await.unwrap(),
                ManagedCredentialAdoptionProgress::Converged
            );
            assert_eq!(
                repo.get(&SessionId::from("idle")).await.unwrap(),
                recovered,
                "exact replay does not mutate"
            );
        }
    }
}
