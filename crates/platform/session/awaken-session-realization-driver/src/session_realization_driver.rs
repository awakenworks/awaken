// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the topology-neutral, stateless executor of the Stage, Activate,
// @formal reason: Publish/Drain and Acknowledge actions projected by the Session aggregate; it owns no
// @formal reason: cursor, retry record, receipt store, or phase authority. Dynamic path: every staged MCP
// @formal reason: receipt, publication, drain, and effect failure is verified and recorded through
// @formal reason: SessionRealizationControl, whose returned directive is the only input to the next step,
// @formal reason: while external effect truth and eventual convergence remain executable or adapter
// @formal reason: evidence rather than a direct formal-proof claim.
//! One topology-neutral driver for the Session realization phase protocol.
//!
//! The Session aggregate (through `SessionRealizationControl`) owns every
//! durable transition; this module only sequences the Stage, Activate,
//! Publish/Drain and Acknowledge effects it projects. The directive, lease and
//! receipt algebra stay in `awaken-session-execution-contract`.

use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_execution_contract::McpAttachmentRealizer;
use awaken_session_execution_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, FailSessionRealization,
    SessionProjectionSynchronizer, SessionRealizationAction, SessionRealizationControl,
    SessionRealizationDirective, SessionRealizationDriveError, SessionRealizationEffectFailure,
    SessionRealizationPermit,
};
use awaken_session_types::SessionId;

/// Close the one repeated effect-failure path. Control owns the durable failure
/// transition; the synchronizer owns only invalidation of its disposable lease
/// projection. Both are best-effort because the original effect error remains
/// the caller-visible result, and response loss on either side must still force
/// the next attempt to reload aggregate truth.
async fn fail_realization_effect(
    session_id: &SessionId,
    source_run_id: Option<&awaken_agent_contract::agent::run::Id>,
    control: &dyn SessionRealizationControl,
    synchronizer: &dyn SessionProjectionSynchronizer,
    lease: &SessionRealizationLease,
    prepared_resource_revision: Option<u64>,
    error: SessionRealizationEffectFailure,
) -> SessionRealizationDriveError {
    let reporting = awaken_time::mono::Stopwatch::start();
    // One classification for Control and the Worker claim projection.
    let _ = control
        .fail_session_realization(FailSessionRealization::for_effect(
            session_id.clone(),
            lease.clone(),
            prepared_resource_revision,
            source_run_id.cloned(),
            error.run_error(),
        ))
        .await;
    let _ = synchronizer
        .invalidate_session_realization_lease(session_id, lease)
        .await;
    SessionRealizationDriveError::Effect(error.after_wait(reporting.elapsed()))
}

