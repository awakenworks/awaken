//! One topology-neutral driver for terminal preparation and physical disposal.
//!
//! The Session aggregate remains the only queue and phase owner. This module
//! merely executes the closed action projected by that aggregate, then returns
//! exact evidence through `SessionRealizationControl`; it persists no cursor,
//! retry record, receipt, or topology-specific state.

use awaken_session_error_contract::RunError;
use awaken_session_realization_contract::{
    SessionRealizationControlFailure, SessionRealizationDriveError, SessionTerminalCleanupControl,
    SessionTerminalCleanupWork, realization_lease_generation_authorizes,
};
use awaken_session_state_contract::{
    SessionRepositoryPublicationEffect, SessionTerminalCleanupAction,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect,
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

async fn next_work<R: awaken_session_execution_contract::RealizationRuntime + ?Sized>(
    control: &dyn SessionTerminalCleanupControl,
    runtime: &R,
    session_id: &awaken_session_types::SessionId,
    lease: &awaken_session_resource_contract::SessionRealizationLease,
) -> Result<
    Option<(
        SessionTerminalCleanupWork,
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
    )>,
    SessionRealizationDriveError,
> {
    let started = runtime.observe_realization_request()?;
    match control.terminal_cleanup_work(session_id, lease).await {
        Ok(work) => Ok(work.map(|work| (work, started))),
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
pub async fn drive_session_terminal_cleanup<R>(
    session_id: &awaken_session_types::SessionId,
    asserted_lease: &awaken_session_resource_contract::SessionRealizationLease,
    control: &dyn SessionTerminalCleanupControl,
    runtime: &R,
) -> Result<SessionTerminalCleanupDriveOutcome, SessionRealizationDriveError>
where
    R: awaken_session_execution_contract::RealizationRuntime
        + awaken_session_execution_contract::TerminalCleanupRuntime
        + ?Sized,
{
    let mut lease = asserted_lease.clone();
    'drive: for _ in 0..8 {
        let Some((work, started)) = next_work(control, runtime, session_id, &lease).await? else {
            runtime
                .acknowledge_completed_terminal_cleanup(session_id, &lease)
                .await;
            return Ok(SessionTerminalCleanupDriveOutcome::Completed);
        };
        if work.assignment.session_id != session_id {
            return Err(invalid_projection(
                "terminal cleanup projection changed its Session root",
            ));
        }
        runtime
            .install_terminal_cleanup_assignment(
                &work.assignment,
                work.assignment.authority.clone().observed(started),
            )
            .await?;
        lease = work.assignment.authority.lease.clone();
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
                            // Another exact driver may have committed this
                            // command after `next_work` returned it. Re-read the
                            // aggregate instead of treating that stale projection
                            // as corruption or executing its physical effect.
                            Err(SessionRealizationControlFailure::NotReady) => continue 'drive,
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
                        // The command was valid when projected, but another
                        // exact owner advanced the aggregate before this
                        // authorization. Only a fresh root may select the next
                        // effect.
                        Err(SessionRealizationControlFailure::NotReady) => continue 'drive,
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
    }
    match next_work(control, runtime, session_id, &lease).await? {
        None => {
            runtime
                .acknowledge_completed_terminal_cleanup(session_id, &lease)
                .await;
            Ok(SessionTerminalCleanupDriveOutcome::Completed)
        }
        Some(_) => Err(SessionRealizationDriveError::DidNotConverge),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_session_application_contract::StepOutcome;
    use awaken_session_realization_contract::{
        FrozenSessionProjection, SessionTerminalCleanupAssignment, SessionTerminalCleanupWork,
    };
    use awaken_session_execution_contract::SessionRuntime;
    use awaken_session_execution_contract::ToolPermissionDecision;
    use awaken_session_state_contract::{
        SessionCleanupCommand, SessionCleanupDisposalReceipt, SessionCleanupOperation,
    };
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn lease() -> awaken_session_resource_contract::SessionRealizationLease {
        awaken_session_resource_contract::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 3,
            expires_at_unix_ms: 10_000,
        }
    }

    fn publication_command() -> awaken_session_state_contract::SessionRepositoryPublicationCommand {
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

    fn projection() -> FrozenSessionProjection {
        let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                    environment_id: "environment".into(),
                    revision: awaken_environment_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint:
                        awaken_session_configuration_contract::EnvironmentFingerprint(
                            "environment-v1".into(),
                        ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network:
                        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization:
                        awaken_credential_contract::CredentialRealizationProfile {
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
                runtime_placement:
                    awaken_session_configuration_contract::SessionRuntimePlacement::Local,
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
            },
        );
        FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_types::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            previous_resource_manifest: Some(
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace",
                    0,
                    Default::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    fn work(
        action: SessionTerminalCleanupAction,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> SessionTerminalCleanupWork {
        SessionTerminalCleanupWork {
            assignment: SessionTerminalCleanupAssignment {
                session_id: "session".into(),
                projection: projection(),
                authority:
                    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                        lease.clone(),
                        0,
                    )
                    .unwrap(),
            },
            action,
        }
    }

    struct RecordingControl {
        work: Mutex<VecDeque<SessionTerminalCleanupWork>>,
        publication: Mutex<
            Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        >,
        events: Arc<Mutex<Vec<String>>>,
        stale_authorization_once: AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionTerminalCleanupQueue for RecordingControl {
        async fn claim_next_terminal_cleanup(
            &self,
            _target: awaken_session_realization_contract::SessionRealizationTarget,
        ) -> Result<
            Option<awaken_session_realization_contract::SessionTerminalCleanupAssignment>,
            SessionRealizationControlFailure,
        > {
            Ok(None)
        }

        async fn terminal_cleanup_work(
            &self,
            _session_id: &awaken_session_types::SessionId,
            _lease: &awaken_session_resource_contract::SessionRealizationLease,
        ) -> Result<Option<SessionTerminalCleanupWork>, SessionRealizationControlFailure> {
            self.events.lock().unwrap().push("poll".into());
            Ok(self.work.lock().unwrap().pop_front())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionTerminalPreparationControl for RecordingControl {
        async fn authorize_terminal_cleanup_effect(
            &self,
            effect: &SessionTerminalCleanupEffect,
        ) -> Result<
            awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
            SessionRealizationControlFailure,
        > {
            self.events
                .lock()
                .unwrap()
                .push(format!("authorize:{}", effect.command.thread_id));
            if self.stale_authorization_once.swap(false, Ordering::SeqCst) {
                return Err(SessionRealizationControlFailure::NotReady);
            }
            awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                "workspace".into(),
                None,
            )
        }

        async fn record_terminal_cleanup_preparation(
            &self,
            _lease: &awaken_session_resource_contract::SessionRealizationLease,
            preparation: awaken_session_state_contract::SessionCleanupPreparation,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events
                .lock()
                .unwrap()
                .push(format!("record:{}", preparation.effect.command.thread_id));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionTerminalDisposalControl for RecordingControl {
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
            _lease: &awaken_session_resource_contract::SessionRealizationLease,
            _receipt: SessionCleanupDisposalReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push("record-disposal".into());
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
        for RecordingControl
    {
        async fn terminal_repository_publication_command(
            &self,
            _session_id: &awaken_session_types::SessionId,
            _lease: &awaken_session_resource_contract::SessionRealizationLease,
        ) -> Result<
            Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
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
            _session_id: &awaken_session_types::SessionId,
            lease: &awaken_session_resource_contract::SessionRealizationLease,
            _receipt: awaken_session_state_contract::SessionRepositoryPublicationReceipt,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push(format!(
                "record-publication-receipt:{}:{}",
                lease.epoch, lease.expires_at_unix_ms
            ));
            Ok(())
        }

        async fn record_terminal_repository_publication_rejection(
            &self,
            _session_id: &awaken_session_types::SessionId,
            lease: &awaken_session_resource_contract::SessionRealizationLease,
            _rejection: awaken_session_state_contract::SessionRepositoryPublicationRejection,
        ) -> Result<(), SessionRealizationControlFailure> {
            self.events.lock().unwrap().push(format!(
                "record-publication-rejection:{}:{}",
                lease.epoch, lease.expires_at_unix_ms
            ));
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

        fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
            Some(self)
        }

        fn terminal_cleanup(
            &self,
        ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
            Some(self)
        }

        fn coordination(
            &self,
        ) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
            Some(self)
        }

        fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
            Some(self)
        }

        fn streaming_runs(
            &self,
        ) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
            Some(self)
        }

        fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
            Some(self)
        }

        fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
            Some(self)
        }

        fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
            Some(self)
        }

        fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
            Some(self)
        }

        fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
            Some(self)
        }
    }

    awaken_session_execution_contract::neutral_role!(CoordinationRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(OutcomeRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingRuntime);

    awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingRuntime);

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::RealizationRuntime for RecordingRuntime {
        fn observe_realization_request(
            &self,
        ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError>
        {
            // The ordering fixture uses a fixed model clock; elapsed-time
            // decisions and real OS sampling have separate executable tests.
            Ok(
                awaken_runtime_contract::authority_lease::AuthorityProofStarted::from_clock_sample(
                    std::time::Instant::now(),
                    std::time::Duration::ZERO,
                ),
            )
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Reconcile the existing Runtime projections through the given Control
        /// owner. Local and Worker deployment use one slot-based selection and
        /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
        async fn renew_session_realization_projections(
            &self,
            control: &dyn awaken_session_realization_contract::SessionRealizationControl,
            owner: &str,
            runtime_incarnation: &str,
            timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
        ) -> Result<usize, awaken_session_error_contract::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Validate the exact provider-effective Sandbox layout without publishing
        /// process-local state or performing provider, cache, Skill, File, Vault,
        /// Repository, or Git I/O. Session creation and Resource mutation call this
        /// before their durable/effect boundaries; Runtime repeats it at the final
        /// realization edge as defense in depth.
        fn validate_session_sandbox_layout(
            &self,
            thread: &str,
            layout: &awaken_session_configuration_contract::SessionSandboxLayout,
        ) -> Result<(), awaken_session_error_contract::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .validate_session_sandbox_layout(thread, layout)
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Install one complete Control-frozen Session projection. Dispatch and
        /// realization are explicit modes of this single port; callers must never
        /// lower publication, baseline, request context, Environment, or lease
        /// fields independently.
        async fn install_session_projection(
            &self,
            thread: &str,
            projection: awaken_session_realization_contract::FrozenSessionProjection,
            mode: awaken_session_realization_contract::SessionProjectionInstallMode,
        ) -> Result<(), awaken_session_error_contract::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .install_session_projection(thread, projection, mode)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
        /// an already-installed realization fence. This is the only lease-only
        /// Runtime mutation; it deliberately carries no projection or phase action.
        async fn renew_session_realization_lease(
            &self,
            thread: &str,
            lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
        ) -> Result<(), awaken_session_error_contract::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .renew_session_realization_lease(thread, lease)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Forget one failed realization fence from disposable Runtime state. The
        /// aggregate remains the sole lease authority: this hook only prevents a
        /// later retry from authorizing effects with a lease that Control has
        /// already failed or may have failed before losing its response.
        async fn invalidate_session_realization_lease(
            &self,
            thread: &str,
            lease: &awaken_session_resource_contract::SessionRealizationLease,
        ) -> Result<(), awaken_session_error_contract::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .invalidate_session_realization_lease(thread, lease)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Install the durable callback used at the exact sandbox creation boundary.
        fn install_environment_binding_sink(
            &self,
            sink: std::sync::Arc<
                dyn awaken_session_execution_contract::SessionEnvironmentBindingSink,
            >,
        ) {
            awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::TerminalCleanupRuntime for RecordingRuntime {
        async fn execute_terminal_repository_publication_for_lease(
            &self,
            _command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
            lease: &awaken_session_resource_contract::SessionRealizationLease,
        ) -> Result<
            awaken_session_state_contract::SessionRepositoryPublicationEffect,
            awaken_session_error_contract::RunError,
        > {
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
            authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
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
            awaken_session_state_contract::SessionCleanupPreparation::try_new(
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
            _session_id: &awaken_session_types::SessionId,
            _lease: &awaken_session_resource_contract::SessionRealizationLease,
        ) {
            self.events.lock().unwrap().push("ack-completed".into());
        }

        async fn install_terminal_cleanup_assignment(
            &self,
            _assignment: &SessionTerminalCleanupAssignment,
            _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
        ) -> Result<(), RunError> {
            self.events.lock().unwrap().push("install".into());
            Ok(())
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
        assert!(cleanup.request(&awaken_session_types::SessionId::from("session")));
        cleanup
            .freeze_targets(
                &awaken_session_types::SessionId::from("session"),
                ["child".into()],
                1,
                2,
            )
            .unwrap();
        let child = cleanup
            .command_for(&awaken_session_types::SessionId::from("session"), "child")
            .unwrap();
        let root = cleanup
            .command_for(&awaken_session_types::SessionId::from("session"), "session")
            .unwrap();
        let child_work = work(
            SessionTerminalCleanupAction::Prepare {
                commands: vec![child.clone()],
            },
            &lease,
        );
        let root_work = work(
            SessionTerminalCleanupAction::Prepare {
                commands: vec![root.clone()],
            },
            &lease,
        );
        // The aggregate law that the last preparation receipt projects this
        // one disposal is pinned beside `SessionCleanupOperation`; this driver
        // test starts from that projected closed action.
        let root_effect = SessionTerminalCleanupEffect::new(root, lease.clone());
        let disposal_work = work(
            SessionTerminalCleanupAction::Dispose {
                command: awaken_session_state_contract::SessionCleanupDisposalCommand {
                    session_id: "session".into(),
                    effect_id: root_effect.command.effect_id.clone(),
                    preparation_fingerprint: "root-preparation".into(),
                    provider_disposal:
                        awaken_provisioning_contract::SandboxDisposalPreparation::new(
                            root_effect.sandbox_effect_fence().unwrap(),
                            "root-preparation",
                        )
                        .unwrap(),
                    restore_target: None,
                },
            },
            &lease,
        );

        let events = Arc::new(Mutex::new(Vec::new()));
        let control = RecordingControl {
            work: Mutex::new(VecDeque::from([child_work, root_work, disposal_work])),
            publication: Mutex::new(None),
            events: Arc::clone(&events),
            stale_authorization_once: AtomicBool::new(false),
        };
        let runtime = RecordingRuntime {
            events: Arc::clone(&events),
            failing_thread: None,
            publication_effect: None,
        };
        assert_eq!(
            drive_session_terminal_cleanup(
                &awaken_session_types::SessionId::from("session"),
                &lease,
                &control,
                &runtime
            )
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
    async fn stale_authorization_reobserves_without_executing_the_old_command() {
        // Cause/effect graph: C1 an exact cleanup command is projected; C2 a
        // concurrent driver advances the aggregate before authorization, so
        // Control reports NotReady; C3 a fresh poll returns the next command.
        // Effects: E1 the stale command has no physical effect or receipt; E2
        // the driver re-reads the sole aggregate queue; E3 only the fresh
        // command executes and completion is acknowledged. Decision table:
        // R1=C1+!C2 => ordinary authorize/prepare/record; R2=C1+C2+C3 =>
        // reobserve(E2), suppress stale effect(E1), execute fresh(E3).
        let lease = lease();
        let stale = SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session"),
            "stale",
            "root-effect",
        );
        let fresh = SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session"),
            "fresh",
            "root-effect",
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = RecordingControl {
            work: Mutex::new(VecDeque::from([
                work(
                    SessionTerminalCleanupAction::Prepare {
                        commands: vec![stale],
                    },
                    &lease,
                ),
                work(
                    SessionTerminalCleanupAction::Prepare {
                        commands: vec![fresh],
                    },
                    &lease,
                ),
            ])),
            publication: Mutex::new(None),
            events: Arc::clone(&events),
            stale_authorization_once: AtomicBool::new(true),
        };
        let runtime = RecordingRuntime {
            events: Arc::clone(&events),
            failing_thread: None,
            publication_effect: None,
        };

        assert_eq!(
            drive_session_terminal_cleanup(
                &awaken_session_types::SessionId::from("session"),
                &lease,
                &control,
                &runtime
            )
            .await
            .unwrap(),
            SessionTerminalCleanupDriveOutcome::Completed,
            "R2/E2+E3"
        );
        assert_eq!(
            *events.lock().unwrap(),
            [
                "poll",
                "install",
                "authorize:stale",
                "poll",
                "install",
                "authorize:fresh",
                "prepare:fresh",
                "record:fresh",
                "ack:fresh",
                "poll",
                "ack-completed",
            ],
            "R2/E1-E3"
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
        let failing = SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session"),
            "failing",
            "root-effect",
        );
        let succeeding = SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session"),
            "succeeding",
            "root-effect",
        );
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
            stale_authorization_once: AtomicBool::new(false),
        };
        let runtime = RecordingRuntime {
            events: Arc::clone(&events),
            failing_thread: Some("failing".into()),
            publication_effect: None,
        };
        assert!(matches!(
            drive_session_terminal_cleanup(
                &awaken_session_types::SessionId::from("session"),
                &lease,
                &control,
                &runtime
            )
            .await,
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
        let receipt = awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
            &command,
            awaken_provisioning_contract::RepositoryPublicationReceipt {
                repository_id: "repository".into(),
                source_remote_url: "https://git.invalid/repository.git".into(),
                branch: "awf/publication".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        );
        let rejection = awaken_session_state_contract::SessionRepositoryPublicationRejection::new(
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
                    awaken_session_realization_contract::SessionRepositoryPublicationProjection::try_new(
                        "workspace".into(),
                        command.clone(),
                        current_lease.clone(),
                    )
                    .unwrap(),
                )),
                events: Arc::clone(&events),
                stale_authorization_once: AtomicBool::new(false),
            };
            let runtime = RecordingRuntime {
                events: Arc::clone(&events),
                failing_thread: None,
                publication_effect: Some(effect),
            };

            assert_eq!(
                drive_session_terminal_cleanup(
                    &awaken_session_types::SessionId::from("session"),
                    &assigned_lease,
                    &control,
                    &runtime,
                )
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
                    awaken_session_realization_contract::SessionRepositoryPublicationProjection::try_new(
                        "workspace".into(),
                        projected_command,
                        current_lease,
                    )
                    .unwrap(),
                )),
                events: Arc::clone(&events),
                stale_authorization_once: AtomicBool::new(false),
            };
            let runtime = RecordingRuntime {
                events: Arc::clone(&events),
                failing_thread: None,
                publication_effect: None,
            };

            assert!(
                matches!(
                    drive_session_terminal_cleanup(
                        &awaken_session_types::SessionId::from("session"),
                        &assigned_lease,
                        &control,
                        &runtime,
                    )
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
