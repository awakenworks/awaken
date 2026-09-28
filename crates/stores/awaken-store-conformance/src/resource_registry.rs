// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-resource-store/tests/registry_conformance.rs
// @formal reason: Trait-generic ResourceRegistryRepository conformance suite (ADR-0081 §7). It is a
// @formal reason: tooling-only test library every backend's test target calls; no product crate links or
// @formal reason: invokes it, and it admits nothing at runtime.
//! Trait-generic conformance suite for the `ResourceRegistryRepository` port
//! (ADR-0081 §7).
//!
//! Every backend runs [`run_suite`] on fresh stores. The laws: R1 insert then
//! load returns the registered aggregate at its initial revision; R2 a replace
//! at the loaded revision commits and advances exactly one revision; R3 a
//! replace at a stale revision is classified `ConcurrentModification` and
//! leaves the committed aggregate untouched; R4 a second insert of the same id
//! is classified, never a second row; R5 the Repository aggregate obeys the
//! same CAS; R6 listing by workspace returns only that workspace's aggregates.
//! Restart durability and cross-connection races stay with each backend, whose
//! physical medium decides them.

use awaken_resource_contract::{
    ClonePolicy, ConfigVersion, InsertOutcome, MemoryStoreAggregate, MemoryStoreConfigVersion,
    MemoryStoreDefinition, ReplaceOutcome, RepositoryAggregate, RepositoryConfigVersion,
    RepositoryDefinition, ResourceRegistryRepository, ResourceState, RetentionPolicy,
};