/// Canonical Stage → Activate → Publish/Drain → Acknowledge driver.
///
/// Local and remote topology adapters share this algorithm. They may differ in
/// how a frozen projection is installed and how MCP effects are transported,
/// but cannot acquire a second ordering, cleanup, receipt, or failure path.
pub async fn drive_session_realization(
    permit: &SessionRealizationPermit,
    source_run_id: Option<awaken_agent_contract::agent::run::Id>,
    control: &dyn SessionRealizationControl,
    synchronizer: &dyn SessionProjectionSynchronizer,
    mcp: &dyn McpAttachmentRealizer,
    mut directive: SessionRealizationDirective,
    mut request_started: awaken_lease_contract::authority::AuthorityProofStarted,
) -> Result<(), SessionRealizationDriveError> {
    // The Session is taken from the exclusion the caller holds, never passed
    // beside it: a permit for another Session cannot drive this one.
    let session_id = permit.session_id();
    loop {
        if matches!(directive.action, SessionRealizationAction::Released) {
            // A released predecessor is only a disposable local cache fence.
            // Do not reinstall it, re-adopt a source or call a provider.
            synchronizer
                .invalidate_session_realization_lease(session_id, &directive.authority.lease)
                .await
                .map_err(SessionRealizationDriveError::Effect)?;
            return Ok(());
        }
        let previous = directive.clone();
        let prepare_session = directive.action.prepares_session();
        match awaken_session_execution_contract::synchronize_or_supersede(
            synchronizer,
            session_id,
            source_run_id.as_ref(),
            &directive,
            request_started,
        )
        .await
        {
            Ok(false) => {}
            Ok(true) => return Ok(()),
            Err(error) => {
                if matches!(directive.action, SessionRealizationAction::Continue { .. }) {
                    // Projection failure grants no physical closure of the
                    // original source. Reload root truth on the next poll.
                    return Err(SessionRealizationDriveError::Effect(error));
                }
                return Err(fail_realization_effect(
                    session_id,
                    source_run_id.as_ref(),
                    control,
                    synchronizer,
                    &directive.authority.lease,
                    prepare_session.then_some(directive.projection.resource_revision),
                    error,
                )
                .await);
            }
        }
        #[cfg(feature = "test-support")]
        awaken_session_execution_contract::realization_observation::observe_directive(
            session_id, &directive,
        )
        .await;
        match directive.action.clone() {
            SessionRealizationAction::Continue { instruction } => {
                // An interrupted continuation keeps its root phase and grant:
                // expiring it through ordinary Stage failure would strand the
                // original-generation source before a verified checkpoint.
                let receipt = synchronizer
                    .execute_environment_continuation(
                        permit,
                        &directive.projection,
                        &directive.authority.lease,
                        instruction,
                    )
                    .await
                    .map_err(SessionRealizationDriveError::Effect)?;
                request_started = synchronizer.observe_realization_request()?;
                directive = control
                    .acknowledge_session_realization(AcknowledgeSessionRealization {
                        session_id: session_id.clone(),
                        lease: directive.authority.lease,
                        published: Vec::new(),
                        drained: Vec::new(),
                        continuation: Some(receipt),
                    })
                    .await?;
            }
            SessionRealizationAction::Released => {
                unreachable!("released directives end before installation")
            }
            SessionRealizationAction::Stage {
                prepare_session,
                mcp_stages,
            } => {
                let mut receipts = Vec::with_capacity(mcp_stages.len());
                let effect = async {
                    for request in mcp_stages {
                        let receipt = mcp.stage_mcp_attachment(request.clone()).await?;
                        receipt.verify(&request).map_err(|_| {
                            RunError::classified(
                                "mcp_receipt_mismatch",
                                "Runtime returned a receipt for another MCP realization",
                            )
                        })?;
                        receipts.push(receipt);
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    for receipt in &receipts {
                        let _ = awaken_session_execution_contract::drain_mcp_generation_receipt(
                            mcp,
                            receipt.generation.clone(),
                        )
                        .await;
                    }
                    return Err(fail_realization_effect(
                        session_id,
                        source_run_id.as_ref(),
                        control,
                        synchronizer,
                        &directive.authority.lease,
                        prepare_session.then_some(directive.projection.resource_revision),
                        error.into(),
                    )
                    .await);
                }
                #[cfg(feature = "test-support")]
                awaken_session_execution_contract::realization_observation::checkpoint("stage", session_id,
                    serde_json::json!({"lease": directive.authority.lease,
                        "generations": receipts.iter().map(|r| awaken_session_execution_contract::realization_observation::observed_generation(&r.generation)).collect::<Vec<_>>()})).await;
                request_started = synchronizer.observe_realization_request()?;
                directive = match control
                    .activate_session_realization(ActivateSessionRealization {
                        session_id: session_id.clone(),
                        lease: directive.authority.lease,
                        prepared_resource_revision: prepare_session
                            .then_some(directive.projection.resource_revision),
                        mcp_receipts: receipts.clone(),
                    })
                    .await
                {
                    Ok(next) => next,
                    Err(error) => {
                        for receipt in receipts {
                            let _ =
                                awaken_session_execution_contract::drain_mcp_generation_receipt(
                                    mcp,
                                    receipt.generation.clone(),
                                )
                                .await;
                        }
                        return Err(error.into());
                    }
                };
            }
            SessionRealizationAction::Publish { publish, drain } => {
                let effect = async {
                    for generation in &publish {
                        awaken_session_execution_contract::publish_mcp_generation_receipt(mcp, generation.clone()).await?;
                        #[cfg(feature = "test-support")]
                        awaken_session_execution_contract::realization_observation::checkpoint(
                            "publish-one",
                            session_id,
                            serde_json::json!({
                                "generation": awaken_session_execution_contract::realization_observation::observed_generation(generation)
                            }),
                        )
                        .await;
                    }
                    for generation in &drain {
                        awaken_session_execution_contract::drain_mcp_generation_receipt(mcp, generation.clone()).await?;
                        #[cfg(feature = "test-support")]
                        awaken_session_execution_contract::realization_observation::checkpoint(
                            "drain-one",
                            session_id,
                            serde_json::json!({
                                "generation": awaken_session_execution_contract::realization_observation::observed_generation(generation)
                            }),
                        )
                        .await;
                    }
                    Ok::<(), RunError>(())
                }
                .await;
                if let Err(error) = effect {
                    return Err(fail_realization_effect(
                        session_id,
                        source_run_id.as_ref(),
                        control,
                        synchronizer,
                        &directive.authority.lease,
                        None,
                        error.into(),
                    )
                    .await);
                }
                #[cfg(feature = "test-support")]
                awaken_session_execution_contract::realization_observation::checkpoint("published", session_id,
                    serde_json::json!({"lease": directive.authority.lease,
                        "published": publish.iter().map(awaken_session_execution_contract::realization_observation::observed_generation).collect::<Vec<_>>(),
                        "drained": drain.iter().map(awaken_session_execution_contract::realization_observation::observed_generation).collect::<Vec<_>>()})).await;
                request_started = synchronizer.observe_realization_request()?;
                directive = control
                    .acknowledge_session_realization(AcknowledgeSessionRealization {
                        continuation: None,
                        session_id: session_id.clone(),
                        lease: directive.authority.lease,
                        published: publish,
                        drained: drain,
                    })
                    .await?;
            }
            SessionRealizationAction::Complete => {
                #[cfg(feature = "test-support")]
                awaken_session_execution_contract::realization_observation::observe_completed(
                    session_id,
                    &directive.authority.lease,
                )
                .await;
                return Ok(());
            }
        }
        // A Control transition may complete on the final permitted action. Do
        // not require a redundant fifth loop iteration merely to observe the
        // terminal directive after a same-epoch renewal catch-up.
        if matches!(directive.action, SessionRealizationAction::Complete) {
            #[cfg(feature = "test-support")]
            awaken_session_execution_contract::realization_observation::observe_completed(
                session_id,
                &directive.authority.lease,
            )
            .await;
            return Ok(());
        }
        // A lease extension can require any number of monotonic Stage/Activate
        // catch-up rounds. The protocol therefore rejects only a Control
        // transition that made no observable progress; an arbitrary iteration
        // cap would turn a healthy, still-owned execution into a false failure.
        if directive.same_progress_as(&previous) {
            return Err(SessionRealizationDriveError::DidNotConverge);
        }
    }
}

#[cfg(test)]
mod tests;
