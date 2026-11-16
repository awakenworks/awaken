//! Checkpoint formats of a Worker whose Session provider is the local
//! (Workdir) tier, decided by `WorkerNodeBuilder::build`.
//!
//! | Rule | local tier | checkpoint store | Effect |
//! |---|---|---|---|
//! | L1 | Workdir | absent | ordinary mode: no provider format advertised |
//! | L2 | Workdir | builder store | the provider's own format is advertised |
//! | L3 | Workdir | deployment `sandbox_checkpoint_s3` | the provider's own format is advertised |
//!
//! A checkpoint-and-release Session requires its exact format in the claiming
//! Worker's manifest (dispatch placement), so before L2/L3 a registered
//! Workdir Worker, the co-located AllInOne one included, could never claim it
//! even with the store installed.

use std::sync::Arc;

use super::{UnusedCheckpointStore, WorkerNodeBuilder, deployment};

async fn local_worker(
    deployment: awaken_deployment_config::DeploymentConfig,
    store: bool,
) -> crate::WorkerNode {
    let builder = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("http://control"),
        awaken_time::system_clock(),
    )
    .with_deployment_config(deployment, false)
    .with_standard_manifest(Default::default());
    let builder = if store {
        builder.with_environment_checkpoint_store(Arc::new(UnusedCheckpointStore))
    } else {
        builder
    };
    builder
        .prepare_session_environment_from_deployment()
        .await
        .expect("the Workdir tier is available on every host")
        .build()
        .expect("a Workdir Worker builds with or without checkpoint custody")
}

#[tokio::test]
async fn a_workdir_worker_advertises_its_checkpoint_format_only_with_custody() {
    let base = tempfile::tempdir().unwrap();
    let mut workdir = deployment();
    workdir.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
    workdir.sandbox_dir = Some(base.path().to_path_buf());
    let format = "awaken-fs-tar-v1".to_string();

    let ordinary = local_worker(workdir.clone(), false).await;
    assert!(
        !ordinary.manifest().checkpoint_formats.contains(&format),
        "L1"
    );

    let explicit = local_worker(workdir.clone(), true).await;
    assert!(
        explicit.manifest().checkpoint_formats.contains(&format),
        "L2"
    );

    let mut s3 = workdir;
    s3.sandbox.checkpoint_s3 = Some(awaken_deployment_config::SandboxCheckpointS3Config {
        bucket: "checkpoints".into(),
        region: "us-east-1".into(),
        prefix: "awaken".into(),
    });
    let deployed = local_worker(s3, false).await;
    assert!(
        deployed.manifest().checkpoint_formats.contains(&format),
        "L3"
    );
}
