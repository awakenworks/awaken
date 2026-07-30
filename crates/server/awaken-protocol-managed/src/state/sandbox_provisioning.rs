//! Session admission for the frozen sandbox creation timing.

use super::RunError;
use awaken_session_contract::SandboxProvisioning;

pub(super) fn validate_sandbox_provisioning_runtime(
    provisioning: SandboxProvisioning,
    runtime: Option<&str>,
) -> Result<(), RunError> {
    if provisioning == SandboxProvisioning::OnToolUse && !matches!(runtime, None | Some("awaken")) {
        return Err(RunError::bad_request(format!(
            "sandbox_provisioning_unsupported: `on_tool_use` requires the native awaken runtime, got `{}`",
            runtime.unwrap_or_default()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use SandboxProvisioning::{Eager, OnToolUse};

    // Cause/effect rules:
    // eager -> accept every runtime; on_tool_use + native/implicit -> accept;
    // on_tool_use + ACP/unknown -> reject before Session realization.
    // C7 disabled policy and C8 exact-version freezing are owned by the
    // Environment policy tests, before this runtime compatibility boundary.
    #[test]
    fn native_only_lazy_provisioning_decision_table() {
        for (case, provisioning, runtime, accepted) in [
            ("C1 eager native", Eager, None, true),
            ("C2 eager ACP", Eager, Some("acp:claude"), true),
            ("C3 lazy implicit native", OnToolUse, None, true),
            ("C4 lazy explicit native", OnToolUse, Some("awaken"), true),
            ("C5 lazy ACP", OnToolUse, Some("acp:claude"), false),
            ("C6 lazy unknown runtime", OnToolUse, Some("remote"), false),
        ] {
            assert_eq!(
                validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
                accepted,
                "{case}"
            );
        }
    }
}
