//! T35-C03 capability refusal: a container tier whose backend feature was not
//! compiled into this build is refused by the canonical realization builder
//! with a stable message, before any runtime client, daemon or cluster access.
//!
//! | Tier | Build | Refusal |
//! |---|---|---|
//! | Docker | no `docker` feature | `AWAKEN_SANDBOX_TIER=docker needs ...` |
//! | Podman | no `podman` feature | `AWAKEN_SANDBOX_TIER=podman needs ...` |
//! | K8s | no `k8s` feature | `AWAKEN_SANDBOX_TIER=k8s needs ...` |
//!
//! With another container feature enabled the tier-specific arm names the
//! missing Cargo feature; with none enabled the feature-less builder names the
//! tier. Each row only compiles where its feature is absent, so a build that
//! does enable the backend runs that backend's own qualification instead.
#![cfg(any(not(feature = "docker"), not(feature = "podman"), not(feature = "k8s")))]

use super::*;

/// The exact message each build shape must produce for a tier it lacks.
fn expected(tier: &str, feature: &str) -> String {
    if cfg!(any(feature = "docker", feature = "podman", feature = "k8s")) {
        format!("AWAKEN_SANDBOX_TIER={tier} needs the `{feature}` feature")
    } else {
        format!("AWAKEN_SANDBOX_TIER={tier} needs its matching container feature")
    }
}

async fn refusal(tier: SandboxTier) -> String {
    let namespace = awaken_sandbox_container::ContainerRealizationNamespace::from_stable_parts([
        "tier-refusal-test",
    ])
    .expect("static namespace parts are valid");
    match build_container_environment_for_realization(
        namespace,
        tier,
        Some("sandbox:latest"),
        &awaken_deployment_config::SandboxSettings::default(),
        &ContainerTierPolicy::default(),
        awaken_time::system_clock(),
    )
    .await
    {
        Ok(_) => panic!("a tier without its compiled backend must be refused"),
        Err(error) => error,
    }
}

#[cfg(not(feature = "docker"))]
#[tokio::test]
async fn docker_tier_without_its_feature_is_refused_with_a_stable_message() {
    assert_eq!(
        refusal(SandboxTier::Docker).await,
        expected("docker", "container-docker")
    );
}

#[cfg(not(feature = "podman"))]
#[tokio::test]
async fn podman_tier_without_its_feature_is_refused_with_a_stable_message() {
    assert_eq!(
        refusal(SandboxTier::Podman).await,
        expected("podman", "container-podman")
    );
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn k8s_tier_without_its_feature_is_refused_with_a_stable_message() {
    assert_eq!(
        refusal(SandboxTier::K8s).await,
        expected("k8s", "container-k8s")
    );
}