#[must_use]
pub fn memory_store(id: &str, workspace_id: &str) -> MemoryStoreAggregate {
    MemoryStoreAggregate::register(
        MemoryStoreDefinition {
            id: id.into(),
            workspace_id: workspace_id.into(),
            name: "Memory".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        MemoryStoreConfigVersion {
            memory_store_id: id.into(),
            version: ConfigVersion::INITIAL,
            retention_policy: RetentionPolicy::default(),
        },
    )
    .expect("valid MemoryStore aggregate")
}

#[must_use]
pub fn repository(id: &str, workspace_id: &str) -> RepositoryAggregate {
    RepositoryAggregate::register(
        RepositoryDefinition {
            id: id.into(),
            workspace_id: workspace_id.into(),
            name: "Repository".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        },
        RepositoryConfigVersion {
            repository_id: id.into(),
            version: ConfigVersion::INITIAL,
            remote_url: "https://example.invalid/one.git".into(),
            credential_binding: Some("vault://repo-token".into()),
            initial_branch: Some("main".into()),
            initial_commit: None,
            clone_policy: ClonePolicy { depth: Some(1) },
        },
    )
    .expect("valid Repository aggregate")
}

pub async fn memory_store_cas_laws<S: ResourceRegistryRepository>(store: &S) {
    let aggregate = memory_store("memory-1", "ws");
    assert_eq!(
        store
            .insert_memory_store(&aggregate)
            .await
            .expect("R1 insert"),
        InsertOutcome::Inserted,
        "R1 first insert"
    );
    let stored = store
        .load_memory_store("memory-1")
        .await
        .expect("R1 load")
        .expect("R1 aggregate present");
    assert_eq!(
        stored.aggregate, aggregate,
        "R1 load returns the registered aggregate"
    );

    let mut updated = stored.aggregate.clone();
    let mut second = updated
        .config(ConfigVersion::INITIAL)
        .expect("R2 initial config")
        .clone();
    second.version = ConfigVersion(2);
    second.retention_policy.retention_days = Some(20);
    updated
        .publish_config("ws", ConfigVersion::INITIAL, second, 2)
        .expect("R2 domain transition");
    let next = stored.revision.checked_next().expect("R2 next revision");
    assert_eq!(
        store
            .replace_memory_store(stored.revision, &updated)
            .await
            .expect("R2 replace"),
        ReplaceOutcome::Replaced { revision: next },
        "R2 replace at the loaded revision advances exactly one revision"
    );

    assert_eq!(
        store
            .replace_memory_store(stored.revision, &aggregate)
            .await
            .expect("R3 stale replace is classified"),
        ReplaceOutcome::ConcurrentModification,
        "R3 stale revision"
    );
    let after = store
        .load_memory_store("memory-1")
        .await
        .expect("R3 load")
        .expect("R3 aggregate present");
    assert_eq!(after.revision, next, "R3 stale replace leaves the revision");
    assert_eq!(
        after.aggregate, updated,
        "R3 stale replace leaves the aggregate"
    );

    assert_ne!(
        store
            .insert_memory_store(&aggregate)
            .await
            .expect("R4 duplicate insert is classified"),
        InsertOutcome::Inserted,
        "R4 second insert of one id is not a second row"
    );
}

pub async fn repository_cas_laws<S: ResourceRegistryRepository>(store: &S) {
    let aggregate = repository("repository-1", "ws");
    assert_eq!(
        store
            .insert_repository(&aggregate)
            .await
            .expect("R5 insert"),
        InsertOutcome::Inserted
    );
    let stored = store
        .load_repository("repository-1")
        .await
        .expect("R5 load")
        .expect("R5 aggregate present");
    assert_eq!(
        stored.aggregate, aggregate,
        "R5 load returns the registered aggregate"
    );
    let mut updated = stored.aggregate.clone();
    let mut second = updated
        .config(ConfigVersion::INITIAL)
        .expect("R5 initial config")
        .clone();
    second.version = ConfigVersion(2);
    second.remote_url = "https://example.invalid/two.git".into();
    updated
        .publish_config("ws", ConfigVersion::INITIAL, second, 2)
        .expect("R5 domain transition");
    let next = stored.revision.checked_next().expect("R5 next revision");
    assert_eq!(
        store
            .replace_repository(stored.revision, &updated)
            .await
            .expect("R5 replace"),
        ReplaceOutcome::Replaced { revision: next }
    );
    assert_eq!(
        store
            .replace_repository(stored.revision, &aggregate)
            .await
            .expect("R5 stale replace is classified"),
        ReplaceOutcome::ConcurrentModification
    );
}

pub async fn listing_is_partitioned_by_workspace<S: ResourceRegistryRepository>(store: &S) {
    for (id, workspace) in [("m-a1", "ws-a"), ("m-a2", "ws-a"), ("m-b1", "ws-b")] {
        assert_eq!(
            store
                .insert_memory_store(&memory_store(id, workspace))
                .await
                .expect("R6 insert"),
            InsertOutcome::Inserted
        );
    }
    for (id, workspace) in [("r-a1", "ws-a"), ("r-b1", "ws-b")] {
        assert_eq!(
            store
                .insert_repository(&repository(id, workspace))
                .await
                .expect("R6 insert repository"),
            InsertOutcome::Inserted
        );
    }
    let mut a: Vec<String> = store
        .list_memory_stores("ws-a")
        .await
        .expect("R6 list")
        .into_iter()
        .map(|stored| stored.aggregate.definition().id.as_str().to_owned())
        .collect();
    a.sort();
    assert_eq!(a, ["m-a1", "m-a2"], "R6 only ws-a memory stores");
    let b: Vec<String> = store
        .list_repositories("ws-b")
        .await
        .expect("R6 list repositories")
        .into_iter()
        .map(|stored| stored.aggregate.definition().id.as_str().to_owned())
        .collect();
    assert_eq!(b, ["r-b1"], "R6 only ws-b repositories");
    assert!(
        store
            .list_memory_stores("ws-none")
            .await
            .expect("R6 empty list")
            .is_empty(),
        "R6 unknown workspace lists nothing"
    );
}

/// Run every law on fresh stores from `fresh`.
pub async fn run_suite<S: ResourceRegistryRepository, F: std::future::Future<Output = S>>(
    fresh: impl Fn() -> F,
) {
    memory_store_cas_laws(&fresh().await).await;
    repository_cas_laws(&fresh().await).await;
    listing_is_partitioned_by_workspace(&fresh().await).await;
}
