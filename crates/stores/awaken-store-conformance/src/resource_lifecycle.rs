//! Shared Resource purge and reverse-reference lifecycle laws for every durable backend.
//! Adapter tests add restart and physical concurrency checks for their own medium.

use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourcePurgeStatus, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};

pub fn intent(id: &str) -> ResourcePurgeIntent {
    ResourcePurgeIntent::new(
        id,
        format!("delete:{id}"),
        ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-1"),
        Some(3),
        10,
        20,
    )
    .unwrap()
}

pub fn reference(workspace: &str, holder: &str) -> ResourceReferenceRecord {
    ResourceReferenceRecord {
        target: ResourceTarget::new(workspace, ResourceKind::File, "hash-1"),
        reference: ResourceReference {
            kind: ResourceReferenceKind::WorkspaceOwnership,
            reference_id: holder.into(),
        },
    }
}

/// Exercise the same immutable intent, CAS, lease, reference and reclamation
/// rules for SQLite and PostgreSQL.
pub async fn run_suite(store: &(impl ResourcePurgeRepository + ResourceReferenceIndex)) {
    // Causes: C1 intent frame valid/incomplete; C2 insert new/replay; C3 CAS
    // exact/stale; C4 recovery clock before/at due or lease expiry; C5 reverse
    // reference new/duplicate/removed; C6 reclamation has blockers, no fence,
    // same owner or foreign owner; C7 replacement repeats/clears one holder's
    // target. Effects: E1 invalid frame leaves no row, E2 one immutable intent,
    // E3 exact mutation or conflict without partial write, E4 due-only scan,
    // E5 idempotent reference set, E6 blocked/acquired/idempotent/contended
    // fence and rejected late reference, E7 deduplicated atomic replacement.
    // Decision rules: R1=!C1->E1; R2=C1+C2->E2; R3=C3->E3;
    // R4=C4->E4; R5=C5->E5; R6=C6->E6; R7=C7->E7.
    // V1 a schema-valid but incomplete claim/receipt frame is rejected
    // before insert on every backend; V2 the same rule precedes root CAS.
    for status in [ResourcePurgeStatus::Claimed, ResourcePurgeStatus::Completed] {
        let mut invalid = intent("invalid-frame");
        invalid.status = status;
        assert!(
            matches!(
                store.put(invalid).await,
                Err(ResourcePurgeError::InvalidIntent { .. })
            ),
            "V1"
        );
        assert!(
            store.get("invalid-frame").await.unwrap().is_none(),
            "V1 no row"
        );
    }
    assert_eq!(
        store.put(intent("purge-1")).await.unwrap(),
        PutResourcePurgeOutcome::Inserted
    );
    assert_eq!(
        store.put(intent("purge-1")).await.unwrap(),
        PutResourcePurgeOutcome::Existing
    );
    let before = store.get("purge-1").await.unwrap().unwrap();
    let mut invalid = before.clone();
    invalid.status = ResourcePurgeStatus::Completed;
    invalid.revision += 1;
    assert!(
        matches!(
            store.save(before.revision, invalid).await,
            Err(ResourcePurgeError::InvalidIntent { .. })
        ),
        "V2"
    );
    assert_eq!(
        store.get("purge-1").await.unwrap().unwrap(),
        before,
        "V2 no partial CAS"
    );
    assert!(store.recoverable(19, 10).await.unwrap().is_empty());
    let mut value = store.recoverable(20, 10).await.unwrap().remove(0);
    let revision = value.revision;
    value.claim("worker", 20, 10).unwrap();
    store.save(revision, value.clone()).await.unwrap();
    assert!(store.recoverable(29, 10).await.unwrap().is_empty());
    assert_eq!(store.recoverable(30, 10).await.unwrap().len(), 1);
    assert!(matches!(
        store.save(revision, value).await,
        Err(ResourcePurgeError::RevisionConflict(_))
    ));

    let a = reference("workspace-a", "ownership-a");
    let b = reference("workspace-b", "ownership-b");
    assert!(store.add_reference(a.clone()).await.unwrap());
    assert!(!store.add_reference(a.clone()).await.unwrap());
    assert!(store.add_reference(b.clone()).await.unwrap());
    assert_eq!(
        store.references(&a.target).await.unwrap(),
        vec![a.reference.clone()]
    );
    assert_eq!(
        store
            .references_for_resource(ResourceKind::File, "hash-1")
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(store.remove_reference(&b).await.unwrap());
    assert!(!store.remove_reference(&b).await.unwrap());
    assert_eq!(
        store
            .references_for_resource(ResourceKind::File, "hash-1")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        store.acquire_reclamation("intent-a", &a.target).await.unwrap(),
        AcquireResourceReclamationOutcome::Blocked(rows) if rows == vec![a.clone()]
    ));
    assert!(store.remove_reference(&a).await.unwrap());
    assert_eq!(
        store
            .acquire_reclamation("intent-a", &a.target)
            .await
            .unwrap(),
        AcquireResourceReclamationOutcome::Acquired
    );
    assert_eq!(
        store
            .acquire_reclamation("intent-a", &a.target)
            .await
            .unwrap(),
        AcquireResourceReclamationOutcome::AlreadyOwned
    );
    assert_eq!(
        store
            .acquire_reclamation("intent-b", &b.target)
            .await
            .unwrap(),
        AcquireResourceReclamationOutcome::Contended
    );
    assert!(matches!(
        store.add_reference(b.clone()).await,
        Err(ResourcePurgeError::ReclamationFenced {
            kind: ResourceKind::File,
            resource_id
        }) if resource_id == "hash-1"
    ));
    assert_eq!(
        store.release_reclamation("intent-b", &b.target).await,
        Err(ResourcePurgeError::StaleReclamationFence)
    );
    assert!(
        store
            .release_reclamation("intent-a", &a.target)
            .await
            .unwrap()
    );
    assert!(
        !store
            .release_reclamation("intent-a", &a.target)
            .await
            .unwrap()
    );
    assert!(store.add_reference(b.clone()).await.unwrap());
    let replacement = ResourceReferenceRecord {
        target: ResourceTarget::new("workspace-c", ResourceKind::File, "hash-2"),
        reference: ResourceReference {
            kind: ResourceReferenceKind::SessionBinding,
            reference_id: "session-1".into(),
        },
    };
    /* Replacement set decision. C1 one holder repeats an identical target
     * coordinate (for example, the same content through two mount paths).
     * E1 persist one safety edge without rejecting the valid replacement.
     * Rule D1 C1=>E1; adapter-specific SQL must not create another policy. */
    store
        .replace_references(
            ResourceReferenceKind::SessionBinding,
            "session-1",
            vec![replacement.clone(), replacement.clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        store.references(&replacement.target).await.unwrap().len(),
        1
    );
    store
        .replace_references(
            ResourceReferenceKind::SessionBinding,
            "session-1",
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(
        store
            .references(&replacement.target)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.get("purge-1").await.unwrap().unwrap().status,
        ResourcePurgeStatus::Claimed
    );
}
