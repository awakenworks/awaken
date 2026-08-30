//! One topology-neutral driver for terminal preparation and physical disposal.
//!
//! The Session aggregate remains the only queue and phase owner. This module
//! merely executes the closed action projected by that aggregate, then returns
//! exact evidence through `SessionRealizationControl`; it persists no cursor,
//! retry record, receipt, or topology-specific state.

use super::SessionTerminalCleanupAction;
use crate::{
    RunError, SessionRealizationControl, SessionRealizationControlFailure,
    SessionRealizationDriveError, SessionRepositoryPublicationEffect, SessionRuntime,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect, SessionTerminalCleanupWork,
    realization_lease_generation_authorizes,
};

/// Observable result of one bounded drive of an already-admitted terminal
/// assignment. `Pending` means the same aggregate currently exposes no
/// executable effect; `Completed` means its physical work was durably accepted
/// or another exact actor retired the Session while this drive was in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTerminalCleanupDriveOutcome {
    Pending,
    Completed,
}

fn invalid_projection(detail: impl Into<String>) -> SessionRealizationDriveError {
    SessionRealizationControlFailure::Invalid(detail.into()).into()
}

fn workspace_matches(expected: &str, authorized: &str) -> Result<(), SessionRealizationDriveError> {
    if expected == authorized {
        Ok(())
    } else {
        Err(invalid_projection(
            "terminal cleanup Workspace differs from its frozen assignment",
        ))
    }
}

