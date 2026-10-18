// @formal evidence session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal evidence session_resources.explicit_empty_manifest_revokes_runtime_projection
//! Physical Resource generations use the actual Session Root and exact transitions.

use super::*;

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn live_file_replacement_requires_the_exact_resource_transition() {
    let storage = tempfile::tempdir().expect("storage");
    let roots = ManagedSessionRootFixture::new().await;
    let mut raw_host = roots.configure(adoption_dependencies_at(storage.path()).await);
    // Cause/effect decision table. Causes: C1 the Session is physically empty;
    // C2 the complete Dispatch projection supplies the exact Empty->desired
    // transition; C3 a following desired-only manifest is exact/different; C4
    // the aggregate supplies the exact previous->desired transition; C5 an
    // Environment is resident. Effects: E1 the canonical projection stages
    // cold requirements and exact desired-only replay does not publish an
    // active manifest; E2 physical create publishes the Root-returned initial generation; E3 a
    // desired-only replacement fails with no logical/physical change; E4 the
    // exact transition removes the File, clears every logical Runtime
    // projection, performs no target activation, and only then publishes
    // generation 2.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | F1 | T | yes | exact | n/a | F | E1 |
    // | F2 | T | yes | exact | yes | T | E2 |
    // | F3 | F | n/a | different | no | T | E3 |
    // | F4 | F | n/a | n/a | yes | T | E4: physical + logical revocation |
    // Workdir remains correctly ineligible for F1's immutability requirement.
    raw_host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            storage.path().join("sandboxes"),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let host = (raw_host).build().unwrap();
    let managed = managed_test_host(host.clone());
    let bytes = b"frozen worker input".to_vec();
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            "workspace-a",
            "input.bin".into(),
            "application/octet-stream".into(),
            &bytes,
        )
        .await
        .expect("create file")
        .id;
    let declaration = crate::session_resource::SessionResourceManifest::new(
        "workspace-a",
        crate::session_resource::ResolvedSessionResources::try_new(
            vec![crate::session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                source: crate::session_resource::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                },
                mount_path: "/uploads/input.bin".to_string(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );

    let projection = roots
        .install_projection(
            &managed,
            "thread-cold-resource",
            frozen_projection_for_manifest(
                ordinary_frozen_projection().baseline.environment,
                &declaration,
            ),
        )
        .await
        .expect("F1 actual Root prepares the complete initial transition");
    let manifest = crate::session_resource::SessionResourceManifest::at_revision(
        projection.workspace_id.clone(),
        projection.resource_revision,
        projection.resources.clone(),
    );
    assert!(
        roots
            .prepare_resource_update(
                &managed,
                "thread-cold-resource",
                projection.clone(),
                Default::default(),
            )
            .await
            .is_err(),
        "F1 staged desired input alone cannot acknowledge physical preparation"
    );
    host.realization_executor
        .install_dispatched_resources("thread-cold-resource", &manifest, None)
        .await
        .expect("F1 revalidate the exact desired manifest without republishing");
    assert!(
        active_manifest(&host, "thread-cold-resource").is_none(),
        "F1 desired-only staging is not completion evidence"
    );
    let activation = test_activation("thread-cold-resource", "run-cold-resource");
    host.run_assembly
        .ctx_for_snapshot(
            "thread-cold-resource",
            Some("agent-a"),
            Some(activation.snapshot),
        )
        .await
        .expect("open environment after resource install");

    let mount = host
        .realization_executor
        .sandbox_spec("thread-cold-resource")
        .mounts
        .into_iter()
        .find(|mount| mount.mount_id == file_id)
        .expect("frozen File mount");
    assert_eq!(mount.mount_path, "/mnt/session/uploads/uploads/input.bin");
    assert_eq!(
        mount.access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );
    let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = mount.source
    else {
        panic!("frozen File uses the binary-safe carried source")
    };
    assert_eq!(contents, bytes);
    assert_eq!(
        active_manifest(&host, "thread-cold-resource"),
        Some(manifest.clone())
    );

    let cleared = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        manifest.revision + 1,
        crate::session_resource::ResolvedSessionResources::default(),
    );
    let error = host
        .realization_executor
        .install_dispatched_resources("thread-cold-resource", &cleared, None)
        .await
        .expect_err("F3 desired-only replacement must fail closed");
    assert_eq!(error.code, "session_resource_transition_conflict", "F3");
    assert_eq!(
        active_manifest(&host, "thread-cold-resource"),
        Some(manifest.clone()),
        "F3 retains the active manifest"
    );
    assert!(
        !host
            .realization_executor
            .environments()
            .session_environment("thread-cold-resource")
            .await
            .expect("installed Sessions")
            .expect("F3 resident environment")
            .harvest()
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .expect("F3 list live files")
            .is_empty(),
        "F3 does not remove the live File"
    );

    let updated = roots
        .prepare_resource_update(
            &managed,
            "thread-cold-resource",
            projection,
            Default::default(),
        )
        .await
        .expect("F4 actual preparation acknowledgement and aggregate-owned replacement");
    assert_eq!(
        updated.resource_revision, cleared.revision,
        "F4 Root allocates next"
    );
    assert_eq!(
        updated.previous_resource_manifest,
        Some(manifest.clone()),
        "F4 retains exact previous"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("thread-cold-resource")
            .await
            .expect("installed Sessions")
            .expect("resident environment")
            .harvest()
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .expect("list live files")
            .is_empty(),
        "F4 removes the stale physical projection"
    );
    assert!(
        host.realization_executor
            .sandbox_spec("thread-cold-resource")
            .mounts
            .is_empty(),
        "F4 publishes the empty staged projection"
    );
    assert_eq!(
        active_manifest(&host, "thread-cold-resource"),
        Some(cleared),
        "F4 publishes generation 2 only after realization"
    );
    let empty_projection = host
        .run_assembly
        .installed_sessions
        .read("thread-cold-resource", |slot| {
            (
                slot.realization.staging().resources.mounts.is_empty(),
                slot.realization.staging().resources.prompts.is_empty(),
                slot.realization
                    .staging()
                    .resources
                    .resource_prompts
                    .is_empty(),
                slot.realization
                    .staging()
                    .resources
                    .binding_checks
                    .is_empty(),
                slot.realization.staging().resources.repositories.is_empty(),
                slot.realization.staging().memory_bindings.is_empty(),
                slot.realization.staging().memory.is_none(),
                slot.realization
                    .staging()
                    .skills
                    .as_ref()
                    .is_some_and(Vec::is_empty),
                slot.run.context.is_none(),
            )
        })
        .expect("F4 canonical Session slot");
    assert_eq!(
        empty_projection,
        (true, true, true, true, true, true, true, true, true),
        "F4 explicit empty is a complete Runtime revocation, not absent input"
    );
}

