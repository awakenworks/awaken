//! The original recording Control, shared by the signed transport fixtures.
use super::*;

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRealizationControl for RecordingSessionControl {
    async fn retire_session_realization(
        &self,
        command: awaken_session_execution_contract::RetireSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationRetirement,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.retirements.lock().unwrap().push(command);
        self.retirement_outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(
                awaken_session_contract::SessionRealizationRetirement::Retired {},
            ))
    }

    async fn begin_session_realization(
        &self,
        command: awaken_session_execution_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.begins.lock().unwrap().push(command.clone());
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(
                    awaken_session_configuration_contract::resource::SessionRealizationLease {
                        owner: command.target.owner,
                        runtime_incarnation: command.target.runtime_incarnation,
                        epoch: 2,
                        expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                    },
                )?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_execution_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.renewals.lock().unwrap().push(command.clone());
        if let Some(error) = self.renewal_failure.lock().unwrap().clone() {
            return Err(error);
        }
        let mut lease = command.asserted_lease;
        lease.expires_at_unix_ms = command.requested_expires_at_unix_ms;
        self.receipt(lease)
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_execution_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        *self.activations.lock().unwrap() += 1;
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(command.lease)?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_execution_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        *self.acknowledgements.lock().unwrap() += 1;
        let projection =
            self.projection.lock().unwrap().clone().ok_or(
                awaken_session_execution_contract::SessionRealizationControlFailure::NotReady,
            )?;
        Ok(
            awaken_session_execution_contract::SessionRealizationDirective {
                projection,
                authority: self.receipt(command.lease)?,
                action: awaken_session_execution_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_execution_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        *self.failures.lock().unwrap() += 1;
        Ok(())
    }
}