async fn next_work(
    control: &dyn SessionRealizationControl,
    session_id: &str,
    lease: &crate::SessionRealizationLease,
) -> Result<Option<SessionTerminalCleanupWork>, SessionRealizationDriveError> {
    match control.terminal_cleanup_work(session_id, lease).await {
        Ok(work) => Ok(work),
        Err(SessionRealizationControlFailure::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Drive the one closed cleanup action until it completes, reaches a durable
/// wait boundary, loses authority, or fails an effect.
///
/// Local composition and a remote Worker must both call this function. The
/// asserted lease identifies an already-admitted terminal assignment;
/// every action is re-read through `control` after the preceding receipt is
/// durable.
pub async fn drive_session_terminal_cleanup(
    session_id: &str,
    asserted_lease: &crate::SessionRealizationLease,
    control: &dyn SessionRealizationControl,
    runtime: &dyn SessionRuntime,
) -> Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError> {
    let Some(mut work) = next_work(control, session_id, asserted_lease).await? else {
        runtime
            .acknowledge_completed_terminal_cleanup(session_id, asserted_lease)
            .await;
        return Ok(SessionTerminalCleanupDriveOutcome::Completed);
    };
    for _ in 0..8 {
        if work.assignment.session_id != session_id {
            return Err(invalid_projection(
                "terminal cleanup projection changed its Session root",
            ));
        }
        runtime
            .install_terminal_cleanup_assignment(&work.assignment)
            .await?;
        let lease = work.assignment.lease.clone();
        let expected_workspace = work.assignment.projection.workspace_id.clone();

        match work.action {
            SessionTerminalCleanupAction::Waiting => {
                let projection = match control
                    .terminal_repository_publication_command(session_id, &lease)
                    .await
                {
                    Ok(projection) => projection,
                    Err(SessionRealizationControlFailure::NotFound) => {
                        runtime
                            .acknowledge_completed_terminal_cleanup(session_id, &lease)
                            .await;
                        return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some(projection) = projection else {
                    return Ok(SessionTerminalCleanupDriveOutcome::Pending);
                };
                if projection.command.session_id != session_id {
                    return Err(invalid_projection(
                        "terminal Repository publication targets another Session root",
                    ));
                }
                workspace_matches(&expected_workspace, &projection.workspace_id)?;
                if !realization_lease_generation_authorizes(&projection.current_lease, &lease) {
                    return Err(invalid_projection(
                        "terminal Repository publication realization generation was replaced",
                    ));
                }
                let current_lease = projection.current_lease;
                let effect = runtime
                    .execute_terminal_repository_publication_for_lease(
                        projection.command,
                        &current_lease,
                    )
                    .await?;
                let admission = match effect {
                    SessionRepositoryPublicationEffect::Published(receipt) => {
                        control
                            .record_terminal_repository_publication_receipt(
                                session_id,
                                &current_lease,
                                receipt,
                            )
                            .await
                    }
                    SessionRepositoryPublicationEffect::Rejected(rejection) => {
                        control
                            .record_terminal_repository_publication_rejection(
                                session_id,
                                &current_lease,
                                rejection,
                            )
                            .await
                    }
                };
                match admission {
                    Ok(()) => {}
                    Err(SessionRealizationControlFailure::NotFound) => {
                        runtime
                            .acknowledge_completed_terminal_cleanup(session_id, &lease)
                            .await;
                        return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            SessionTerminalCleanupAction::Prepare { commands } => {
                if commands.is_empty() {
                    return Err(invalid_projection(
                        "terminal cleanup preparation action is empty",
                    ));
                }
                let mut first_effect_error: Option<RunError> = None;
                for command in commands {
                    if command.session_id != session_id {
                        return Err(invalid_projection(
                            "terminal cleanup command targets another Session root",
                        ));
                    }
                    let effect = SessionTerminalCleanupEffect::new(command, lease.clone());
                    let authorization =
                        match control.authorize_terminal_cleanup_effect(&effect).await {
                            Ok(authorization) => authorization,
                            Err(SessionRealizationControlFailure::NotFound) => {
                                runtime
                                    .acknowledge_completed_terminal_cleanup(session_id, &lease)
                                    .await;
                                return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                            }
                            Err(error) => return Err(error.into()),
                        };
                    authorization.verify_for(&effect)?;
                    workspace_matches(&expected_workspace, authorization.workspace_id())?;
                    let preparation = match runtime
                        .prepare_terminal_cleanup_for_effect(effect.clone(), authorization)
                        .await
                    {
                        Ok(preparation) => preparation,
                        Err(error) => {
                            first_effect_error.get_or_insert(error);
                            continue;
                        }
                    };
                    match control
                        .record_terminal_cleanup_preparation(&lease, preparation)
                        .await
                    {
                        Ok(()) => {
                            runtime
                                .acknowledge_terminal_cleanup_preparation(&effect)
                                .await;
                        }
                        Err(SessionRealizationControlFailure::NotFound) => {
                            runtime
                                .acknowledge_terminal_cleanup_preparation(&effect)
                                .await;
                            runtime
                                .acknowledge_completed_terminal_cleanup(session_id, &lease)
                                .await;
                            return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                if let Some(error) = first_effect_error {
                    return Err(error.into());
                }
            }
            SessionTerminalCleanupAction::Dispose { command } => {
                if command.session_id != session_id {
                    return Err(invalid_projection(
                        "terminal cleanup disposal targets another Session root",
                    ));
                }
                let effect = SessionTerminalCleanupDisposalEffect::new(command, lease.clone());
                let authorized_workspace =
                    match control.authorize_terminal_cleanup_disposal(&effect).await {
                        Ok(workspace) => workspace,
                        Err(SessionRealizationControlFailure::NotFound) => {
                            runtime
                                .acknowledge_completed_terminal_cleanup(session_id, &lease)
                                .await;
                            return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                        }
                        Err(error) => return Err(error.into()),
                    };
                workspace_matches(&expected_workspace, &authorized_workspace)?;
                let receipt = runtime
                    .dispose_terminal_cleanup_for_effect(effect.clone())
                    .await?;
                match control
                    .record_terminal_cleanup_disposal(&lease, receipt)
                    .await
                {
                    Ok(()) | Err(SessionRealizationControlFailure::NotFound) => {
                        runtime.acknowledge_terminal_cleanup_disposal(&effect).await;
                        runtime
                            .acknowledge_completed_terminal_cleanup(session_id, &lease)
                            .await;
                        return Ok(SessionTerminalCleanupDriveOutcome::Completed);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }

        let Some(next) = next_work(control, session_id, &lease).await? else {
            runtime
                .acknowledge_completed_terminal_cleanup(session_id, &lease)
                .await;
            return Ok(SessionTerminalCleanupDriveOutcome::Completed);
        };
        work = next;
    }
    Err(SessionRealizationDriveError::DidNotConverge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
        FailSessionRealization, FrozenSessionProjection, SessionCleanupCommand,
        SessionCleanupDisposalReceipt, SessionCleanupOperation, SessionRealizationDirective,
        SessionTerminalCleanupAssignment, SessionTerminalCleanupWork, StepOutcome,
        ToolPermissionDecision,
    };
    use awaken_agent_contract::agent::content::ContentBlock;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    fn lease() -> crate::SessionRealizationLease {
        crate::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 3,
            expires_at_unix_ms: 10_000,
        }
    }

    fn publication_command() -> crate::SessionRepositoryPublicationCommand {
        serde_json::from_value(serde_json::json!({
            "session_id": "session",
            "effect_id": "publication-effect",
            "intent": {
                "input": {
                    "binding_id": "repository-binding",
                    "source": {
                        "kind": "repository",
                        "repository_id": "repository",
                        "config": {
                            "repository_id": "repository",
                            "version": 1,
                            "remote_url": "https://git.invalid/repository.git",
                            "initial_branch": "main"
                        }
                    },
                    "mount_path": "/workspace/repository",
                    "access": "read_write"
                },
                "expectation": {
                    "branch": "awf/publication",
                    "commit": "0123456789abcdef0123456789abcdef01234567",
                    "expected_prior_commit": "1111111111111111111111111111111111111111"
                }
            }
        }))
        .expect("valid publication command")
    }

    fn preparation(effect: &SessionTerminalCleanupEffect) -> crate::SessionCleanupPreparation {
        crate::SessionCleanupPreparation::try_new(
            effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap()
    }

    fn projection() -> FrozenSessionProjection {
        let baseline = crate::SessionBaseline::compile(crate::SessionBaselineInputs {
            environment: crate::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: crate::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: crate::EnvironmentFingerprint("environment-v1".into()),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: crate::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Workload,
                        "awaken.workload.acp",
                    ),
                    mcp_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                    resource_holder: awaken_credential_contract::PlaintextHolder::new(
                        awaken_credential_contract::PlaintextBoundary::Worker,
                        "awaken.worker",
                    ),
                },
            },
            runtime_placement: crate::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: crate::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            previous_resource_manifest: Some(crate::SessionResourceManifest::at_revision(
                "workspace",
                0,
                Default::default(),
            )),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    fn work(
        action: SessionTerminalCleanupAction,
        lease: &crate::SessionRealizationLease,
    ) -> SessionTerminalCleanupWork {
        SessionTerminalCleanupWork {
            assignment: SessionTerminalCleanupAssignment {
                session_id: "session".into(),
                projection: projection(),
                lease: lease.clone(),
            },
            action,
        }
    }

    struct RecordingControl {
        work: Mutex<VecDeque<SessionTerminalCleanupWork>>,
        publication: Mutex<Option<crate::SessionRepositoryPublicationProjection>>,
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl SessionRealizationControl for RecordingControl {
        async fn begin_session_realization(
            &self,
            _command: BeginSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("terminal-only fixture")
        }

        async fn activate_session_realization(
            &self,
            _command: ActivateSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("terminal-only fixture")
        }

        async fn acknowledge_session_realization(
            &self,
            _command: AcknowledgeSessionRealization,
        ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
            unreachable!("terminal-only fixture")
        }

        async fn fail_session_realization(
            &self,
            _command: FailSessionRealization,
        ) -> Result<(), SessionRealizationControlFailure> {
            unreachable!("terminal-only fixture")
        }

        async fn terminal_cleanup_work(
            &self,
            _session_id: &str,
            _lease: &crate::SessionRealizationLease,
        ) -> Result<Option<SessionTerminalCleanupWork>, SessionRealizationControlFailure> {
            self.events.lock().unwrap().push("poll".into());
            Ok(self.work.lock().unwrap().pop_front())
        }

        async fn terminal_repository_publication_command(
            &self,
            _session_id: &str,
            _lease: &crate::SessionRealizationLease,
        ) -> Result<
            Option<crate::SessionRepositoryPublicationProjection>,
            SessionRealizationControlFailure,
        > {
            self.events
                .lock()
                .unwrap()
                .push("project-publication".into());
            Ok(self.publication.lock().unwrap().clone())
        }

        async fn record_terminal_repository_publication_receipt(
            &self,
            _session_id: &str,
            lease: &crate::SessionRealizationLease,
            _receipt: crate::SessionRepositoryPublicationReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push(format!(
                "record-publication-receipt:{}:{}",
                lease.epoch, lease.expires_at_unix_ms
            ));
            Ok(())
        }

        async fn record_terminal_repository_publication_rejection(
            &self,
            _session_id: &str,
            lease: &crate::SessionRealizationLease,
            _rejection: crate::SessionRepositoryPublicationRejection,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push(format!(
                "record-publication-rejection:{}:{}",
                lease.epoch, lease.expires_at_unix_ms
            ));
            Ok(())
        }

        async fn authorize_terminal_cleanup_effect(
            &self,
            effect: &SessionTerminalCleanupEffect,
        ) -> Result<
            crate::SessionTerminalCleanupPreparationAuthorization,
            SessionRealizationControlFailure,
        > {
            self.events
                .lock()
                .unwrap()
                .push(format!("authorize:{}", effect.command.thread_id));
            crate::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                "workspace".into(),
                None,
            )
        }

        async fn record_terminal_cleanup_preparation(
            &self,
            _lease: &crate::SessionRealizationLease,
            preparation: crate::SessionCleanupPreparation,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events
                .lock()
                .unwrap()
                .push(format!("record:{}", preparation.effect.command.thread_id));
            Ok(())
        }

        async fn authorize_terminal_cleanup_disposal(
            &self,
            _effect: &SessionTerminalCleanupDisposalEffect,
        ) -> Result<String, SessionRealizationControlFailure> {
            self.events
                .lock()
                .unwrap()
                .push("authorize-disposal".into());
            Ok("workspace".into())
        }

        async fn record_terminal_cleanup_disposal(
            &self,
            _lease: &crate::SessionRealizationLease,
            _receipt: SessionCleanupDisposalReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push("record-disposal".into());
            Ok(())
        }
    }

    struct RecordingRuntime {
        events: Arc<Mutex<Vec<String>>>,
        failing_thread: Option<String>,
        publication_effect: Option<SessionRepositoryPublicationEffect>,
    }

    #[async_trait::async_trait]
    impl SessionRuntime for RecordingRuntime {
        fn model(&self) -> String {
            "test-model".into()
        }

        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("terminal-only fixture")
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("terminal-only fixture")
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("terminal-only fixture")
        }

        async fn install_terminal_cleanup_assignment(
            &self,
            _assignment: &SessionTerminalCleanupAssignment,
        ) -> Result<(), RunError> {
            self.events.lock().unwrap().push("install".into());
            Ok(())
        }

        async fn execute_terminal_repository_publication_for_lease(
            &self,
            _command: crate::SessionRepositoryPublicationCommand,
            lease: &crate::SessionRealizationLease,
        ) -> Result<SessionRepositoryPublicationEffect, RunError> {
            self.events.lock().unwrap().push(format!(
                "publish:{}:{}",
                lease.epoch, lease.expires_at_unix_ms
            ));
            self.publication_effect
                .clone()
                .ok_or_else(|| RunError::internal("missing publication fixture outcome"))
        }

        async fn prepare_terminal_cleanup_for_effect(
            &self,
            effect: SessionTerminalCleanupEffect,
            authorization: crate::SessionTerminalCleanupPreparationAuthorization,
        ) -> Result<crate::SessionCleanupPreparation, RunError> {
            authorization
                .verify_for(&effect)
                .map_err(|error| RunError::internal(error.to_string()))?;
            self.events
                .lock()
                .unwrap()
                .push(format!("prepare:{}", effect.command.thread_id));
            if self
                .failing_thread
                .as_deref()
                .is_some_and(|thread| thread == effect.command.thread_id)
            {
                return Err(RunError::unavailable("injected preparation failure"));
            }
            let provider_prepared_effect_fence = effect
                .sandbox_effect_fence()
                .map_err(|error| RunError::internal(error.to_string()))?;
            crate::SessionCleanupPreparation::try_new(
                &effect,
                provider_prepared_effect_fence,
                Vec::new(),
            )
            .map_err(|error| RunError::internal(error.to_string()))
        }

        async fn acknowledge_terminal_cleanup_preparation(
            &self,
            effect: &SessionTerminalCleanupEffect,
        ) {
            self.events
                .lock()
                .unwrap()
                .push(format!("ack:{}", effect.command.thread_id));
        }

        async fn dispose_terminal_cleanup_for_effect(
            &self,
            effect: SessionTerminalCleanupDisposalEffect,
        ) -> Result<SessionCleanupDisposalReceipt, RunError> {
            self.events.lock().unwrap().push("dispose".into());
            Ok(SessionCleanupDisposalReceipt::new(&effect.command))
        }

        async fn acknowledge_terminal_cleanup_disposal(
            &self,
            _effect: &SessionTerminalCleanupDisposalEffect,
        ) {
            self.events.lock().unwrap().push("ack-disposal".into());
        }

        async fn acknowledge_completed_terminal_cleanup(
            &self,
            _session_id: &str,
            _lease: &crate::SessionRealizationLease,
        ) {
            self.events.lock().unwrap().push("ack-completed".into());
        }
    }

    #[tokio::test]
    async fn one_driver_orders_durable_preparation_before_physical_disposal() {
        // Cause/effect graph: C1 child and root preparation are pending; C2
        // each exact receipt is accepted; C3 the aggregate then exposes its
        // one disposal. Effects: E1 install each root snapshot; E2 authorize,
        // prepare, record, then acknowledge each source effect; E3 physical
        // disposal begins only after the root preparation CAS; E4 disposal is
        // recorded before process-local retirement. Decision rule D1 is the
        // complete success path C1+C2+C3 => E1→E2→E3→E4.
        let lease = lease();
        let mut cleanup = SessionCleanupOperation::default();
        assert!(cleanup.request("session"));
        cleanup
            .freeze_targets("session", ["child".into()], 1, 2)
            .unwrap();
        let child = cleanup.command_for("session", "child").unwrap();
        let root = cleanup.command_for("session", "session").unwrap();
        let child_work = work(
            SessionTerminalCleanupAction::Prepare {
                commands: vec![child.clone()],
            },
            &lease,
        );
        cleanup
            .record_preparation(
                "session",
                preparation(&SessionTerminalCleanupEffect::new(child, lease.clone())),
                None,
            )
            .unwrap();
        let root_work = work(
            SessionTerminalCleanupAction::Prepare {
                commands: vec![root.clone()],
            },
            &lease,
        );
        cleanup
            .record_preparation(
                "session",
                preparation(&SessionTerminalCleanupEffect::new(root, lease.clone())),
                Some(
                    crate::SessionCleanupRepositoryPreparation::new(
                        "session",
                        "workspace",
                        &crate::SessionResourceState::default(),
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        let provider_disposal = cleanup
            .terminal_provider_disposal_preparation("session")
            .unwrap();
        let disposal_work = work(
            cleanup
                .terminal_work_action("session", provider_disposal.as_ref())
                .unwrap()
                .expect("disposal is ready"),
            &lease,
        );

        let events = Arc::new(Mutex::new(Vec::new()));
        let control = RecordingControl {
            work: Mutex::new(VecDeque::from([child_work, root_work, disposal_work])),
            publication: Mutex::new(None),
            events: Arc::clone(&events),
        };
        let runtime = RecordingRuntime {
            events: Arc::clone(&events),
            failing_thread: None,
            publication_effect: None,
        };
        assert_eq!(
            drive_session_terminal_cleanup("session", &lease, &control, &runtime)
                .await
                .unwrap(),
            SessionTerminalCleanupDriveOutcome::Completed
        );
        assert_eq!(
            *events.lock().unwrap(),
            [
                "poll",
                "install",
                "authorize:child",
                "prepare:child",
                "record:child",
                "ack:child",
                "poll",
                "install",
                "authorize:session",
                "prepare:session",
                "record:session",
                "ack:session",
                "poll",
                "install",
                "authorize-disposal",
                "dispose",
                "record-disposal",
                "ack-disposal",
                "ack-completed",
            ],
            "D1/E1-E4"
        );
    }

    #[tokio::test]
    async fn preparation_batch_attempts_every_frozen_child_before_retry() {
        // Cause/effect graph: C1 one child preparation fails; C2 a sibling is
        // independently valid. Effects: E1 no physical disposal; E2 attempt
        // the sibling and durably record its preparation; E3 return the first
        // effect error so the existing retry driver re-polls the aggregate.
        // Decision rule F1=C1+C2 => E1+E2+E3.
        let lease = lease();
        let failing = SessionCleanupCommand::new("session", "failing", "root-effect");
        let succeeding = SessionCleanupCommand::new("session", "succeeding", "root-effect");
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = RecordingControl {
            work: Mutex::new(VecDeque::from([work(
                SessionTerminalCleanupAction::Prepare {
                    commands: vec![failing, succeeding],
                },
                &lease,
            )])),
            publication: Mutex::new(None),
            events: Arc::clone(&events),
        };
        let runtime = RecordingRuntime {
            events: Arc::clone(&events),
            failing_thread: Some("failing".into()),
            publication_effect: None,
        };
        assert!(matches!(
            drive_session_terminal_cleanup("session", &lease, &control, &runtime).await,
            Err(SessionRealizationDriveError::Effect(_))
        ));
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| event == "prepare:failing"), "F1");
        assert!(
            events.iter().any(|event| event == "record:succeeding"),
            "F1/E2"
        );
        assert!(!events.iter().any(|event| event == "dispose"), "F1/E1");
    }

    #[tokio::test]
    async fn publication_uses_current_generation_and_admits_both_terminal_outcomes() {
        // Cause/effect graph: C1 the Waiting projection carries a monotonic
        // same-generation lease renewal; C2 runtime returns either Published
        // or permanent Rejected evidence. Effects: E1 execute under the
        // projection's renewed lease, never the older assignment lease; E2
        // admit only the matching receipt/rejection through Control; E3 re-poll
        // after durable admission and retire the completed assignment.
        // Decision rules: P1=C1+Published => E1+receipt/E2+E3;
        // P2=C1+Rejected => E1+rejection/E2+E3.
        let assigned_lease = lease();
        let mut current_lease = assigned_lease.clone();
        current_lease.expires_at_unix_ms = 20_000;
        let command = publication_command();
        let receipt = crate::SessionRepositoryPublicationReceipt::new(
            &command,
            awaken_provisioning_contract::RepositoryPublicationReceipt {
                repository_id: "repository".into(),
                source_remote_url: "https://git.invalid/repository.git".into(),
                branch: "awf/publication".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        );
        let rejection = crate::SessionRepositoryPublicationRejection::new(
            &command,
            awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
        )
        .unwrap();

        for (rule, effect, admission) in [
            (
                "P1",
                SessionRepositoryPublicationEffect::Published(receipt),
                "record-publication-receipt:3:20000",
            ),
            (
                "P2",
                SessionRepositoryPublicationEffect::Rejected(rejection),
                "record-publication-rejection:3:20000",
            ),
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let control = RecordingControl {
                work: Mutex::new(VecDeque::from([work(
                    SessionTerminalCleanupAction::Waiting,
                    &assigned_lease,
                )])),
                publication: Mutex::new(Some(
                    crate::SessionRepositoryPublicationProjection::try_new(
                        "workspace".into(),
                        command.clone(),
                        current_lease.clone(),
                    )
                    .unwrap(),
                )),
                events: Arc::clone(&events),
            };
            let runtime = RecordingRuntime {
                events: Arc::clone(&events),
                failing_thread: None,
                publication_effect: Some(effect),
            };

            assert_eq!(
                drive_session_terminal_cleanup("session", &assigned_lease, &control, &runtime,)
                    .await
                    .unwrap(),
                SessionTerminalCleanupDriveOutcome::Completed,
                "{rule}/E3"
            );
            assert_eq!(
                *events.lock().unwrap(),
                [
                    "poll",
                    "install",
                    "project-publication",
                    "publish:3:20000",
                    admission,
                    "poll",
                    "ack-completed",
                ],
                "{rule}/E1-E3"
            );
        }
    }

    #[tokio::test]
    async fn publication_rejects_foreign_root_or_stale_generation_before_effect() {
        // Cause/effect graph: C1 the projection targets another Session root;
        // C2 its current lease differs from the admitted assignment by owner,
        // runtime incarnation, or epoch, or regresses its expiry. Effect E1
        // fail closed before Repository runtime execution and before either
        // Control admission. Decision rule F1=any(C1,C2) => E1.
        let assigned_lease = lease();
        let mut owner = assigned_lease.clone();
        owner.owner = "other-worker".into();
        owner.expires_at_unix_ms = 20_000;
        let mut incarnation = assigned_lease.clone();
        incarnation.runtime_incarnation = "worker/next-boot".into();
        incarnation.expires_at_unix_ms = 20_000;
        let mut epoch = assigned_lease.clone();
        epoch.epoch += 1;
        epoch.expires_at_unix_ms = 20_000;
        let mut regressed = assigned_lease.clone();
        regressed.expires_at_unix_ms -= 1;
        let command = publication_command();
        let mut foreign_command = command.clone();
        foreign_command.session_id = "foreign-session".into();
        let mut renewed = assigned_lease.clone();
        renewed.expires_at_unix_ms = 20_000;

        for (rule, current_lease, projected_command) in [
            ("foreign-root", renewed, foreign_command),
            ("owner", owner, command.clone()),
            ("incarnation", incarnation, command.clone()),
            ("epoch", epoch, command.clone()),
            ("expiry", regressed, command.clone()),
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let control = RecordingControl {
                work: Mutex::new(VecDeque::from([work(
                    SessionTerminalCleanupAction::Waiting,
                    &assigned_lease,
                )])),
                publication: Mutex::new(Some(
                    crate::SessionRepositoryPublicationProjection::try_new(
                        "workspace".into(),
                        projected_command,
                        current_lease,
                    )
                    .unwrap(),
                )),
                events: Arc::clone(&events),
            };
            let runtime = RecordingRuntime {
                events: Arc::clone(&events),
                failing_thread: None,
                publication_effect: None,
            };

            assert!(
                matches!(
                    drive_session_terminal_cleanup("session", &assigned_lease, &control, &runtime,)
                        .await,
                    Err(SessionRealizationDriveError::Control(
                        SessionRealizationControlFailure::Invalid(_)
                    ))
                ),
                "F1/{rule}"
            );
            assert_eq!(
                *events.lock().unwrap(),
                ["poll", "install", "project-publication"],
                "F1/{rule}/E1"
            );
        }
    }
}
