//! Durable container-handle validation and physical locator decoding.

use awaken_provisioning_contract as pc;

use crate::{RuntimeError, err};

pub(crate) fn container_locator(
    handle: &pc::SandboxHandle,
) -> Result<(String, String), pc::SandboxError> {
    if handle.provider_kind != "container" {
        return Err(err(RuntimeError::Backend(format!(
            "container provider cannot adopt {:?} handle",
            handle.provider_kind
        ))));
    }
    let container_id = handle
        .extra
        .as_ref()
        .and_then(|value| value.get("container_id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| err(RuntimeError::Backend("handle missing container_id".into())))?;
    let outputs_path = handle
        .extra
        .as_ref()
        .and_then(|value| value.get("outputs_path"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("/mnt/session/outputs");
    Ok((container_id.to_string(), outputs_path.to_string()))
}
