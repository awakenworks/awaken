//! T23-C02 oracle 2: the Dream source snapshot is read only behind the source
//! store's authorization boundary.
use super::*;
use awaken_resource_contract::MemErr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts every source snapshot read while delegating to the real selected source.
struct CountingSnapshots {
    original: Arc<dyn MemorySnapshotSource>,
    reads: AtomicUsize,
}

#[async_trait::async_trait]
impl MemorySnapshotSource for CountingSnapshots {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.original.snapshot_heads(store).await
    }
}

struct Fixture {
    _state: crate::test_session_fixture::SessionReadFixture,
    resources: awaken_resource_application::ResourcesApplication,
    agent: BuiltInDreamAgent,
    counter: Arc<CountingSnapshots>,
}

async fn fixture() -> Fixture {
    let resources = awaken_resource_persistence::ephemeral(
        awaken_time::system_clock(),
        Arc::new(awaken_resource_contract::LocalInvalidator::default()),
    )
    .await
    .unwrap();
    let authorities = resources.authorities();
    let assembled = crate::test_session_fixture::session_read_fixture(
        authorities.resource_registry(),
        awaken_time::system_clock(),
    )
    .await;
    resources
        .memory_stores()
        .create(CreateMemoryStoreCommand {
            workspace_id: "default".into(),
            id: Some("dream-source".into()),
            name: "caller source".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            initial_state: ResourceState::Active,
            retention_policy: Default::default(),
        })
        .await
        .unwrap();
    authorities
        .memory_writer()
        .create("dream-source", "/secret.md", "source sentinel")
        .await
        .unwrap();
    let mut agent = BuiltInDreamAgent::new(
        assembled.sessions.clone(),
        &authorities,
        authorities.resource_registry(),
        resources.memory_stores(),
        resources.files(),
    );
    let counter = Arc::new(CountingSnapshots {
        original: authorities.memory_snapshot_source(),
        reads: AtomicUsize::new(0),
    });
    agent.snapshots = counter.clone();
    Fixture {
        _state: assembled,
        resources,
        agent,
        counter,
    }
}

fn request(job: &str, source: &str) -> DreamRequest {
    DreamRequest {
        job_id: job.into(),
        workspace_id: "default".into(),
        source_memory_store_id: source.into(),
        output_behavior: DreamOutputBehavior::CreateNew,
        session_ids: Vec::new(),
        model: awaken_session_dream_contract::DreamModelConfig {
            id: "stub".into(),
            speed: None,
        },
        request_guidance: None,
        agent_id: awaken_dream_application::BUILT_IN_DREAM_AGENT_ID.into(),
    }
}

/// Negative-control mutant: read the source before resolving its authority.
async fn read_then_authorize(agent: &BuiltInDreamAgent, request: &DreamRequest) -> bool {
    let _ = agent
        .snapshots
        .snapshot_heads(&request.source_memory_store_id)
        .await;
    agent.validate_inputs(request).await.is_ok()
}

/// A1 an unresolvable source is refused with the stable input code and zero
/// snapshot reads; A2 the resolvable source is admitted, still with zero reads
/// (authorization never needs content). The read-then-authorize mutant reads
/// the unauthorized source once, which the zero-read oracle detects.
#[tokio::test]
async fn an_unauthorized_dream_source_is_refused_before_any_snapshot_read() {
    let fixture = fixture().await;
    let refused = fixture
        .agent
        .validate_inputs(&request("auth-a1", "foreign-source"))
        .await
        .unwrap_err();
    assert_eq!(refused.kind, "input_memory_store_unavailable", "A1");
    assert_eq!(fixture.counter.reads.load(Ordering::SeqCst), 0, "A1");

    fixture
        .agent
        .validate_inputs(&request("auth-a2", "dream-source"))
        .await
        .expect("A2");
    assert_eq!(fixture.counter.reads.load(Ordering::SeqCst), 0, "A2");

    assert!(!read_then_authorize(&fixture.agent, &request("auth-nc", "foreign-source")).await);
    assert_eq!(
        fixture.counter.reads.load(Ordering::SeqCst),
        1,
        "NC the zero-read oracle detects a read that precedes authorization"
    );
}

/// The source is authorized by `validate_inputs`, then deleted before
/// `prepare`. `prepare` must refuse before reading the deleted source.
#[tokio::test]
#[ignore = "product gap: Dream prepare reads the source snapshot without re-resolving its authority (dream.rs:436)"]
async fn prepare_re_resolves_the_source_before_its_snapshot_read() {
    let fixture = fixture().await;
    let request = request("auth-toctou", "dream-source");
    fixture.agent.validate_inputs(&request).await.unwrap();
    fixture
        .resources
        .memory_stores()
        .delete("default", "dream-source", 1)
        .await
        .unwrap();

    let result = fixture.agent.prepare(&request).await;
    assert_eq!(
        fixture.counter.reads.load(Ordering::SeqCst),
        0,
        "a deleted source is never read"
    );
    assert!(
        matches!(&result, Err(failure) if failure.kind == "input_memory_store_unavailable"),
        "prepare refuses the deleted source"
    );
}
