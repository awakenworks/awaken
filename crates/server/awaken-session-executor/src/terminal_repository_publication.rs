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
    /// is compiled through the ordinary staging projection, while an optional
    /// Worker lease selects the same HTTP binding boundary under terminal
    /// authority. Local topology enters with no remote lease and uses the
    /// validated frozen command directly, never a second live Registry read.
    pub(crate) async fn publish_terminal_repository(
        &self,
        command: SessionRepositoryPublicationCommand,
        lease: Option<&SessionRealizationLease>,
    ) -> Result<SessionRepositoryPublicationEffect, RunError> {
        if let Some(asserted) = lease {
            let authorized = self
                .host
                .session_slots
                .read(&command.session_id, |slot| {
                    slot.realization_lease.as_ref().is_some_and(|current| {
                        awaken_session_contract::realization_lease_authorizes(
                            current,
                            asserted,
                            runtime_unix_now_ms(),
                        )
                    })
                })
                .unwrap_or(false);
            if !authorized {
                return Err(RunError::unavailable_classified(
                    "session_repository_publication_stale_lease",
                    "terminal Repository publication lost its realization lease",
                ));
            }
        }
        let workspace = self.host.thread_workspace(&command.session_id);
        let staged = self
            .stage_terminal_repository_publication_input(&workspace, &command, lease)
            .await?;
        let fence = lease.map(|lease| (command.clone(), lease.clone()));
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
        let environment = self
            .host
            .session_environment(&command.session_id)
            .await
            .ok_or_else(|| {
                RunError::unavailable_classified(
                    "session_repository_publication_environment_unavailable",
                    "terminal Repository publication has no retained Session environment",
                )
            })?;
        let effect = self
            .host
            .publish_repository_activation(
                &command.session_id,
                staged.repositories.first().expect("checked"),
                &staged.binding_checks,
                environment.as_ref(),
                &command.intent.expectation,
                fence.as_ref(),
            )
            .await;
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
