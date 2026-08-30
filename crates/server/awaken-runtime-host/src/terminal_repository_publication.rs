//! Runtime execution of the one aggregate-derived terminal Repository effect.

use awaken_session_contract::{
    RunError, SessionRealizationLease, SessionRepositoryPublicationCommand,
    SessionRepositoryPublicationEffect, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection,
};

use crate::{ManagedHost, managed_adapter_error::to_run_error};

impl ManagedHost {
    /// Execute one aggregate-derived terminal Repository publication without
    /// installing another Resource generation. The command's `ResolvedInput`
    /// is compiled through the ordinary staging projection. Local and remote
    /// topology both carry the same aggregate lease to the Repository transport
    /// boundary; only the transport implementation differs.
    pub(crate) async fn publish_terminal_repository(
        &self,
        command: SessionRepositoryPublicationCommand,
        lease: &SessionRealizationLease,
    ) -> Result<SessionRepositoryPublicationEffect, RunError> {
        let workspace = self.host.thread_workspace(&command.session_id);

        // The canonical driver may install only the frozen aggregate after a
        // Worker restart. Hold the existing realization/lifecycle lock order
        // across exact Environment recovery and publication so root cleanup
        // cannot dispose the source between adoption and its durable receipt.
        // Lease renewal may still update the process-local slot while these
        // effect locks are held; every boundary therefore re-reads the current
        // same-generation lease rather than extending the asserted value.
        let realization_lock = self
            .host
            .session_slots
            .realization_lock(&command.session_id);
        let _realization = realization_lock.lock().await;
        let lifecycle_lock = self
            .host
            .session_slots
            .update(&command.session_id, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle_lock.lock().await;
        let mut current_lease = self
            .host
            .authorize_terminal_cleanup_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        let staged = self
            .stage_terminal_repository_publication_input(&workspace, &command, &current_lease)
            .await?;
        if staged.repositories.len() != 1
            || staged.binding_checks.len() != 1
            || !matches!(
                staged.binding_checks.first(),
                Some(crate::provisioning::ResourceBindingCheck::Repository { .. })
            )
        {
            return Err(RunError::internal(
                "terminal Repository publication did not compile one exact activation",
            ));
        }
        current_lease = self
            .host
            .authorize_terminal_cleanup_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        let (state, resolved_resources) = self
            .host
            .session_slots
            .read(&command.session_id, |slot| {
                (
                    slot.terminal_environment_state.clone(),
                    slot.resource_transition
                        .as_ref()
                        .map(|transition| transition.previous().resources.clone()),
                )
            })
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_repository_publication_projection_missing",
                    "terminal Repository publication has no installed Session projection",
                )
            })?;
        let state = state.ok_or_else(|| {
            RunError::unavailable_classified(
                "session_repository_publication_state_missing",
                "terminal Repository publication has no frozen Environment state",
            )
        })?;
        let binding = state
            .terminal_repository_publication_binding()
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_repository_publication_source_unavailable",
                    "terminal Repository publication has no live aggregate source",
                )
            })?;
        let effect_fence = current_lease
            .sandbox_effect_fence(command.effect_id.clone())
            .map_err(|error| RunError::internal(error.to_string()))?;
        let prepared = self
            .host
            .prepare_bound_environment_for_effect_under_lifecycle(
                &command.session_id,
                binding,
                &effect_fence,
                resolved_resources.as_ref(),
                crate::host::BoundEnvironmentPreparationMode::LiveSource,
            )
            .await
            .map_err(to_run_error)?;
        if !prepared.permits_live_io() {
            return Err(RunError::unavailable_classified(
                "session_repository_publication_source_not_live",
                "terminal Repository publication source is not available for live I/O",
            ));
        }
        current_lease = self
            .host
            .authorize_terminal_cleanup_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        let environment = prepared
            .environment
            .map(|(environment, _)| environment)
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_repository_publication_environment_unavailable",
                    "terminal Repository publication could not recover its exact Environment",
                )
            })?;
        let fence = (command.clone(), current_lease);
        let effect = self
            .host
            .publish_repository_activation(
                &command.session_id,
                staged.repositories.first().expect("checked"),
                &staged.binding_checks,
                environment.as_ref(),
                &command.intent.expectation,
                Some(&fence),
            )
            .await;
        self.host
            .authorize_terminal_cleanup_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        match effect {
            Ok(effect_receipt) => Ok(SessionRepositoryPublicationEffect::Published(
                SessionRepositoryPublicationReceipt::new(&command, effect_receipt),
            )),
            Err(crate::provisioning::RepositoryPublicationActivationError::Rejected(
                effect_rejection,
            )) => Ok(SessionRepositoryPublicationEffect::Rejected(
                SessionRepositoryPublicationRejection::new(&command, effect_rejection).map_err(
                    |error| {
                        RunError::internal(format!(
                            "Repository publication rejection did not match its command: {error}"
                        ))
                    },
                )?,
            )),
            Err(crate::provisioning::RepositoryPublicationActivationError::Failed(error)) => {
                Err(to_run_error(error))
            }
        }
    }
}

pub(crate) fn runtime_unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
