// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! Session Work admission for one claimed Run-free Session preparation.
//!
//! A preparation the Session root hands to a Ready Worker (an Idle MCP update,
//! say) enters Environment effects under the Session's Work, as a claimed Run
//! does: every later realization request of that Worker only renews the lease.
//! The claim therefore takes the Session's dispatched Work for the claiming
//! Worker before the assignment leaves the Coordinator.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Claim the next Run-free preparation for `worker_owner` and admit it under
/// its Session's Work. When the Work is held elsewhere or is not dispatched,
/// the just-issued grant never leaves the Coordinator: it is retired through
/// the Session root and nothing is handed out on this poll.
///
/// | Rule | Work observation          | effect                               |
/// |------|---------------------------|--------------------------------------|
/// | A1   | not required              | hand out the assignment              |
/// | A2   | leased to this Worker     | hand out the assignment              |
/// | A3   | unowned or another holder | retire the grant, hand out nothing   |
/// | A4   | unreadable                | retire the grant, report the failure |
pub(super) async fn claim_next_admitted_preparation(
    service: &WorkerDispatchService,
    target: awaken_session_execution_contract::SessionRealizationTarget,
    worker_owner: &str,
) -> Result<
    Option<awaken_session_execution_contract::SessionPreparationAssignment>,
    RealizationHttpError,
> {
    let Some(assignment) = service
        .session_preparation_control
        .claim_next_session_preparation(target)
        .await
        .map_err(RealizationHttpError::from)?
    else {
        return Ok(None);
    };
    let ownership = service
        .session_work
        .acquire_session_work(
            &assignment.session_id,
            worker_owner,
            awaken_session_application_contract::SessionWorkAcquisition::ClaimedPreparation,
        )
        .await;
    if matches!(&ownership, Ok(ownership) if ownership.permits_effect_for(worker_owner)) {
        return Ok(Some(assignment));
    }
    service
        .session_control
        .retire_session_realization(
            awaken_session_execution_contract::RetireSessionRealization {
                session_id: assignment.session_id.clone(),
                lease: assignment.directive.authority.lease.clone(),
            },
        )
        .await
        .map_err(RealizationHttpError::Control)?;
    ownership
        .map_err(|error| ApplicationError::internal(error.to_string()))
        .map_err(RealizationHttpError::from)?;
    Ok(None)
}
