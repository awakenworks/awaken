//! Failure injection at the original purge port; real selected content verifies effects.
use super::*;
use awaken_resource_contract::{MemErr, MemoryPurgeSummary, MemoryVersion};
use std::sync::atomic::{AtomicBool, Ordering};

struct FailOncePurge {
    original: Arc<dyn MemoryContentPurge>,
    store: String,
    failed: AtomicBool,
}

#[async_trait::async_trait]
impl MemoryContentPurge for FailOncePurge {
    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        if store == self.store && !self.failed.swap(true, Ordering::SeqCst) {
            return Err(MemErr::Storage("private reset blocked".into()));
        }
        self.original.purge_store(store).await
    }
}

struct RefuseSourceSnapshot;
#[async_trait::async_trait]
impl MemorySnapshotSource for RefuseSourceSnapshot {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        assert_eq!(store, "dream-source");
        Err(MemErr::Storage("source boundary probe".into()))
    }
}

async fn content(
    authorities: &awaken_resource_application::ResourceAuthorities,
    store: &str,
) -> (Vec<Memory>, Vec<MemoryVersion>) {
    (
        authorities
            .memory_snapshot_source()
            .snapshot_heads(store)
            .await
            .unwrap(),
        authorities
            .memory_versions()
            .list_versions(store)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn preparation_requires_private_cleanup_and_preserves_existing_output_on_retry() {
    // P1 snapshot reset fails: preserve both orphan stores and admit nothing.
    // P2 result reset fails: retain the original result, with the snapshot cleared.
    // P3 UpdateExisting: never purge the caller's existing output.
    // Each retry removes partial private data, then deliberately refuses source
    // input before cloning/admission. These rows call production prepare, so
    // swallowing the purge error would reach the probe and fail the first assertion.
    // The existing Managed Dream E2E owns successful source/Session execution.
    for (row, fail_snapshot, update_existing) in [
        ("P1", true, false),
        ("P2", false, false),
        ("P3", true, true),
    ] {
        let resources = awaken_resource_persistence::ephemeral(
            awaken_time::system_clock(),
            Arc::new(awaken_sandbox_memoryd::LocalInvalidator::default()),
        )
        .await
        .unwrap();
        let authorities = resources.authorities();
        let dependencies = awaken_session_executor_testkit::volatile_dependencies(
            Arc::new(awaken_scenario_host::EchoModel),
            "stub",
        )
        .await;
        let assembled =
            crate::local_managed_state(dependencies, authorities.resource_registry()).await;
        let snapshot = format!("mem_snapshot_reset-{row}");
        let result = format!("mem_result_reset-{row}");
        for (store, name) in std::iter::once(("dream-source", "caller source"))
            .chain(update_existing.then_some((result.as_str(), "caller output")))
        {
            resources
                .memory_stores()
                .create(CreateMemoryStoreCommand {
                    workspace_id: "default".into(),
                    id: Some(store.into()),
                    name: name.into(),
                    description: String::new(),
                    metadata: BTreeMap::new(),
                    initial_state: ResourceState::Active,
                    retention_policy: Default::default(),
                })
                .await
                .unwrap();
        }
        for (store, bytes) in [
            (snapshot.as_str(), "partial snapshot"),
            (result.as_str(), "partial result"),
            ("dream-source", "source sentinel"),
        ] {
            authorities
                .memory_writer()
                .create(store, "/old.md", bytes)
                .await
                .unwrap();
        }
        let before_snapshot = content(&authorities, &snapshot).await;
        let before_result = content(&authorities, &result).await;
        let before_source = content(&authorities, "dream-source").await;
        let mut agent = BuiltInDreamAgent::new(
            assembled.sessions.clone(),
            &authorities,
            authorities.resource_registry(),
            resources.memory_stores(),
            resources.files(),
        );
        agent.content_purge = Arc::new(FailOncePurge {
            original: authorities.memory_content_purge(),
            store: if fail_snapshot {
                snapshot.clone()
            } else {
                result.clone()
            },
            failed: AtomicBool::new(false),
        });
        agent.snapshots = Arc::new(RefuseSourceSnapshot);
        let request = DreamRequest {
            job_id: format!("reset-{row}"),
            workspace_id: "default".into(),
            source_memory_store_id: "dream-source".into(),
            output_behavior: if update_existing {
                DreamOutputBehavior::UpdateExisting {
                    memory_store_id: result.clone(),
                }
            } else {
                DreamOutputBehavior::CreateNew
            },
            session_ids: Vec::new(),
            model: awaken_session_dream_contract::DreamModelConfig {
                id: "stub".into(),
                speed: None,
            },
            request_guidance: None,
            agent_id: "dream".into(),
        };
        let error = agent.prepare(&request).await.unwrap_err();
        assert_eq!(
            error.kind, "internal_error",
            "{row}: prepare must stop at failed reset"
        );
        assert!(
            error.message.contains("private reset blocked"),
            "{row}: {}",
            error.message
        );
        assert_eq!(
            content(&authorities, &snapshot).await,
            if fail_snapshot {
                before_snapshot
            } else {
                (Vec::new(), Vec::new())
            },
            "{row}"
        );
        assert_eq!(content(&authorities, &result).await, before_result, "{row}");
        assert!(
            resources
                .memory_stores()
                .get("default", &snapshot)
                .await
                .unwrap()
                .is_none(),
            "{row}: no new snapshot catalog entry"
        );
        assert_eq!(
            resources
                .memory_stores()
                .get("default", &result)
                .await
                .unwrap()
                .is_some(),
            update_existing,
            "{row}"
        );
        assert!(
            assembled
                .sessions
                .queries()
                .session_transcript("default", &format!("sesn_dream_{}", request.job_id))
                .await
                .is_err(),
            "{row}: no auxiliary Session admitted"
        );
        let retry = agent.prepare(&request).await.unwrap_err();
        assert_eq!(retry.kind, "input_memory_store_unavailable", "{row}");
        assert!(retry.message.contains("source boundary probe"), "{row}");
        assert_eq!(
            content(&authorities, &snapshot).await,
            (Vec::new(), Vec::new()),
            "{row}: retry cleared snapshot heads/history"
        );
        assert_eq!(
            content(&authorities, &result).await,
            if update_existing {
                before_result
            } else {
                (Vec::new(), Vec::new())
            },
            "{row}: caller target survives"
        );
        assert_eq!(
            content(&authorities, "dream-source").await,
            before_source,
            "{row}: caller source unchanged"
        );
    }
}
