use awaken_resource_contract::{
    ConfigVersion, MemoryStoreAggregate, MemoryStoreConfigVersion, MemoryStoreDefinition,
    ResourceState, RetentionPolicy,
};
use proptest::prelude::*;

fn aggregate() -> MemoryStoreAggregate {
    MemoryStoreAggregate::register(
        MemoryStoreDefinition {
            id: "memory-property".into(),
            workspace_id: "workspace".into(),
            name: "Memory".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        MemoryStoreConfigVersion {
            memory_store_id: "memory-property".into(),
            version: ConfigVersion::INITIAL,
            retention_policy: RetentionPolicy::default(),
        },
    )
    .expect("valid property fixture")
}

proptest! {
    #[test]
    fn publication_accepts_only_the_exact_successor_and_failures_are_no_ops(
        expected in any::<u64>(),
        next in any::<u64>(),
    ) {
        // Model-based property: at current V1 only expected=V1,next=V2 commits.
        // Every other command is rejected and leaves the complete aggregate,
        // including immutable history and timestamps, byte-for-byte unchanged.
        let mut aggregate = aggregate();
        let before = aggregate.clone();
        let result = aggregate.publish_config(
            "workspace",
            ConfigVersion(expected),
            MemoryStoreConfigVersion {
                memory_store_id: "memory-property".into(),
                version: ConfigVersion(next),
                retention_policy: RetentionPolicy::default(),
            },
            42,
        );
        if expected == 1 && next == 2 {
            prop_assert!(result.is_ok());
            prop_assert_eq!(aggregate.definition().current_config_version, ConfigVersion(2));
            prop_assert!(aggregate.config(ConfigVersion::INITIAL).is_some());
            prop_assert!(aggregate.config(ConfigVersion(2)).is_some());
        } else {
            prop_assert!(result.is_err());
            prop_assert_eq!(aggregate, before);
        }
    }
}
