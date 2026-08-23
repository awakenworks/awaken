use awaken_provisioning_contract as pc;

pub(crate) fn memory_mount(
    mount_id: &str,
    store_id: &str,
    mount_path: &str,
    access: pc::MountAccess,
) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: mount_id.into(),
        source: pc::MountSource::MemoryStore {
            store_id: store_id.into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: mount_path.into(),
        access,
        lifetime: pc::MountLifetime::Session,
        required: true,
    }
}
