// @formal evidence assembly_role.process_role_projects_exact_startup_authority
// @formal evidence service_lifecycle.startup_wiring_fails_closed_for_unknown_missing_or_extra_authority
//! Validate runtime process dependencies before recovery, Host assembly or tasks.
//!
//! This private value carries selected ports; the canonical role and component
//! manifest retain startup authority. It opens no stores and creates no Runtime.

use crate::executable_agent_registration::ExecutableAgentWiring;
use crate::executable_environment_registration::ExecutableEnvironmentWiring;
use crate::process_startup::ProcessStartup;
use crate::process_stores::ProcessStores;
use crate::worker_observation_wiring::WorkerObservationWiring;
use awaken_service_lifecycle::StartupComponent;

pub(super) struct RuntimeStartup {
    pub(super) process: ProcessStartup,
    pub(super) worker_directory: awaken_coordinator::WorkerDirectoryHandle,
    pub(super) worker_observation_wiring: WorkerObservationWiring,
    pub(super) worker_transport: awaken_coordinator::WorkerTransport,
    pub(super) executable_agent_wiring: ExecutableAgentWiring,
    pub(super) executable_environment_wiring: ExecutableEnvironmentWiring,
    pub(super) enrollment_signing_key: [u8; 32],
}

pub(super) fn prepare(
    stores: &ProcessStores,
    mut process: ProcessStartup,
) -> Result<RuntimeStartup, String> {
    let role = process.role;
    if !role.owns_startup_component(StartupComponent::Coordinator) {
        return Err(format!(
            "{} role does not own Runtime process assembly",
            role.as_str()
        ));
    }
    if stores.coordinator.is_none() {
        return Err("runtime process requires Coordinator stores".into());
    }
    let owns_control = role.owns_startup_component(StartupComponent::Control);
    if owns_control && stores.control.is_none() {
        return Err("AllInOne requires Control stores".into());
    }
    if !owns_control && stores.control.is_some() {
        return Err("split Coordinator cannot own Control stores".into());
    }
    if !owns_control && process.control_service.is_none() {
        return Err("split Coordinator requires Control services".into());
    }
    let worker_directory = required(process.worker_directory.take(), "WorkerDirectory")?;
    let worker_observation_wiring =
        required(process.worker_observations.take(), "Worker observations")?;
    let worker_transport = required(process.worker_transport.take(), "Worker transport")?;
    let executable_agent_wiring = required(
        process.executable_agent_wiring.take(),
        "executable Agent wiring",
    )?;
    let executable_environment_wiring = required(
        process.executable_environment_wiring.take(),
        "executable Environment wiring",
    )?;
    #[cfg(not(any(test, feature = "test-support")))]
    {
        if process.deployment.is_none() {
            return Err("runtime process requires a resolved deployment".into());
        }
        if process.runtime_authority.is_none() {
            return Err("runtime process requires a selected Runtime authority".into());
        }
    }
    let enrollment_signing_key = if owns_control {
        #[cfg(any(test, feature = "test-support"))]
        {
            process.enrollment_signing_key.unwrap_or([0xA5; 32])
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            required(
                process.enrollment_signing_key,
                "derived enrollment signing key",
            )?
        }
    } else {
        [0; 32]
    };
    Ok(RuntimeStartup {
        process,
        worker_directory,
        worker_observation_wiring,
        worker_transport,
        executable_agent_wiring,
        executable_environment_wiring,
        enrollment_signing_key,
    })
}

fn required<T>(value: Option<T>, name: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("runtime process requires explicit {name}"))
}
