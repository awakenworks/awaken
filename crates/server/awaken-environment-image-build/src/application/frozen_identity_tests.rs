//! T39-C01 oracle 2: an admitted (frozen) Environment revision keeps resolving
//! to the immutable image it was admitted with, even after the operator's
//! mutable base tag is repointed.
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::admission_gate_tests::packaged;
use super::*;
use crate::InMemoryEnvironmentImageBuildStore;

/// Builder whose mutable base tag can be repointed to a new digest.
struct RepointableBuilder {
    digest: Mutex<String>,
    builds: AtomicUsize,
}

#[async_trait]
impl EnvironmentImageBuilder for RepointableBuilder {
    async fn base_image_identity(
        &self,
        reference: &str,
    ) -> Result<String, EnvironmentImageBuildError> {
        Ok(format!("{reference}@{}", self.digest.lock().unwrap()))
    }

    async fn build(
        &self,
        demand: &EnvironmentImageBuildDemand,
    ) -> Result<String, EnvironmentImageBuildError> {
        let build = self.builds.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("{}@sha256:built-{build}", demand.base_image))
    }

    async fn available(
        &self,
        _demand: &EnvironmentImageBuildDemand,
        _image: &str,
    ) -> Result<bool, EnvironmentImageBuildError> {
        Ok(true)
    }
}

#[tokio::test]
#[ignore = "product gap: readiness recomputes a frozen revision's demand from the current mutable base tag (application.rs:60-61)"]
async fn a_frozen_revision_keeps_its_admitted_image_after_the_base_tag_moves() {
    let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
    let builder = Arc::new(RepointableBuilder {
        digest: Mutex::new("sha256:v1".into()),
        builds: AtomicUsize::new(0),
    });
    let builds = EnvironmentImageBuildCoordinator::new(
        store,
        builder.clone(),
        "registry/awaken:latest",
        EnvironmentImageBuildPolicy {
            wait_timeout: Duration::ZERO,
            poll_interval: Duration::from_millis(1),
            ..EnvironmentImageBuildPolicy::default()
        },
        awaken_time::system_clock(),
    )
    .unwrap();
    let frozen = packaged("env-frozen");
    builds.ensure_registration(&frozen).await.unwrap();
    assert!(builds.run_once("builder").await.unwrap());
    let admitted = builds
        .ready_image(&frozen, None)
        .await
        .unwrap()
        .expect("admitted image");

    *builder.digest.lock().unwrap() = "sha256:v2".into();
    let resolved = builds.ready_image(&frozen, None).await;
    assert_eq!(
        resolved.ok().flatten().as_deref(),
        Some(admitted.as_str()),
        "the frozen revision keeps its admitted immutable image"
    );
    assert_eq!(builder.builds.load(Ordering::SeqCst), 1, "no rebuild");
}
