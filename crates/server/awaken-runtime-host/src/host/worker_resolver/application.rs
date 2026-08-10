//! Application-owned frozen Session contribution and refresh.

use super::*;

fn application_provisioning_error(
    run_id: &str,
    error: awaken_session_contract::ApplicationSessionProvisionError,
) -> awaken_run_ingress::Error {
    let detail = format!("run {run_id} application provisioning failed: {error}");
    if error.is_terminal() {
        HostWorkerResolver::terminal_resolution_error(detail)
    } else {
        HostWorkerResolver::execution_error(detail)
    }
}

async fn refresh_frozen_application_material(
    claimed: &awaken_run_ingress::Claimed,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    provisioner: &dyn awaken_session_contract::ApplicationSessionProvisioner,
    ownership: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
) -> Result<(), awaken_run_ingress::Error> {
    provisioner
        .refresh_frozen(&claimed.request.activation, &thread_id.0, ownership.clone())
        .await
        .map_err(|error| application_provisioning_error(&claimed.lease.run_id.0, error))?;
    ownership.verify_current().await.map_err(|error| {
        HostWorkerResolver::execution_error(format!(
            "run {} lost ownership during frozen application material refresh: {error}",
            claimed.lease.run_id.0
        ))
    })
}

/// Install the frozen Session projection under the authenticated Run claim.
/// Application contribution is one optional branch; ordinary registered-Worker
/// Sessions use this same realization owner and phase driver.
pub(super) async fn install_claimed_session_projection(
    host: &SharedHost,
    claimed: &awaken_run_ingress::Claimed,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    dispatched_resources: Option<&awaken_session_contract::SessionResourceManifest>,
) -> Result<(), awaken_run_ingress::Error> {
    // `RunDispatch::session_thread_id` is the contract-owned discriminator:
    // ordinary Runs omit it, while Session Runs name the realization owner.
    // A registered Worker must not turn an ordinary durable thread into a
    // Coordinator Session lookup merely because the client is installed.
    if claimed.request.session_thread_id.is_none() {
        if let Some(manifest) = dispatched_resources {
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        }
        return Ok(());
    }
    let Some(control) = host.application_session_control.as_ref() else {
        if host.application_session_provisioner.is_some() {
            return Err(HostWorkerResolver::execution_error(
                "application Session provisioner has no Control contribution client",
            ));
        }
        // The co-located AllInOne pool already used the same in-process Session
        // realization driver before enqueue. Only a registered Worker installs
        // the outbound control client.
        if let Some(manifest) = dispatched_resources {
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        }
        return Ok(());
    };

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
            "run {} lost ownership before application provisioning: {error}",
            claimed.lease.run_id.0
        ))
    })?;
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
    // A delegated Run carries its parent's Session identity separately from its
    // own execution Thread and Agent snapshot. That child snapshot must never be
    // used to realize the parent Session; the synchronizer resolves the parent's
    // frozen Agent publication from Control instead.
    let session_publication =
        (claimed.request.thread_id() == thread_id).then_some(&claimed.request.activation.snapshot);

    if let Some(directive) = control
        .resume_frozen(&claim, &thread_id.0)
        .await
        .map_err(|error| {
            HostWorkerResolver::execution_error(format!(
                "run {} frozen application Session resume failed: {error}",
                claimed.lease.run_id.0
            ))
        })?
    {
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
        let has_application_material = directive.projection.baseline.application.is_some();
        if has_application_material {
            let provisioner = host
                .application_session_provisioner
                .as_ref()
                .ok_or_else(|| {
                    HostWorkerResolver::execution_error(
                        "application Session resume has no application provisioner",
                    )
                })?;
            refresh_frozen_application_material(
                claimed,
                thread_id,
                provisioner.as_ref(),
                ownership.clone(),
            )
            .await?;
        }
        HostWorkerResolver::realize_application_session(
            host,
            control.as_ref(),
            &thread_id.0,
            directive,
            Some(&claim),
            session_publication,
            claimed.request.placement.recovery
                == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await?;
        // Environment adoption or reconstruction can outlive attempt-scoped
        // application credentials. Reissue them after realization and before
        // the claimed Run can execute against the frozen Session.
        if has_application_material {
            let provisioner = host
                .application_session_provisioner
                .as_ref()
                .expect("application material was already checked above");
            refresh_frozen_application_material(
                claimed,
                thread_id,
                provisioner.as_ref(),
                ownership.clone(),
            )
            .await?;
        }
    } else {
        let provisioner = host
            .application_session_provisioner
            .as_ref()
            .ok_or_else(|| {
                HostWorkerResolver::execution_error(
                    "preparing application Session has no application provisioner",
                )
            })?;
        let contribution = provisioner
            .prepare(&claimed.request.activation, &thread_id.0, ownership.clone())
            .await
            .map_err(|error| application_provisioning_error(&claimed.lease.run_id.0, error))?;
        ownership.verify_current().await.map_err(|error| {
            HostWorkerResolver::execution_error(format!(
                "run {} lost ownership during application provisioning: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        let receipt = control
            .contribute(&claim, contribution)
            .await
            .map_err(|error| {
                HostWorkerResolver::execution_error(format!(
                    "run {} application contribution failed: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
        ownership.verify_current().await.map_err(|error| {
            HostWorkerResolver::execution_error(format!(
                "run {} lost ownership during application contribution: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        if let Some(dispatched) = dispatched_resources
            && (dispatched.workspace_id != receipt.contribution.projection.workspace_id
                || dispatched.resources != receipt.contribution.projection.resources)
        {
            return Err(HostWorkerResolver::execution_error(
                "Control contribution projection conflicts with the claimed resource snapshot",
            ));
        }
        let has_application_material = receipt
            .contribution
            .projection
            .baseline
            .application
            .is_some();
        HostWorkerResolver::realize_application_session(
            host,
            control.as_ref(),
            &thread_id.0,
            receipt.realization,
            Some(&claim),
            session_publication,
            claimed.request.placement.recovery
                == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await?;
        // Initial Environment construction is allowed to be slow. Refresh the
        // already-frozen application material only after it completes so the
        // Run never starts with credentials aged during image realization.
        if has_application_material {
            refresh_frozen_application_material(
                claimed,
                thread_id,
                provisioner.as_ref(),
                ownership.clone(),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absorbing_application_provisioning_failures_terminalize_the_claim() {
        // Cause/effect decision table: A1 transient ownership/dependency failure
        // (Retryable) -> return execution error and preserve the durable claim
        // for retry; A2 deterministic frozen-input rejection (Terminal) -> mark
        // terminal resolution so ingress cannot reprovision it forever.
        let retryable = application_provisioning_error(
            "run-retryable",
            awaken_session_contract::ApplicationSessionProvisionError::retryable(
                "transport unavailable",
            ),
        );
        let terminal = application_provisioning_error(
            "run-terminal",
            awaken_session_contract::ApplicationSessionProvisionError::terminal(
                "frozen request rejected",
            ),
        );

        assert!(!retryable.is_terminal_resolution());
        assert!(terminal.is_terminal_resolution());
    }
}
