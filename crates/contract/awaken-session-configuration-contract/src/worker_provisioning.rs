// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: Validates the existing frozen Environment provisioning/runtime compatibility before Session effects.
//! Session runtime compatibility with frozen Environment provisioning.
use awaken_provisioning_contract::SandboxProvisioning;
use awaken_session_types::RunError;
pub fn is_native_session_runtime(runtime: Option<&str>) -> bool {
    runtime.is_none_or(|backend_ref| {
        matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
            awaken_runtime_contract::resolved::Backend::Native
        )
    })
}

pub fn validate_sandbox_provisioning_runtime(
    provisioning: SandboxProvisioning,
    runtime: Option<&str>,
) -> Result<(), RunError> {
    if provisioning == SandboxProvisioning::OnToolUse && !is_native_session_runtime(runtime) {
        return Err(RunError::bad_request(format!(
            "sandbox_provisioning_unsupported: `on_tool_use` requires a native backend, got `{}`",
            runtime.unwrap_or_default()
        )));
    }
    Ok(())
}
