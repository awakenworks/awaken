// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: Runtime execution adapter for the aggregate-derived terminal Repository
// @formal reason: effect. Dynamic path: the required exact current realization lease gates execution, the
// @formal reason: frozen input compiles through the existing staging path, and exactly one Repository
// @formal reason: activation produces the receipt returned to the Session cleanup authority; this adapter
// @formal reason: owns no second intent or receipt store. Evidence: resource.files.resource_lifecycle and
// @formal reason: terminal Repository publication tests; this is a product-requirement classification, not
// @formal reason: a direct formal-proof claim.
//! Runtime execution of the one aggregate-derived terminal Repository effect.

use awaken_session_resource_contract::SessionRealizationLease;
use awaken_session_state_contract::{
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationReceipt, SessionRepositoryPublicationRejection,
};
use awaken_session_types::RunError;

use crate::ManagedHost;
use crate::host_error::to_run_error;

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
        let cleanup = &self.host.realizer.cleanup;
        let workspace = self.host.thread_workspace(command.session_id.as_str());

        // The canonical driver may install only the frozen aggregate after a
        // Worker restart. Hold the existing realization/lifecycle lock order
        // across exact Environment recovery and publication so root cleanup
        // cannot dispose the source between adoption and its durable receipt.
        // Lease renewal may still update the process-local slot while these
        // effect locks are held; every boundary therefore re-reads the current
        // same-generation lease rather than extending the asserted value.
        let _realization = self
            .host
            .installed_sessions
            .acquire_realization(command.session_id.as_str())
            .await;
        let _lifecycle = self
            .host
            .installed_sessions
            .lock_lifecycle(command.session_id.as_str())
            .await;
        let mut current_lease = cleanup
            .authorize_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        let staged = self
            .stage_terminal_repository_publication_input(&workspace, &command, &current_lease)
            .await?;
        if staged.repositories.len() != 1
            || staged.binding_checks.len() != 1
            || !matches!(
                staged.binding_checks.first(),
                Some(crate::staged_resources::ResourceBindingCheck::Repository { .. })
            )
        {
            return Err(RunError::internal(
                "terminal Repository publication did not compile one exact activation",
            ));
        }
        current_lease = cleanup
            .authorize_lease(&command.session_id, lease)
            .map_err(to_run_error)?;
        let (assignment, resolved_resources) = self
            .host
            .installed_sessions
            .read_realization(command.session_id.as_str(), |r| {
                (
                    r.terminal_cleanup_assignment.clone(),
                    r.previous_resources(),
                )
            })
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_repository_publication_projection_missing",
                    "terminal Repository publication has no installed Session projection",
                )
            })?;
        let assignment = assignment.ok_or_else(|| {
            RunError::unavailable_classified(
                "session_repository_publication_state_missing",
                "terminal Repository publication has no frozen Environment state",
            )
        })?;
        let state = assignment.projection.environment;
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
                command.session_id.as_str(),
                binding,
                &effect_fence,
                resolved_resources.as_ref(),
                crate::session_environment::BoundEnvironmentPreparationMode::TerminalRepositoryPublication,
            )
            .await
            .map_err(to_run_error)?;
        if let Some(observation) = prepared.publication_source_closure() {
            cleanup
                .authorize_lease(&command.session_id, lease)
                .map_err(to_run_error)?;
            let rejection = SessionRepositoryPublicationRejection::source_closed(
                &command,
                binding,
                observation.clone(),
            )
            .map_err(|error| RunError::internal(error.to_string()))?;
            return Ok(SessionRepositoryPublicationEffect::Rejected(rejection));
        }
        if !prepared.permits_live_io() {
            return Err(RunError::unavailable_classified(
                "session_repository_publication_source_not_live",
                "terminal Repository publication source is not available for live I/O",
            ));
        }
        current_lease = cleanup
            .authorize_lease(&command.session_id, lease)
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
                command.session_id.as_str(),
                staged.repositories.first().expect("checked"),
                &staged.binding_checks,
                environment.repositories().as_ref(),
                &command.intent.expectation,
                Some(&fence),
            )
            .await;
        cleanup
            .authorize_lease(&command.session_id, lease)
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

/// Test fixtures stamp lease expiries on the real clock; production reads the
/// owning Host's injected clock instead.
#[cfg(test)]
pub(crate) fn runtime_unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
