//! Durable container-handle validation and physical locator decoding.

use crate::{RuntimeError, err};
use awaken_provisioning_contract as pc;

pub(crate) type ContainerHandleV1 = pc::ContainerSandboxHandleV1;

pub(crate) fn decode_handle(
    handle: &pc::SandboxHandle,
) -> Result<ContainerHandleV1, pc::SandboxError> {
    handle
        .container_payload()
        .cloned()
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))
}
