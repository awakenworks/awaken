//! The recording cleanup runtime's terminal-cleanup role: publication, preparation and disposal effects in order.
use super::*;

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for RecordingCleanupRuntime {
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<awaken_session_state_contract::SessionRepositoryPublicationEffect, RunError> {
        let exact = self
            .terminal_assignments
            .lock()
            .unwrap()
            .last()
            .is_some_and(|assignment| assignment.authority.lease == *lease);
        if !exact {
            return Err(RunError::internal(
                "terminal Repository effect does not match its installed assignment",
            ));
        }
        self.publication_intents
            .lock()
            .unwrap()
            .push(command.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("publication".into());
        if self.fail_publication_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable_classified(
                "repository_publication_transport_unavailable",
                "injected publication transport loss",
            ));
        }
        if self.close_publication_source.load(Ordering::SeqCst) {
            // Control conformance only: derive identity from the installed
            // assignment, while the Runtime Host integration owns the physical
            // observation. No independent source store or forged Git ref.
            let binding = self
                .terminal_assignments
                .lock()
                .unwrap()
                .last()
                .and_then(|assignment| {
                    assignment
                        .projection
                        .environment
                        .terminal_repository_publication_binding()
                })
                .map(str::to_owned)
                .ok_or_else(|| RunError::internal("closure fixture has no exact source"))?;
            let rejection = awaken_session_state_contract::SessionRepositoryPublicationRejection::source_closed(
                &command,
                &binding,
                awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: None,
                },
            ).map_err(|error| RunError::internal(error.to_string()))?;
            return Ok(
                awaken_session_state_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ),
            );
        }
        if self.reject_publication.load(Ordering::SeqCst) {
            let rejection =
                awaken_session_state_contract::SessionRepositoryPublicationRejection::new(
                    &command,
                    awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
                )
                .map_err(|error| RunError::internal(error.to_string()))?;
            return Ok(
                awaken_session_state_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ),
            );
        }
        let awaken_session_resource_contract::ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } = &command.intent.input.source
        else {
            return Err(RunError::internal(
                "test publication command did not contain a Repository",
            ));
        };
        Ok(
            awaken_session_state_contract::SessionRepositoryPublicationEffect::Published(
                awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
                    &command,
                    awaken_provisioning_contract::RepositoryPublicationReceipt {
                        repository_id: repository_id.to_string(),
                        source_remote_url: config.remote_url.clone(),
                        branch: command.intent.expectation.branch.clone(),
                        commit: command.intent.expectation.commit.clone(),
                    },
                ),
            ),
        )
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        authorization
            .verify_for(&effect)
            .map_err(|error| RunError::internal(error.to_string()))?;
        let assignment = self
            .terminal_assignments
            .lock()
            .unwrap()
            .last()
            .cloned()
            .ok_or_else(|| RunError::internal("terminal effect has no installed assignment"))?;
        if assignment.session_id != effect.command.session_id
            || assignment.authority.lease != effect.lease
        {
            return Err(RunError::internal(
                "terminal effect does not match its installed assignment",
            ));
        }
        let intent = effect.command.clone();
        if self.fail_before_effect_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-effect crash window"));
        }
        self.intents.lock().unwrap().push(intent.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push(format!("cleanup:{}", intent.thread_id));
        self.effective_ids
            .lock()
            .unwrap()
            .insert(intent.effect_id.clone());
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected cleanup crash window"));
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
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        self.acknowledged_effects
            .lock()
            .unwrap()
            .push(effect.clone());
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("dispose".into());
        Ok(awaken_session_state_contract::SessionCleanupDisposalReceipt::new(&effect.command))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) {
        self.acknowledged_completed
            .lock()
            .unwrap()
            .push((session_id.to_string(), lease.clone()));
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        self.terminal_assignments
            .lock()
            .unwrap()
            .push(assignment.clone());
        Ok(())
    }
}
