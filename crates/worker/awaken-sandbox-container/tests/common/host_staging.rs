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
     * R5=live container+missing root => reject without removal/replacement;
     * R6=live container+symlink root => reject without following it; restoring
     * the exact root then enables R2, so rejection cannot destroy its bytes.
     * R7=R2+second process loss => a cold provider reconstructs the same terminal
     * participants, prepares and performs exact fenced cleanup without reactivation;
     * R8=absent container after R3 => reject without recreating physical state.
     * The creating wrapper being gone is not itself evidence of missing binds.
     * Both real daemon adapters consume this one decision table. */
    let scope = format!(
        "ordinary-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let spec = pc::SandboxSpec {
        // This shared Docker/Podman row qualifies bind retention, not the
        // separately probed process-tree supervision guarantee.
        process_tree_supervision: false,
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
    let initial_fence = super::disposal::effect_fence(&spec.scope, "create", "creator", 1);
    let terminal_fence = super::disposal::effect_fence(&spec.scope, "cleanup", "successor", 2);
    let creator = ContainerProvider::new(first_runtime, image);
    let sandbox = creator
        .create_for_effect(
            &spec,
            &initial_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&initial_fence),
        )
        .await
        .expect("R1 ordinary creation");
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
    let displaced = root.with_extension("test-displaced");
    assert!(
        !displaced.exists(),
        "fault target belongs only to this test"
    );
    std::fs::rename(&root, &displaced).unwrap();
    let missing_rejected = peer
        .adopt_environment(ContainerEnvironmentAdoption::new(&spec, &handle))
        .await
        .is_err();
    let missing_preserved =
        peer_runtime.inspect(&physical_id).await.unwrap() == ContainerState::Running;
    #[cfg(unix)]
    let (symlink_rejected, symlink_preserved) = {
        std::os::unix::fs::symlink(&displaced, &root).unwrap();
        let rejected = peer
            .adopt_environment(ContainerEnvironmentAdoption::new(&spec, &handle))
            .await
            .is_err();
        let preserved = peer_runtime.inspect(&physical_id).await.unwrap()
            == ContainerState::Running
            && displaced.is_dir();
        std::fs::remove_file(&root).unwrap();
        (rejected, preserved)
    };
    std::fs::rename(&displaced, &root).unwrap();
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
    // R7: a second process replacement after ordinary adoption must also
    // reconstruct terminal participants. It must not need a warm cache or
    // reactivate the Session. This is the multi-session cleanup failure's
    // missing adapter edge, exercised identically on both real daemons.
    drop(adopted);
    drop(peer);
    let cold = ContainerProvider::new(peer_runtime.clone(), image);
    let cleanup = cold
        .prepare_terminal_for_effect(
            &spec,
            Some(&handle),
            Some(&initial_fence),
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("R7 cold terminal reconstruction")
        .expect("exact object exists");
    assert_eq!(
        cleanup.handle(),
        handle,
        "R7 no replacement or reactivation"
    );
    cleanup
        .prepare_disposal_for_effect(
            &terminal_fence,
            &pc::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("R7 source preparation");
    let authorization = super::disposal::disposal_authorization(&terminal_fence);
    // R3/R7 preparation and physical disposal carry different operation ids;
    // the shared authority fixture binds the exact effect at each boundary.
    let disposal_authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
    cleanup
        .dispose_for_effect(&authorization, &disposal_authority)
        .await
        .expect("R3 canonical terminal disposal");
    cleanup
        .dispose_for_effect(&authorization, &disposal_authority)
        .await
        .expect("R3 disposal replay");
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
    assert!(
        missing_rejected && missing_preserved,
        "R5 reject with zero physical removal"
    );
    #[cfg(unix)]
    assert!(
        symlink_rejected && symlink_preserved,
        "R6 reject with zero symlink-target mutation"
    );
    assert!(
        cold.adopt_environment(ContainerEnvironmentAdoption::new(&spec, &handle))
            .await
            .is_err(),
        "R8 absent physical environment is not silently recreated"
    );
    assert!(!root.exists(), "R8 rejection cannot recreate staging");
}
