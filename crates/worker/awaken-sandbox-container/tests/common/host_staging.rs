//! One ordinary host-bind lifetime/recovery oracle for Docker and Podman.
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_container::{
    ContainerEnvironmentAdoption, ContainerEnvironmentProvider, ContainerProvider,
    ContainerRuntime, ContainerState,
};
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn verify_ordinary_host_staging_recovery<R: ContainerRuntime + 'static>(
    first_runtime: Arc<R>,
    peer_runtime: Arc<R>,
    image: &str,
) {
    /* Host-bind lifetime and recovery decision table. C1 ordinary creation has
     * completed; C2 all creator wrappers are dropped; C3 a fresh runtime reads
     * the exact daemon mount evidence; C4 authorized terminal disposal succeeds.
     * R1=C1+C2 => physical container and its exact host bytes survive;
     * R2=R1+C3 => fresh provider adopts the same handle and reads a newly
     * projected input through the retained bind; R3=R2+C4 => both physical
     * container and staging disappear, repeated disposal is harmless. R4 an
     * unfenced create retry is rejected before staging; retained files alone
     * never authorize a second physical creation or replacement.
     * These are real daemon operations, not a successful mock recovery claim.
     * The missing-root rejection row remains in podman_it's peer test. */
    let scope = format!(
        "ordinary-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let spec = pc::SandboxSpec {
        scope,
        isolation: pc::IsolationClass::Container,
        environment: Some(pc::EnvironmentKind::Image {
            reference: image.into(),
        }),
        command: vec!["sleep".into(), "30".into()],
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "source".into(),
            source: pc::MountSource::Inline {
                contents: "source-before-drop".into(),
            },
            mount_path: "/data/source.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    };
    let creator = ContainerProvider::new(first_runtime, image);
    let sandbox = creator.create(&spec).await.expect("R1 ordinary creation");
    let handle = sandbox.handle();
    let physical_id = handle.container_payload().unwrap().container_id.clone();
    let root = peer_runtime
        .recovered_host_staging_root(&physical_id)
        .await
        .unwrap()
        .unwrap();
    drop(sandbox);
    drop(creator);

    // Capture the old-bug evidence before cleanup so a red test leaves no daemon
    // object behind. Retention is a required independent assertion of recovery.
    let retained = root.is_dir();
    if !retained {
        peer_runtime.remove(&physical_id).await.unwrap();
    }
    assert!(
        retained,
        "R1 wrapper drop cannot unlink a live container's host binds"
    );
    assert_eq!(
        peer_runtime.inspect(&physical_id).await.unwrap(),
        ContainerState::Running
    );
    peer_runtime
        .project_live_input(
            &physical_id,
            "/mnt/session/uploads/after.txt",
            b"source-after-drop",
        )
        .await
        .unwrap();
    let peer = ContainerProvider::new(peer_runtime.clone(), image);
    let adopted = peer
        .adopt_environment(ContainerEnvironmentAdoption::new(&spec, &handle))
        .await
        .expect("R2 fresh provider adopts exact retained evidence");
    assert_eq!(adopted.handle(), handle, "R2 same physical environment");
    let process = adopted.spawn(pc::Command::new(["sh", "-c",
        "test \"$(cat /data/source.txt)\" = source-before-drop && test \"$(cat /mnt/session/uploads/after.txt)\" = source-after-drop"
    ])).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), process.wait()).await;
    let error = match peer.create(&spec).await {
        Ok(_) => panic!("R4 unfenced create cannot recover an existing effect"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("durable effect fence"),
        "R4 {error}"
    );
    assert!(root.is_dir(), "R4 rejection preserves the exact live stage");
    adopted
        .dispose()
        .await
        .expect("R3 canonical terminal disposal");
    adopted.dispose().await.expect("R3 disposal replay");
    assert_eq!(
        result.unwrap().unwrap().code,
        Some(0),
        "R2 exact original and fresh bytes"
    );
    assert!(
        !root.exists(),
        "R3 terminal disposal removes retained staging"
    );
    assert!(
        !matches!(
            peer_runtime.inspect(&physical_id).await,
            Ok(ContainerState::Running)
        ),
        "R3 physical container removed"
    );
}
