//! Durable container-handle validation and physical locator decoding.

use crate::ContainerHandleCodec as _;
use awaken_provisioning_contract as pc;

pub(crate) type ContainerHandleV1 = crate::ContainerSandboxHandleV1;

pub(crate) fn decode_handle(
    handle: &pc::SandboxHandle,
) -> Result<ContainerHandleV1, pc::SandboxError> {
    let payload = handle.container_payload()?;
    if payload
        .runtime_handle
        .as_ref()
        .is_some_and(crate::ContainerContinuationHandle::is_host_bind_restoration)
    {
        return Err(pc::SandboxError::unsupported(
            "host-bind restoration requires the gated restoring provider",
        ));
    }
    Ok(payload)
}

/// Lower the one verified original realization into its existing terminal
/// handle. Control identity is a Pod fact, never permission to bind a channel.
pub(crate) fn terminal_handle(
    spec: &pc::SandboxSpec,
    container_id: String,
    runtime_handle: crate::ContainerContinuationHandle,
    adoption_fingerprint: pc::SandboxRealizationFingerprint,
    realization_fingerprint: pc::SandboxRealizationFingerprint,
    live_input_projection: bool,
) -> Result<pc::SandboxHandle, pc::SandboxError> {
    let sandbox_control_incarnation = if spec.control_services.is_empty() {
        None
    } else {
        match &runtime_handle {
            crate::ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid,
                participants,
                ..
            } if !participants.terminal_only() && participants.restoration().is_none() => Some(
                crate::SandboxControlIncarnation::kubernetes_pod(pod_uid.clone())?,
            ),
            _ => {
                return Err(pc::SandboxError::invalid(
                    "terminal Control reconstruction lacks its complete original Pod realization",
                ));
            }
        }
    };
    let owned_paths = source_mount_paths(spec);
    Ok(pc::SandboxHandle::container_v2(
        &spec.scope,
        crate::ContainerSandboxHandleV2 {
            previous: crate::ContainerSandboxHandleV1 {
                container_id,
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection,
                continuation_excluded_paths: owned_paths.clone(),
                runtime_handle: Some(runtime_handle),
                sandbox_control_incarnation,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint,
            realization_fingerprint,
            owned_paths,
        },
    ))
}

pub(crate) fn source_mount_paths(spec: &pc::SandboxSpec) -> Vec<String> {
    spec.mounts.iter().map(|m| m.mount_path.clone()).collect()
}
