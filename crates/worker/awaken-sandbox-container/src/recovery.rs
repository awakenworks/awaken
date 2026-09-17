//! Durable container-handle validation and physical locator decoding.

use awaken_provisioning_contract as pc;

pub(crate) type ContainerHandleV1 = pc::ContainerSandboxHandleV1;

pub(crate) fn decode_handle(
    handle: &pc::SandboxHandle,
) -> Result<ContainerHandleV1, pc::SandboxError> {
    let payload = handle.container_payload().cloned()?;
    if payload
        .runtime_handle
        .as_ref()
        .is_some_and(pc::ContainerContinuationHandle::is_host_bind_restoration)
    {
        return Err(pc::SandboxError::unsupported(
            "host-bind restoration requires the gated restoring provider",
        ));
    }
    Ok(payload)
}
