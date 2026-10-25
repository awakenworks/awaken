//! Resource projection reads the original installed Session throughout its lifecycle.
use super::*;
use awaken_provisioning_contract as pc;

fn mount(id: &str, memory: bool) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: id.into(),
        source: if memory {
            pc::MountSource::MemoryStore {
                store_id: id.into(),
                materialization_reference: None,
                write_consistency: Default::default(),
            }
        } else {
            pc::MountSource::Inline {
                contents: id.into(),
            }
        },
        mount_path: format!(".mnt/{id}"),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    }
}

#[tokio::test]
async fn mount_projection_reads_current_resource_and_frozen_inputs_without_retaining_removed_installed_sessions()
 {
    use crate::sandbox_layout::ManagedContentDelivery;
    use awaken_session_configuration_contract::{SessionBaseline, SessionBaselineInputs};
    // M1 unknown reads neither create an installed Session nor inherit another Session.
    // M2 the same original installed Session supplies Resource + baseline mounts and delivery.
    // M3 semantic delivery filters Memory from BOTH sources. M4 removal drops
    // every old projection, even through a retained component clone; recreating
    // the same Session id reads only the new installed Session, never an executor-owned cache.
    let host = crate::SessionExecutorBuilder::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .expect("validated Host dependencies");
    let staging = host.realization_executor.staging().clone();
    let thread = "mount-view";
    assert!(
        staging
            .thread_session_mounts(thread)
            .expect("installed Sessions")
            .is_empty(),
        "M1"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read(thread, |_| ())
            .expect("installed Sessions")
            .is_none(),
        "M1"
    );
    let resource = mount("resource", false);
    let resource_memory = mount("resource-memory", true);
    let baseline_mount = mount("baseline", false);
    let baseline_memory = mount("baseline-memory", true);
    let baseline = SessionBaseline::compile(SessionBaselineInputs {
        environment: awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "mount-view-environment".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "mount-view-environment-v1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        },
        runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
        mcp_authoring: Default::default(),
        agent_id: "agent".into(),
        agent_revision: None,
        model: "provider/model".into(),
        model_override: None,
        runtime: None,
        delegate_ids: Vec::new(),
        toolsets: Vec::new(),
        mounts: vec![baseline_mount.clone(), baseline_memory.clone()],
        env: Vec::new(),
        prompts: Vec::new(),
        transcript_prefix: None,
    });
    host.run_assembly
        .installed_sessions
        .replace_frozen_with(thread, |next| next.baseline = Some(baseline))
        .expect("installed Sessions");
    staging
        .sessions()
        .update_realization(thread, |r| {
            r.staging_mut().resources.mounts = vec![resource.clone(), resource_memory.clone()];
            r.staging_mut().content_delivery = Some(ManagedContentDelivery::ManagedFilesystem);
        })
        .expect("installed Sessions");
    assert_eq!(
        staging
            .thread_session_mounts(thread)
            .expect("installed Sessions"),
        vec![
            resource.clone(),
            resource_memory,
            baseline_mount.clone(),
            baseline_memory
        ],
        "M2"
    );
    staging
        .sessions()
        .update_realization(thread, |r| {
            r.staging_mut().content_delivery = Some(ManagedContentDelivery::SemanticTools);
        })
        .expect("installed Sessions");
    assert_eq!(
        staging
            .thread_session_mounts(thread)
            .expect("installed Sessions"),
        vec![resource, baseline_mount],
        "M3"
    );
    host.run_assembly.installed_sessions.remove(thread);
    assert!(
        staging
            .thread_session_mounts(thread)
            .expect("installed Sessions")
            .is_empty(),
        "M4 removed"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read(thread, |_| ())
            .expect("installed Sessions")
            .is_none(),
        "M4 no resurrection"
    );
    let replacement = mount("replacement", false);
    host.run_assembly
        .installed_sessions
        .install(thread)
        .expect("installed Sessions");
    staging
        .sessions()
        .update_realization(thread, |r| {
            r.staging_mut().resources.mounts = vec![replacement.clone()]
        })
        .expect("installed Sessions");
    assert_eq!(
        staging
            .thread_session_mounts(thread)
            .expect("installed Sessions"),
        vec![replacement],
        "M4 recreated"
    );
}
