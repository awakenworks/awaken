// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Start-up capability evidence of a container engine (ADR-0092 decision 5): a
// @formal reason: capability whose probe fails is withheld from the advertised manifest, never
// @formal reason: assumed. Covered by the exhaustive decision tables in this module and the
// @formal reason: scripted-engine tests of each adapter.
//! What a container engine proved at start-up, before the Worker registers.

/// Capabilities the engine proved before registration. The default image must
/// resolve or the probe fails outright: without it no isolation tier remains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerStartupEvidence {
    /// The engine enforces CPU, memory and pid limits.
    pub resource_limits: bool,
    /// The configured package builder answered (true when none is configured
    /// and the engine itself builds).
    pub package_builder: bool,
}

/// Limits are enforceable only when every controller a spec may cap is.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) const fn limits_enforceable(cpu: bool, memory: bool, pids: bool) -> bool {
    cpu && memory && pids
}

/// Podman reports its delegated cgroup controllers as a JSON string list
/// (`podman info --format '{{json .Host.CgroupControllers}}'`); rootless Podman
/// without delegation lists fewer than the three limits need.
#[cfg(feature = "podman")]
pub(crate) fn podman_controllers_enforce_limits(controllers_json: &str) -> bool {
    let Ok(controllers) = serde_json::from_str::<Vec<String>>(controllers_json.trim()) else {
        return false;
    };
    let has = |name: &str| controllers.iter().any(|controller| controller == name);
    limits_enforceable(has("cpu"), has("memory"), has("pids"))
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[cfg(any(feature = "docker", feature = "podman"))]
    #[test]
    fn limits_are_advertised_only_when_every_controller_is_enforced() {
        // Decision table over (cpu, memory, pids): only all three => limits.
        for bits in 0u8..8 {
            let (cpu, memory, pids) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            assert_eq!(
                limits_enforceable(cpu, memory, pids),
                bits == 7,
                "{bits:03b}"
            );
        }
    }

    #[cfg(feature = "podman")]
    #[test]
    fn podman_controller_lists_withhold_limits_unless_complete() {
        // P1 full delegation; P2 rootless without cpu/pids delegation; P3
        // empty; P4 unparsable output fails closed.
        assert!(podman_controllers_enforce_limits(
            r#"["cpuset","cpu","io","memory","pids"]"#
        ));
        assert!(!podman_controllers_enforce_limits(r#"["memory"]"#));
        assert!(!podman_controllers_enforce_limits("[]"));
        assert!(!podman_controllers_enforce_limits("null"));
        assert!(!podman_controllers_enforce_limits("not json"));
    }
}
