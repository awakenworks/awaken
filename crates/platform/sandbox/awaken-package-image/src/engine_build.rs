//! The network and deadline of one Docker or Podman package image build. The
//! recipe itself is `awaken_container_contract::package_image_recipe`.

use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;

/// Package resolution may depend on an external registry, but it must never
/// occupy an Environment activation forever. Keep this aligned with the
/// Kubernetes Kaniko Job deadline.
pub(crate) const PACKAGE_BUILD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

/// The network one package image build is given. Every engine asks the same
/// question, so it is answered once here and each builder only renders the
/// answer in its own vocabulary. An engine that decided this for itself would
/// be the second mapping this module exists to prevent, and the two would
/// drift: the host opt-in reached Docker builds and silently never reached
/// Podman's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageBuildNetwork {
    /// The engine's own build network. Reachable, and bounded by the engine.
    EngineDefault,
    /// The deployment opted this build in to the host network.
    Host,
    /// The Environment denies egress, so its build gets none either.
    Denied,
}

/// Cause/effect for the build network, crossed over the host opt-in:
///
/// | Rule | policy       | host opt-in | Effect                     |
/// |------|--------------|-------------|----------------------------|
/// | B1   | Unrestricted | false       | EngineDefault              |
/// | B2   | Unrestricted | true        | Host                       |
/// | B3   | None         | either      | Denied                     |
/// | B4   | Allowlist    | either      | refused: no no-bypass net  |
///
/// B4 is a refusal rather than a narrowing because an image build that cannot
/// enforce the allowlist would reach hosts the Environment denies; failing the
/// activation closed is the only answer that preserves the egress boundary.
pub(crate) fn package_build_network(
    network: &pc::NetworkPolicy,
    host_opt_in: bool,
) -> Result<PackageBuildNetwork, RuntimeError> {
    match network {
        pc::NetworkPolicy::Unrestricted => Ok(if host_opt_in {
            PackageBuildNetwork::Host
        } else {
            PackageBuildNetwork::EngineDefault
        }),
        pc::NetworkPolicy::None => Ok(PackageBuildNetwork::Denied),
        pc::NetworkPolicy::Allowlist { .. } => Err(RuntimeError::Backend(
            "package image build has no no-bypass allowlist network".into(),
        )),
    }
}

/// A local mechanism failure (I/O, codec, rendered detail) of an engine build.
pub(crate) fn backend(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(error.to_string())
}
