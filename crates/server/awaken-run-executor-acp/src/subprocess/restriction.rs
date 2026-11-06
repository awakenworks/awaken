// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Validates the actual launch restriction and canonical host environment projection;
// @formal reason: it never grants a tool or replaces Run permission/ceiling authority.
//! The existing subprocess launch's fixed-program restriction checks.
use super::{AcpLaunch, Codec, HOST_PASSTHROUGH_ENV, OpenError, pc};

impl AcpLaunch {
    /// Whether the actual launch contains only the canonical local executable/
    /// home locators, without duplicate keys, secret references or egress values.
    /// This is a value check, not a declaration of a tool execution capability.
    #[must_use]
    pub fn uses_only_host_lookup_environment(&self) -> bool {
        let mut names = std::collections::BTreeSet::new();
        self.env.iter().all(|entry| {
            HOST_PASSTHROUGH_ENV.contains(&entry.name.as_str())
                && names.insert(entry.name.as_str())
                && entry.visibility == pc::EnvVisibility::Process
                && matches!(&entry.value, pc::EnvValue::Inline { .. })
        })
    }

    pub fn check_tool_narrowing(
        &self,
        codec: Codec,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> Result<(), OpenError> {
        (self.tool_narrowing_check)(self, codec, narrowing)
    }

    /// All unproven launch constructors share the same default refusal.
    pub fn opaque_tool_narrowing(
        _launch: &Self,
        _codec: Codec,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> Result<(), OpenError> {
        crate::opaque_tool_narrowing(narrowing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::inline_env;
    #[test]
    fn host_lookup_environment_check_is_exact_and_does_not_grant_tools() {
        // C/E table: E1 env is only unique inline/process host locators; R1 empty
        // or canonical locator=>E1; R2 unknown key/duplicate/secret/egress=>!E1.
        // Every row's ordinary custom launch still rejects DenyAll: this query
        // never attests an executor profile by itself.
        let mut launch = AcpLaunch::custom(vec!["fixture".into()], Vec::new());
        assert!(launch.uses_only_host_lookup_environment());
        for row in 0..5 {
            launch.env = vec![inline_env("HOME", "/private")];
            match row {
                0 => {}
                1 => launch.env[0].name = "BASH_ENV".into(),
                2 => launch.env.push(inline_env("HOME", "/other")),
                3 => {
                    launch.env[0].value = pc::EnvValue::Secret {
                        reference: "private-ref".into(),
                    }
                }
                4 => launch.env[0].visibility = pc::EnvVisibility::EgressOnly,
                _ => unreachable!(),
            }
            assert_eq!(launch.uses_only_host_lookup_environment(), row == 0);
            assert!(
                launch
                    .check_tool_narrowing(
                        Codec::Newline,
                        &awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll
                    )
                    .is_err()
            );
        }
    }
}
