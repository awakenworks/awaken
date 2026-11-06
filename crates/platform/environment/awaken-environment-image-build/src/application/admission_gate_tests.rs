//! T39-C02 oracle 1: a build that is not Ready and available never yields a
//! Session-admissible Environment snapshot, through either readiness path.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use awaken_environment_application::execution::EnvironmentExecutionApplication;
use awaken_environment_contract::{
    EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
};
use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
};

use super::*;
use crate::InMemoryEnvironmentImageBuildStore;

struct GateBuilder {
    builds: AtomicUsize,
    available: AtomicBool,
}

#[async_trait]
impl EnvironmentImageBuilder for GateBuilder {
    async fn base_image_identity(
        &self,
        reference: &str,
    ) -> Result<String, EnvironmentImageBuildError> {
        Ok(format!("{reference}@sha256:base"))
    }

    async fn build(
        &self,
        demand: &EnvironmentImageBuildDemand,
    ) -> Result<String, EnvironmentImageBuildError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{}@sha256:built", demand.base_image))
    }

    async fn available(
        &self,
        _demand: &EnvironmentImageBuildDemand,
        _image: &str,
    ) -> Result<bool, EnvironmentImageBuildError> {
        Ok(self.available.load(Ordering::SeqCst))
    }
}

/// Negative-control mutant: readiness that answers with the mutable base tag
/// whatever the durable build state is.
struct TagReadiness;

#[async_trait]
impl EnvironmentImageReadiness for TagReadiness {
    async fn ready_image(
        &self,
        _registration: &ExecutableEnvironmentRegistration,
        _base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        Ok(Some("registry/awaken:base".into()))
    }
}

pub(super) fn packaged(environment_id: &str) -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(
        EnvItem {
            id: environment_id.into(),
            revision: EnvironmentRevision(1),
            name: "gate".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::Cloud {
                networking: Default::default(),
                packages: EnvironmentPackages {
                    npm: vec!["tsx@4.0.0".into()],
                    ..Default::default()
                },
            },
            sandbox_policy: None,
            archived_at: None,
            created_at: Some("2026-02-03T04:05:06.007Z".into()),
            updated_at: Some("2026-02-03T04:05:06.007Z".into()),
        },
        None,
    )
}

struct Gate {
    store: Arc<InMemoryEnvironmentImageBuildStore>,
    builder: Arc<GateBuilder>,
    builds: Arc<EnvironmentImageBuildCoordinator>,
    catalog: Arc<ExecutableEnvironmentCatalog>,
    execution: EnvironmentExecutionApplication,
    registration: ExecutableEnvironmentRegistration,
}

async fn gate(environment_id: &str) -> Gate {
    let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
    let builder = Arc::new(GateBuilder {
        builds: AtomicUsize::new(0),
        available: AtomicBool::new(true),
    });
    let builds = Arc::new(
        EnvironmentImageBuildCoordinator::new(
            store.clone(),
            builder.clone(),
            "registry/awaken:base",
            EnvironmentImageBuildPolicy {
                wait_timeout: Duration::ZERO,
                poll_interval: Duration::from_millis(1),
                ..EnvironmentImageBuildPolicy::default()
            },
            awaken_time::system_clock(),
        )
        .unwrap(),
    );
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let registration = packaged(environment_id);
    BuildAwareExecutableEnvironmentRegistrar::new(
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
        builds.clone(),
    )
    .register(registration.clone())
    .await
    .unwrap();
    let execution = EnvironmentExecutionApplication::new(
        Arc::new(awaken_work_store::InMemoryWorkQueue::new(
            awaken_time::system_clock(),
        )),
        catalog.clone(),
    )
    .with_image_readiness(builds.clone());
    Gate {
        store,
        builder,
        builds,
        catalog,
        execution,
        registration,
    }
}

impl Gate {
    /// The oracle: no admissible snapshot and no non-blocking readiness. An
    /// unfinished build exhausts the bounded wait (`Timeout`); a known Failed
    /// build is refused at once as `Unavailable` while its retry stays queued
    /// (the coordinator's own Failed-readiness decision table).
    async fn refuses(&self, row: &str, known_failed: bool) {
        let id = &self.registration.definition.id;
        let refusal = self.execution.snapshot(id, None).await;
        assert!(
            if known_failed {
                matches!(refusal, Err(EnvironmentImageBuildError::Unavailable(_)))
            } else {
                matches!(refusal, Err(EnvironmentImageBuildError::Timeout(_)))
            },
            "{row}: blocking admission refuses an unready image"
        );
        assert_eq!(
            self.builds
                .ready_image_now(&self.registration, None)
                .await
                .unwrap(),
            None,
            "{row}: non-blocking readiness grants nothing"
        );
    }
}

/// G1 Pending, G2 Building under a live foreign lease, G3 Failed before its
/// retry: each refuses admission and readiness and invokes no builder outside
/// the claim. NC: the same admission check with a tag-answering readiness
/// mutant admits the Pending Environment with a mutable image and is detected.
#[tokio::test]
async fn unready_build_states_grant_no_session_admission() {
    let gate = gate("env-gate").await;
    let now = awaken_time::system_clock().now_ms();
    gate.refuses("G1 Pending", false).await;

    let claim = gate
        .store
        .claim_next("foreign-builder", now, 3_600_000)
        .await
        .unwrap()
        .expect("G2 claim");
    gate.refuses("G2 Building", false).await;

    assert!(
        gate.store
            .fail(&claim, "injected failure", now, 3_600_000)
            .await
            .unwrap()
    );
    gate.refuses("G3 Failed before retry", true).await;
    assert_eq!(gate.builder.builds.load(Ordering::SeqCst), 0, "G1-G3");

    let mutant = EnvironmentExecutionApplication::new(
        Arc::new(awaken_work_store::InMemoryWorkQueue::new(
            awaken_time::system_clock(),
        )),
        gate.catalog.clone(),
    )
    .with_image_readiness(Arc::new(TagReadiness));
    let admitted = mutant
        .snapshot(&gate.registration.definition.id, None)
        .await
        .expect("NC the mutant admits")
        .expect("NC snapshot");
    assert_eq!(
        admitted.prepared_image.as_deref(),
        Some("registry/awaken:base"),
        "NC the refusal oracle detects admission on a mutable tag"
    );
}

/// R1 Ready and available => admitted with the exact built digest; R2 the same
/// Ready image becomes unavailable => admission is refused again.
#[tokio::test]
async fn only_a_ready_and_available_image_is_admitted() {
    let gate = gate("env-ready").await;
    assert!(gate.builds.run_once("builder").await.unwrap());
    let snapshot = gate
        .execution
        .snapshot(&gate.registration.definition.id, None)
        .await
        .unwrap()
        .expect("R1 snapshot");
    assert_eq!(
        snapshot.prepared_image.as_deref(),
        Some("registry/awaken:base@sha256:base@sha256:built"),
        "R1"
    );
    gate.builder.available.store(false, Ordering::SeqCst);
    gate.refuses("R2 Ready but unavailable", false).await;
    assert_eq!(
        gate.builder.builds.load(Ordering::SeqCst),
        1,
        "R2 no rebuild"
    );
}
