//! Claimed Worker installation of one already-frozen Session projection.

use super::session_realization::{
    SessionRealizationWorkerEffect, session_realization_worker_effect,
};
use super::*;

async fn acquire_session_realization_admission(
    host: &SharedHost,
    session_id: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    host.session_slots
        .realization_lock(session_id)
        .lock_owned()
        .await
}

fn map_claimed_session_control_error(
    run_id: &str,
    error: awaken_run_ingress_contract::ClaimedSessionControlError,
) -> awaken_run_ingress::Error {
    match session_realization_worker_effect(error.disposition()) {
        SessionRealizationWorkerEffect::Defer => awaken_run_ingress::Error::ResolutionNotReady(
            format!("run {run_id} is waiting for its Session Environment Work slot"),
        ),
        SessionRealizationWorkerEffect::Relinquish => HostWorkerResolver::execution_error(format!(
            "run {run_id} frozen Session resume failed: {error}"
        )),
        SessionRealizationWorkerEffect::Absorb => HostWorkerResolver::terminal_resolution_error(
            format!("run {run_id} frozen Session cannot resume: {error}"),
        ),
    }
}

/// Install the frozen Session projection under the authenticated Run claim.
/// Session authoring is complete before WorkQueue dispatch; the Worker can
/// realize committed truth but cannot contribute another desired-state input.
pub(super) async fn install_claimed_session_projection(
    host: &SharedHost,
    claimed: &awaken_run_ingress::Claimed,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    dispatched_resources: Option<&awaken_session_contract::SessionResourceManifest>,
) -> Result<(), awaken_run_ingress::Error> {
    if claimed.request.session_thread_id.is_none() {
        if let Some(manifest) = dispatched_resources {
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        }
        return Ok(());
    }
    let Some(control) = host.session_control.as_ref() else {
        if let Some(manifest) = dispatched_resources {
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        }
        return Ok(());
    };

    // Cause graph: concurrent attempts may address one Session, but only the
    // admitted Work/Run owner may advance its realization lease. Serializing
    // before the first Control mutation prevents a later entrant from fencing a
    // slow earlier effect between directive creation and installation.
    let _realization = acquire_session_realization_admission(host, &thread_id.0).await;
    let dispatch: Arc<dyn awaken_run_ingress::DispatchQueue> = host
        .dispatch_store()
        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
    let ownership = awaken_run_ingress::claim_bound_ownership_verifier(
        dispatch,
        awaken_run_ingress::RunClaim::from(&claimed.lease),
        Arc::new(awaken_run_ingress::SystemClock),
    );
    ownership.verify_current().await.map_err(|error| {
        HostWorkerResolver::execution_error(format!(
            "run {} lost ownership before Session realization: {error}",
            claimed.lease.run_id.0
        ))
    })?;
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    let directive = control
        .resume_frozen(&claim, &thread_id.0)
        .await
        .map_err(|error| map_claimed_session_control_error(&claimed.lease.run_id.0, error))?
        .ok_or_else(|| {
            HostWorkerResolver::execution_error(
                "WorkQueue dispatched a Session whose creation was not finalized",
            )
        })?;
    ownership.verify_current().await.map_err(|error| {
        HostWorkerResolver::execution_error(format!(
            "run {} lost ownership during frozen Session resume: {error}",
            claimed.lease.run_id.0
        ))
    })?;
    if let Some(dispatched) = dispatched_resources
        && (dispatched.workspace_id != directive.projection.workspace_id
            || dispatched.resources != directive.projection.resources)
    {
        return Err(HostWorkerResolver::execution_error(
            "Control resume projection conflicts with the claimed resource snapshot",
        ));
    }
    let session_publication =
        (claimed.request.thread_id() == thread_id).then_some(&claimed.request.activation.snapshot);
    HostWorkerResolver::drive_session_realization(
        host,
        control.as_ref(),
        &thread_id.0,
        directive,
        Some(&claim),
        session_publication,
        claimed.request.placement.recovery
            == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_session_control_uses_the_contract_disposition() {
        // Cause/effect graph: C1 temporary Work-slot backpressure, C2 a
        // retryable dependency failure, and C3 durable terminal Session truth.
        // Effects: E1 defer, E2 relinquish through Execution, and E3 absorb the
        // Run. In particular C3 must never enter the claim/relinquish hot loop.
        //
        // | Rule | Control cause | durable Dispatch effect |
        // | R1 | NotReady | ResolutionNotReady |
        // | R2 | stale/conflict/unavailable | Execution |
        // | R3 | not-found/retired/terminal/invalid | TerminalResolution |
        let cases = [
            (
                "R1",
                awaken_session_contract::SessionRealizationControlFailure::NotReady,
                "resolution_not_ready",
            ),
            (
                "R2 stale",
                awaken_session_contract::SessionRealizationControlFailure::StaleOwnership,
                "execution",
            ),
            (
                "R2 conflict",
                awaken_session_contract::SessionRealizationControlFailure::Conflict,
                "execution",
            ),
            (
                "R2 unavailable",
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                    "dependency".into(),
                ),
                "execution",
            ),
            (
                "R3 not found",
                awaken_session_contract::SessionRealizationControlFailure::NotFound,
                "terminal_resolution",
            ),
            (
                "R3 terminal",
                awaken_session_contract::SessionRealizationControlFailure::Terminal,
                "terminal_resolution",
            ),
            (
                "R3 retired",
                awaken_session_contract::SessionRealizationControlFailure::Retired,
                "terminal_resolution",
            ),
            (
                "R3 invalid",
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    "invalid phase".into(),
                ),
                "terminal_resolution",
            ),
        ];
        for (rule, failure, expected) in cases {
            let actual = map_claimed_session_control_error("run-a", failure.into());
            let actual = match actual {
                awaken_run_ingress::Error::ResolutionNotReady(_) => "resolution_not_ready",
                awaken_run_ingress::Error::Execution(_) => "execution",
                awaken_run_ingress::Error::TerminalResolution(_) => "terminal_resolution",
                error => panic!("{rule}: unexpected {error:?}"),
            };
            assert_eq!(actual, expected, "{rule}");
        }
    }

    #[tokio::test]
    async fn realization_admission_precedes_concurrent_control_mutation() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // Cause/effect decision table: while C1 first admission is held, C2 a
        // concurrent entrant must not perform Control mutation (E1). Releasing
        // C1 permits exactly the waiting entrant (E2).
        //
        // | Rule | First held | Second entered | Effect |
        // | R1 | yes | no | second waits |
        // | R2 | no | yes | second proceeds |
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        ));
        let first = acquire_session_realization_admission(&host, "shared-session").await;
        let second_entered = Arc::new(AtomicBool::new(false));
        let waiting = {
            let host = host.clone();
            let second_entered = second_entered.clone();
            tokio::spawn(async move {
                let _second = acquire_session_realization_admission(&host, "shared-session").await;
                second_entered.store(true, Ordering::SeqCst);
            })
        };

        tokio::task::yield_now().await;
        assert!(!second_entered.load(Ordering::SeqCst), "R1");
        drop(first);
        waiting.await.expect("second admission joins");
        assert!(second_entered.load(Ordering::SeqCst), "R2");
    }
}