/// Desired-only versus exact-transition cause/effect decision table.
/// Causes: C0 a complete Dispatch projection supplies the initial exact
/// Empty->desired transition; C1 an active process-local generation exists; C2
/// desired-only input is exact/different; C3 the aggregate supplies exact
/// previous+desired.
/// Effects: E1 exact input revalidates cold requirements without republishing;
/// E2 every desired-only change rejects and retains the active generation; E3
/// exact transition advances physical/logical state. Rules:
/// R0=C0+!C1+exact=>E1; R1=C1+exact=>E1; R2=C1+different+!C3=>E2;
/// R3=C1+different+C3=>E3. Revision ordering and Workspace alone never
/// substitute for the aggregate transition.
#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn desired_only_worker_requires_an_exact_transition_for_every_change() {
    let storage = tempfile::tempdir().expect("storage");
    let roots = ManagedSessionRootFixture::new().await;
    let mut raw_host = roots.configure(adoption_dependencies_at(storage.path()).await);
    raw_host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            storage.path().join("sandboxes"),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let host = raw_host.build().unwrap();
    let managed = managed_test_host(host.clone());
    let claim = ingress::RunClaim {
        run_id: RunId("run-generation-fence".into()),
        owner: "worker-generation-fence".into(),
        epoch: 1,
    };
    // R0 supplies real nonempty data: File lowering always preserves immutable
    // ReadOnly access. The existing Namespace provider supplies that capability
    // and Linux path fidelity; Workdir correctly cannot admit this File. Live
    // Memory would be an invalid positive because its create-time pin cannot
    // be removed. R3 is File->empty through actual Control/manifest commands,
    // never a manufactured empty->empty revision. R1/R2 retain exact replay and
    // lower/equal/foreign desired-only rejection. No missing physical backend
    // becomes a positive or a Linux skip; non-Linux path fidelity is separate.
    let bytes = b"actual generation fence input";
    let file_id = host
        .file_application()
        .expect("File fixture owner")
        .create_uploaded_file(
            "workspace-a",
            "generation.txt".into(),
            "text/plain".into(),
            bytes,
        )
        .await
        .expect("create actual scoped File")
        .id;
    let declaration = crate::session_resource::SessionResourceManifest::new(
        "workspace-a",
        crate::session_resource::ResolvedSessionResources::try_new(
            vec![crate::session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("generation-file"),
                source: crate::session_resource::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                },
                mount_path: "/generation.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );
    let projection = roots
        .install_projection(
            &managed,
            "thread-generation-fence",
            frozen_projection_for_manifest(
                ordinary_frozen_projection().baseline.environment,
                &declaration,
            ),
        )
        .await
        .expect("Root prepares the complete initial transition");
    let initial_manifest = crate::session_resource::SessionResourceManifest::at_revision(
        projection.workspace_id.clone(),
        projection.resource_revision,
        projection.resources.clone(),
    );
    host.realization_executor
        .install_dispatched_resources("thread-generation-fence", &initial_manifest, None)
        .await
        .expect("revalidate the exact desired manifest without republishing");
    host.run_assembly
        .ctx_for_snapshot(
            "thread-generation-fence",
            Some("agent-a"),
            Some(test_activation("thread-generation-fence", "open-generation-fence").snapshot),
        )
        .await
        .expect("physically publish initial generation");
    assert_eq!(
        initial_manifest.resources.inputs().len(),
        1,
        "R0 genuine nonempty Root desired"
    );
    let environment = host
        .realization_executor
        .environments()
        .session_environment("thread-generation-fence")
        .await
        .expect("installed Sessions")
        .expect("R0 physically resident File owner");
    let initial_files = environment
        .harvest()
        .list_frozen_mount_files("/mnt/session/uploads")
        .await
        .expect("R0 actual File projection");
    assert_eq!(initial_files.len(), 1, "R0 actual File before removal");

    host.realization_executor
        .install_dispatched_resources("thread-generation-fence", &initial_manifest, Some(&claim))
        .await
        .expect("R1 exact replay revalidation");

    for rejected in [
        crate::session_resource::SessionResourceManifest::at_revision(
            "workspace-a",
            initial_manifest.revision - 1,
            crate::session_resource::ResolvedSessionResources::default(),
        ),
        crate::session_resource::SessionResourceManifest::at_revision(
            "workspace-a",
            initial_manifest.revision,
            crate::session_resource::ResolvedSessionResources::try_new(
                Vec::new(),
                vec![crate::session_resource::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "different-same-generation".into(),
                    version: 1,
                    bundle_sha256: "sha256-different".into(),
                }],
            )
            .unwrap(),
        ),
        crate::session_resource::SessionResourceManifest::at_revision(
            "workspace-b",
            initial_manifest.revision + 1,
            crate::session_resource::ResolvedSessionResources::default(),
        ),
    ] {
        let result = host
            .realization_executor
            .install_dispatched_resources("thread-generation-fence", &rejected, Some(&claim))
            .await;
        assert!(
            result.is_err(),
            "R2 rejects desired-only replacement: {rejected:?}"
        );
        assert_eq!(
            active_manifest(&host, "thread-generation-fence"),
            Some(initial_manifest.clone())
        );
    }

    let next_manifest = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        initial_manifest.revision + 1,
        crate::session_resource::ResolvedSessionResources::default(),
    );
    host.realization_executor
        .install_dispatched_resources("thread-generation-fence", &next_manifest, Some(&claim))
        .await
        .expect_err("R2 newer desired-only generation still lacks previous");
    let updated = roots
        .prepare_resource_update(
            &managed,
            "thread-generation-fence",
            projection,
            Default::default(),
        )
        .await
        .expect("R3 actual aggregate transition advances the prepared generation");
    assert_eq!(
        updated.resource_revision, next_manifest.revision,
        "R3 actual next"
    );
    assert_eq!(
        updated.previous_resource_manifest,
        Some(initial_manifest),
        "R3 exact previous"
    );
    assert_eq!(
        active_manifest(&host, "thread-generation-fence"),
        Some(next_manifest),
        "R3 publishes only after exact physical transition"
    );
    assert!(
        environment
            .harvest()
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .expect("R3 actual post-removal File projection")
            .is_empty(),
        "R3 exact transition physically removed the real initial File"
    );
}
