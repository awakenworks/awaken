// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! Session Work admission for one claimed Run-free Session preparation.
//!
//! A preparation the Session root hands to a Ready Worker (an Idle MCP update,
//! say) enters Environment effects under the Session's Work, as a claimed Run
//! does: every later realization request of that Worker only renews the lease.
//! The claim therefore takes the Session's dispatched Work for the claiming
//! Worker before the assignment leaves the Coordinator, and the Worker's
//! report that the preparation finished releases it again unless a Run has
//! been admitted meanwhile, as a settled root Run releases its own Work.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::session_requests::*;
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
        .claim_next_session_preparation(target.clone())
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
    let handoff_admission = if matches!(&ownership, Ok(ownership) if ownership.permits_effect_for(worker_owner))
    {
        service
            .session_preparation_control
            .validate_session_preparation_assignment(&assignment, &target)
            .await
    } else {
        Ok(false)
    };
    if matches!(&handoff_admission, Ok(true)) {
        return Ok(Some(assignment));
    }
    let retirement = service
        .session_control
        .retire_session_realization(
            awaken_session_execution_contract::RetireSessionRealization {
                session_id: assignment.session_id.clone(),
                lease: assignment.directive.authority.lease.clone(),
            },
        )
        .await;
    handoff_admission.map_err(RealizationHttpError::Control)?;
    retirement.map_err(RealizationHttpError::Control)?;
    ownership
        .map_err(|error| ApplicationError::internal(error.to_string()))
        .map_err(RealizationHttpError::from)?;
    Ok(None)
}

/// Release the Session Work a finished Run-free preparation took. The Worker
/// reports only after its local preparation drive returned (Worker-local
/// closure); the Session root then decides whether the grant it reports is
/// still the current one and no Run has been admitted; and the Work store
/// releases only Work this exact Worker incarnation still holds under the
/// preparation's claim, in one compare-and-release.
///
/// | Rule | grant owner | root decision      | Work                         | effect          |
/// |------|-------------|--------------------|------------------------------|-----------------|
/// | F1   | this Worker | release            | held by this preparation     | release, `true` |
/// | F2   | this Worker | keep (Run admitted)| any                          | nothing         |
/// | F3   | this Worker | release            | another holder               | nothing         |
/// | F4   | another     | -                  | -                            | refused         |
/// | F5   | this Worker | release            | a Run acquired it since      | nothing         |
pub(super) async fn session_preparation_finish(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<
        SessionRealizationReq<awaken_session_execution_contract::FinishSessionPreparation>,
    >,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_worker_identity(&worker, &request.identity).map_err(ApplicationError::invalid)?;
        claim_authority(&service, &worker, Some(&request.identity), false).await?;
        let owner = request.identity.lease_owner();
        if request.command.lease.owner != request.identity.worker_id
            || request.command.lease.runtime_incarnation != owner
        {
            return Err(RealizationHttpError::from(ApplicationError::invalid(
                "finished Session preparation lease is not owned by the authenticated Worker incarnation",
            )));
        }
        let released = service
            .session_preparation_work_release
            .release_finished_preparation_work(request.command, &owner)
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "released": released }))
    }
    .await;
    respond_realization(result)
}
