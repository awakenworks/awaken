//! Application-owned frozen Session contribution and refresh.

use super::*;

pub(super) async fn install_application_projection(
    host: &SharedHost,
    claimed: &awaken_run_ingress::Claimed,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    dispatched_resources: Option<&awaken_session_contract::SessionResourceManifest>,
) -> Result<(), awaken_run_ingress::Error> {
    let Some(provisioner) = &host.application_session_provisioner else {
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
    let control = host.application_session_control.as_ref().ok_or_else(|| {
        HostWorkerResolver::execution_error(
            "application Session provisioner has no Control contribution client",
        )
    })?;
    let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);

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
        provisioner
            .refresh_frozen(&claimed.request.activation, &thread_id.0, ownership.clone())
            .await
            .map_err(|error| {
                HostWorkerResolver::execution_error(format!(
                    "run {} frozen application material refresh failed: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
        ownership.verify_current().await.map_err(|error| {
            HostWorkerResolver::execution_error(format!(
                "run {} lost ownership during frozen application material refresh: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        HostWorkerResolver::realize_application_session(
            host,
            control,
            &thread_id.0,
            directive,
            Some(&claim),
        )
        .await?;
    } else {
        let contribution = provisioner
            .prepare(&claimed.request.activation, &thread_id.0, ownership.clone())
            .await
            .map_err(|error| {
                HostWorkerResolver::execution_error(format!(
                    "run {} application provisioning failed: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
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
        HostWorkerResolver::realize_application_session(
            host,
            control,
            &thread_id.0,
            receipt.realization,
            Some(&claim),
        )
        .await?;
    }
    Ok(())
}
